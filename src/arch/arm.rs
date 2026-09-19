//! 32-bit Arm (A32) and Thumb (T32) instruction encoding.
//!
//! Everything here works on little-endian instructions and knows nothing
//! about relocation types: the ELF backend maps a relocation to a
//! [`Field`], computes the value the ABI prescribes, and asks this module to
//! pack it into the instruction, or to read the addend a `SHT_REL`
//! relocation keeps there ([`Field::decode`]).
//!
//! An A32 instruction is one 32-bit word. A 32-bit Thumb instruction is two
//! 16-bit halfwords, the first at the lower address; this module handles it
//! as one `u32` holding the first halfword in its upper half
//! ([`read_thumb32`], [`write_thumb32`]).
//!
//! Branches that change instruction set go through `blx`: the A32 form
//! takes bit 1 of the offset in its H bit, the Thumb form branches
//! relative to the word-aligned PC. Range-extension and interworking
//! thunks ([`write_thunk`]) load the destination into `ip` and `bx` to it,
//! so bit 0 of the destination selects the state.

#![deny(clippy::arithmetic_side_effects)]

use super::Overflow;

/// A32 `nop` (the ARMv6K hint), with the condition in the top four bits.
pub const NOP: u32 = 0xe320_f000;
/// Thumb-2 `nop.w`.
pub const THUMB_NOP_W: u32 = 0xf3af_8000;
/// Thumb `bx ip`.
pub const THUMB_BX_IP: u16 = 0x4760;
/// Thumb `add ip, pc`.
pub const THUMB_ADD_IP_PC: u16 = 0x44fc;
/// A32 `bx ip`.
pub const BX_IP: u32 = 0xe12f_ff1c;
/// A32 `add ip, ip, pc`.
pub const ADD_IP_IP_PC: u32 = 0xe08c_c00f;
/// `EXIDX_CANTUNWIND`: an exception index entry for code that cannot be
/// unwound.
pub const EXIDX_CANTUNWIND: u32 = 1;

/// Size of one range-extension or interworking thunk: the largest form
/// (position-independent A32) is four instructions.
pub const THUNK_SIZE: u64 = 16;

/// How far the PC reads ahead of an A32 instruction.
pub const ARM_PC_BIAS: u64 = 8;
/// How far the PC reads ahead of a Thumb instruction.
pub const THUMB_PC_BIAS: u64 = 4;

/// A relocatable field of one instruction (or data word).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Field {
    /// A32 `b`, `bl` and `blx`: a signed 24-bit word offset (±32 MiB).
    Branch24,
    /// Thumb-2 `bl`, `blx` and `b.w`: ±16 MiB.
    ThumbBranch24,
    /// Thumb-2 `b<cond>.w`: ±1 MiB.
    ThumbBranch19,
    /// Thumb `b`: ±2 KiB.
    ThumbBranch11,
    /// Thumb `b<cond>`: ±256 bytes.
    ThumbBranch8,
    /// A32 `movw`: the low 16 bits of the value.
    Movw,
    /// A32 `movt`: the high 16 bits of the value.
    Movt,
    /// Thumb-2 `movw`.
    ThumbMovw,
    /// Thumb-2 `movt`.
    ThumbMovt,
    /// A 31-bit PC-relative word whose top bit is kept (`.ARM.exidx`).
    Prel31,
    /// Thumb-2 `adr.w` (`addw`/`subw` from the PC): ±4095 from the
    /// word-aligned PC.
    ThumbAdr,
    /// Thumb-2 `ldr.w` from a literal: ±4095 from the word-aligned PC.
    ThumbLdrLiteral,
    /// A32 `ldr` from a literal (`LDR_PC_G0`): ±4095.
    LdrLiteral,
    /// A32 `add`/`sub` from the PC with a rotated 8-bit immediate
    /// (`ALU_PC_G0`), checked when `checked`.
    AluPc {
        /// Whether the value must be encodable exactly.
        checked: bool,
    },
}

impl Field {
    /// The size of the field's instruction in bytes.
    #[must_use]
    pub fn bytes(self) -> usize {
        match self {
            Self::ThumbBranch11 | Self::ThumbBranch8 => 2,
            _ => 4,
        }
    }

    /// Whether the instruction is a 32-bit Thumb one, two halfwords.
    #[must_use]
    pub fn is_thumb32(self) -> bool {
        matches!(
            self,
            Self::ThumbBranch24
                | Self::ThumbBranch19
                | Self::ThumbMovw
                | Self::ThumbMovt
                | Self::ThumbAdr
                | Self::ThumbLdrLiteral
        )
    }

    /// Whether the value is computed from the word-aligned place (the
    /// ABI's `Pa`) rather than the place itself.
    #[must_use]
    pub fn from_aligned_place(self) -> bool {
        matches!(self, Self::ThumbAdr | Self::ThumbLdrLiteral)
    }

    /// Reads the field's instruction at `at` in `data` (a 32-bit Thumb
    /// instruction as [`read_thumb32`] does).
    #[must_use]
    pub fn read(self, data: &[u8], at: usize) -> Option<u32> {
        match self.bytes() {
            2 => read16(data, at).map(u32::from),
            _ if self.is_thumb32() => read_thumb32(data, at),
            _ => read32(data, at),
        }
    }

    /// Writes `insn` back as [`Field::read`] read it.
    pub fn write(self, data: &mut [u8], at: usize, insn: u32) -> Option<()> {
        match self.bytes() {
            2 => write16(data, at, insn as u16),
            _ if self.is_thumb32() => write_thumb32(data, at, insn),
            _ => write32(data, at, insn),
        }
    }

    /// The addend a `SHT_REL` relocation keeps in instruction `insn`.
    #[must_use]
    pub fn decode(self, insn: u32) -> i64 {
        let (hi, lo) = (insn >> 16, insn & 0xffff);
        match self {
            Self::Branch24 => sign_extend((insn & 0x00ff_ffff) << 2, 26),
            Self::ThumbBranch24 => {
                let s = (hi >> 10) & 1;
                let i1 = !((lo >> 13) ^ s) & 1;
                let i2 = !((lo >> 11) ^ s) & 1;
                let value = (s << 24)
                    | (i1 << 23)
                    | (i2 << 22)
                    | ((hi & 0x3ff) << 12)
                    | ((lo & 0x7ff) << 1);
                sign_extend(value, 25)
            }
            Self::ThumbBranch19 => {
                let value = (((hi >> 10) & 1) << 20)
                    | (((lo >> 11) & 1) << 19)
                    | (((lo >> 13) & 1) << 18)
                    | ((hi & 0x3f) << 12)
                    | ((lo & 0x7ff) << 1);
                sign_extend(value, 21)
            }
            Self::ThumbBranch11 => sign_extend((insn & 0x7ff) << 1, 12),
            Self::ThumbBranch8 => sign_extend((insn & 0xff) << 1, 9),
            Self::Movw | Self::Movt => sign_extend(((insn >> 4) & 0xf000) | (insn & 0x0fff), 16),
            Self::ThumbMovw | Self::ThumbMovt => {
                let imm =
                    ((hi & 0xf) << 12) | ((hi & 0x400) << 1) | ((lo & 0x7000) >> 4) | (lo & 0xff);
                sign_extend(imm, 16)
            }
            Self::Prel31 => sign_extend(insn & 0x7fff_ffff, 31),
            Self::ThumbAdr => {
                let imm = i64::from(((hi & 0x400) << 1) | ((lo & 0x7000) >> 4) | (lo & 0xff));
                if hi & 0xf0 != 0 { imm.wrapping_neg() } else { imm }
            }
            Self::ThumbLdrLiteral => {
                let imm = i64::from(lo & 0xfff);
                if hi & 0x80 != 0 { imm } else { imm.wrapping_neg() }
            }
            Self::LdrLiteral => {
                let imm = i64::from(insn & 0xfff);
                if insn & 0x0080_0000 != 0 {
                    imm
                } else {
                    imm.wrapping_neg()
                }
            }
            Self::AluPc { .. } => {
                let rotate = ((insn >> 8) & 0xf).wrapping_mul(2);
                let imm = i64::from((insn & 0xff).rotate_right(rotate));
                if insn & 0x0040_0000 != 0 {
                    imm.wrapping_neg()
                } else {
                    imm
                }
            }
        }
    }

    /// Packs `value` into instruction `insn` (read as [`Field::read`]
    /// does).
    ///
    /// # Errors
    ///
    /// [`Overflow`] when the value does not fit.
    pub fn encode(self, insn: u32, value: i64) -> Result<u32, Overflow> {
        let v = value as u32;
        match self {
            Self::Branch24 => {
                check_signed(value, 26)?;
                Ok((insn & 0xff00_0000) | ((v >> 2) & 0x00ff_ffff))
            }
            Self::ThumbBranch24 => {
                check_signed(value, 25)?;
                let hi = 0xf000 | ((v >> 14) & 0x0400) | ((v >> 12) & 0x03ff);
                let lo = ((insn & 0xffff) & 0xd000)
                    | ((!(v >> 10) ^ (v >> 11)) & 0x2000)
                    | ((!(v >> 11) ^ (v >> 13)) & 0x0800)
                    | ((v >> 1) & 0x07ff);
                Ok((hi << 16) | lo)
            }
            Self::ThumbBranch19 => {
                check_signed(value, 21)?;
                let hi = ((insn >> 16) & 0xfbc0) | ((v >> 10) & 0x0400) | ((v >> 12) & 0x003f);
                let lo = 0x8000 | ((v >> 8) & 0x0800) | ((v >> 5) & 0x2000) | ((v >> 1) & 0x07ff);
                Ok((hi << 16) | lo)
            }
            Self::ThumbBranch11 => {
                check_signed(value, 12)?;
                Ok((insn & 0xf800) | ((v >> 1) & 0x07ff))
            }
            Self::ThumbBranch8 => {
                check_signed(value, 9)?;
                Ok((insn & 0xff00) | ((v >> 1) & 0x00ff))
            }
            Self::Movw => Ok(arm_mov(insn, v & 0xffff)),
            Self::Movt => Ok(arm_mov(insn, v >> 16)),
            Self::ThumbMovw => Ok(thumb_mov(insn, v & 0xffff)),
            Self::ThumbMovt => Ok(thumb_mov(insn, v >> 16)),
            Self::Prel31 => {
                check_signed(value, 31)?;
                Ok((insn & 0x8000_0000) | (v & 0x7fff_ffff))
            }
            Self::ThumbAdr => {
                let (imm, sub) = magnitude(value, 12)?;
                let hi =
                    ((insn >> 16) & 0xfb0f) | if sub { 0x00a0 } else { 0 } | ((imm & 0x800) >> 1);
                let lo = (insn & 0x8f00) | ((imm & 0x700) << 4) | (imm & 0xff);
                Ok((hi << 16) | lo)
            }
            Self::ThumbLdrLiteral => {
                let (imm, negative) = magnitude(value, 12)?;
                let hi = ((insn >> 16) & !0x0080) | if negative { 0 } else { 0x0080 };
                let lo = (insn & 0xf000) | imm;
                Ok((hi << 16) | lo)
            }
            Self::LdrLiteral => {
                let (imm, negative) = magnitude(value, 12)?;
                let up = if negative { 0 } else { 0x0080_0000 };
                Ok((insn & 0xff7f_f000) | up | imm)
            }
            Self::AluPc { checked } => {
                let negative = value < 0;
                let magnitude = value.unsigned_abs();
                let (imm, rotate) = match u32::try_from(magnitude).ok().and_then(modified_immediate)
                {
                    Some(encoded) => encoded,
                    None if checked => return Err(Overflow),
                    // `_NC`: the most significant eight bits that can be
                    // encoded, as the group relocations define G0.
                    None => lossy_immediate(magnitude as u32),
                };
                let opcode = if negative { 0x0040_0000 } else { 0x0080_0000 };
                Ok((insn & 0xff3f_f000) | opcode | (rotate << 8) | imm)
            }
        }
    }
}

/// `value` sign-extended from `bits` bits.
fn sign_extend(value: u32, bits: u32) -> i64 {
    let shift = 32u32.saturating_sub(bits);
    i64::from(((value << shift) as i32) >> shift)
}

fn check_signed(value: i64, bits: u32) -> Result<(), Overflow> {
    if super::aarch64::fits_signed(value, bits) {
        Ok(())
    } else {
        Err(Overflow)
    }
}

/// The magnitude of `value`, which must fit `bits` bits, and whether it is
/// negative.
fn magnitude(value: i64, bits: u32) -> Result<(u32, bool), Overflow> {
    let magnitude = value.unsigned_abs();
    if magnitude >> bits != 0 {
        return Err(Overflow);
    }
    Ok((magnitude as u32, value < 0))
}

fn arm_mov(insn: u32, imm: u32) -> u32 {
    (insn & !0x000f_0fff) | ((imm & 0xf000) << 4) | (imm & 0x0fff)
}

fn thumb_mov(insn: u32, imm: u32) -> u32 {
    let hi = ((insn >> 16) & !0x040f) | ((imm >> 1) & 0x0400) | ((imm >> 12) & 0x000f);
    let lo = (insn & !0x70ff & 0xffff) | ((imm << 4) & 0x7000) | (imm & 0x00ff);
    (hi << 16) | lo
}

/// `value` as an A32 modified immediate: eight bits and a rotation (in
/// units of two bits), if it is one.
#[must_use]
pub fn modified_immediate(value: u32) -> Option<(u32, u32)> {
    (0u32..16).find_map(|rotate| {
        let imm = value.rotate_left(rotate.wrapping_mul(2));
        (imm <= 0xff).then_some((imm, rotate))
    })
}

/// The most significant eight bits of `value`, from an even bit position,
/// as an A32 modified immediate.
fn lossy_immediate(value: u32) -> (u32, u32) {
    if value == 0 {
        return (0, 0);
    }
    let top = 31u32.saturating_sub(value.leading_zeros());
    // The lowest bit kept, rounded down to an even position.
    let low = top.saturating_sub(7) & !1;
    let imm = (value >> low) & 0xff;
    let rotate = (32u32.wrapping_sub(low) % 32) / 2;
    (imm, rotate)
}

/// Whether A32 instruction `insn` is `blx <label>` (the unconditional
/// encoding that switches to Thumb).
#[must_use]
pub fn is_arm_blx(insn: u32) -> bool {
    insn & 0xfe00_0000 == 0xfa00_0000
}

/// A32 `blx <label>` for an offset of `value` bytes from the PC.
///
/// # Errors
///
/// [`Overflow`] beyond ±32 MiB.
pub fn arm_blx(value: i64) -> Result<u32, Overflow> {
    check_signed(value, 26)?;
    let v = value as u32;
    Ok(0xfa00_0000 | ((v & 2) << 23) | ((v >> 2) & 0x00ff_ffff))
}

/// A32 `bl <label>` (unconditional) with the offset of `insn`.
#[must_use]
pub fn arm_bl(insn: u32) -> u32 {
    0xeb00_0000 | (insn & 0x00ff_ffff)
}

/// Whether 32-bit Thumb instruction `insn` is `blx <label>` rather than
/// `bl <label>` (bit 12 of its second halfword is clear).
#[must_use]
pub fn is_thumb_blx(insn: u32) -> bool {
    insn & 0x1000 == 0
}

/// `insn`, a Thumb `bl` or `blx`, turned into `blx` (`true`) or `bl`.
#[must_use]
pub fn thumb_set_blx(insn: u32, blx: bool) -> u32 {
    if blx { insn & !0x1000 } else { insn | 0x1000 }
}

/// Reads a 16-bit halfword.
#[must_use]
pub fn read16(data: &[u8], at: usize) -> Option<u16> {
    data.get(at..)
        .and_then(|rest| rest.first_chunk::<2>())
        .map(|half| u16::from_le_bytes(*half))
}

/// Writes a 16-bit halfword.
pub fn write16(data: &mut [u8], at: usize, half: u16) -> Option<()> {
    let slot = data
        .get_mut(at..)
        .and_then(|rest| rest.first_chunk_mut::<2>())?;
    *slot = half.to_le_bytes();
    Some(())
}

/// Reads a 32-bit word (an A32 instruction or data).
#[must_use]
pub fn read32(data: &[u8], at: usize) -> Option<u32> {
    data.get(at..)
        .and_then(|rest| rest.first_chunk::<4>())
        .map(|word| u32::from_le_bytes(*word))
}

/// Writes a 32-bit word.
pub fn write32(data: &mut [u8], at: usize, word: u32) -> Option<()> {
    let slot = data
        .get_mut(at..)
        .and_then(|rest| rest.first_chunk_mut::<4>())?;
    *slot = word.to_le_bytes();
    Some(())
}

/// Reads a 32-bit Thumb instruction: the first halfword in the upper half.
#[must_use]
pub fn read_thumb32(data: &[u8], at: usize) -> Option<u32> {
    let hi = read16(data, at)?;
    let lo = read16(data, at.checked_add(2)?)?;
    Some((u32::from(hi) << 16) | u32::from(lo))
}

/// Writes a 32-bit Thumb instruction as [`read_thumb32`] reads it.
pub fn write_thumb32(data: &mut [u8], at: usize, insn: u32) -> Option<()> {
    let second = at.checked_add(2)?;
    if data.len() < second.checked_add(2)? {
        return None;
    }
    write16(data, at, (insn >> 16) as u16)?;
    write16(data, second, insn as u16)
}

/// A32 `movw ip, #imm` of the low half of `value`.
#[must_use]
pub fn movw_ip(value: u32) -> u32 {
    arm_mov(0xe300_c000, value & 0xffff)
}

/// A32 `movt ip, #imm` of the high half of `value`.
#[must_use]
pub fn movt_ip(value: u32) -> u32 {
    arm_mov(0xe340_c000, value >> 16)
}

/// Thumb-2 `movw ip, #imm` of the low half of `value`.
#[must_use]
pub fn thumb_movw_ip(value: u32) -> u32 {
    thumb_mov(0xf240_0c00, value & 0xffff)
}

/// Thumb-2 `movt ip, #imm` of the high half of `value`.
#[must_use]
pub fn thumb_movt_ip(value: u32) -> u32 {
    thumb_mov(0xf2c0_0c00, value >> 16)
}

/// What a thunk's code needs to know.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThunkKind {
    /// Its callers are Thumb code, so the thunk is too.
    pub thumb: bool,
    /// The output is position-independent: the destination is computed
    /// from the PC rather than loaded as an absolute address.
    pub pic: bool,
}

/// Writes the thunk at `address`, which branches to `destination` (bit 0
/// set for a Thumb destination), into `out`, 16 bytes.
///
/// A32: `movw ip; movt ip; bx ip`, or with `pic`
/// `movw ip; movt ip; add ip, ip, pc; bx ip` from the PC of the `add`.
/// Thumb: the same with Thumb-2 `movw`/`movt` and 16-bit `add`/`bx`.
/// The rest of the 16 bytes is zero.
///
/// # Errors
///
/// [`Overflow`] when `out` is too small or the destination is beyond the
/// 32-bit address space.
pub fn write_thunk(
    out: &mut [u8],
    address: u64,
    destination: u64,
    kind: ThunkKind,
) -> Result<(), Overflow> {
    let destination = u32::try_from(destination).map_err(|_| Overflow)?;
    let address = u32::try_from(address).map_err(|_| Overflow)?;
    if out.len() < 16 {
        return Err(Overflow);
    }
    out.iter_mut().take(16).for_each(|b| *b = 0);
    let fail = |r: Option<()>| r.ok_or(Overflow);
    match (kind.thumb, kind.pic) {
        (false, false) => {
            fail(write32(out, 0, movw_ip(destination)))?;
            fail(write32(out, 4, movt_ip(destination)))?;
            fail(write32(out, 8, BX_IP))?;
        }
        (false, true) => {
            // The `add` at +8 reads the PC as +16.
            let value = destination.wrapping_sub(address.wrapping_add(16));
            fail(write32(out, 0, movw_ip(value)))?;
            fail(write32(out, 4, movt_ip(value)))?;
            fail(write32(out, 8, ADD_IP_IP_PC))?;
            fail(write32(out, 12, BX_IP))?;
        }
        (true, false) => {
            fail(write_thumb32(out, 0, thumb_movw_ip(destination)))?;
            fail(write_thumb32(out, 4, thumb_movt_ip(destination)))?;
            fail(write16(out, 8, THUMB_BX_IP))?;
        }
        (true, true) => {
            // The Thumb `add` at +8 reads the PC as +12.
            let value = destination.wrapping_sub(address.wrapping_add(12));
            fail(write_thumb32(out, 0, thumb_movw_ip(value)))?;
            fail(write_thumb32(out, 4, thumb_movt_ip(value)))?;
            fail(write16(out, 8, THUMB_ADD_IP_PC))?;
            fail(write16(out, 10, THUMB_BX_IP))?;
        }
    }
    Ok(())
}

/// The first PLT entry (GNU ld's `elf32_arm_plt0_entry`): pushes `lr` and
/// jumps to the resolver in `.got.plt[2]`, leaving `lr` pointing at it:
///
/// ```text
/// str lr, [sp, #-4]!
/// ldr lr, [pc, #4]
/// add lr, pc, lr
/// ldr pc, [lr, #8]!
/// .word .got.plt - (. + 16)
/// ```
#[must_use]
pub fn plt_header(plt: u64, got_plt: u64) -> [u32; 5] {
    let offset = got_plt.wrapping_sub(plt.wrapping_add(16)) as u32;
    [0xe52d_e004, 0xe59f_e004, 0xe08f_e00e, 0xe5be_f008, offset]
}

/// A PLT entry at `entry` jumping through the `.got.plt` slot at `slot`
/// (GNU ld's `elf32_arm_plt_entry_short`), leaving the slot's address in
/// `ip` for the resolver:
///
/// ```text
/// add ip, pc, #0xNN00000
/// add ip, ip, #0xNN000
/// ldr pc, [ip, #0xNNN]!
/// ```
///
/// # Errors
///
/// [`Overflow`] when the slot is not within 256 MiB after the entry.
pub fn plt_entry(entry: u64, slot: u64) -> Result<[u32; 3], Overflow> {
    let offset = slot.wrapping_sub(entry.wrapping_add(8));
    let offset = u32::try_from(offset).map_err(|_| Overflow)?;
    if offset >> 28 != 0 {
        return Err(Overflow);
    }
    Ok([
        0xe28f_c600 | ((offset >> 20) & 0xff),
        0xe28c_ca00 | ((offset >> 12) & 0xff),
        0xe5bc_f000 | (offset & 0xfff),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arm_branches_round_trip() {
        // bl . (addend -8)
        let bl = 0xebff_fffe;
        assert_eq!(Field::Branch24.decode(bl), -8);
        let bl = Field::Branch24.encode(bl, 0x20).unwrap();
        assert_eq!(bl, 0xeb00_0008);
        assert_eq!(Field::Branch24.decode(bl), 0x20);
        assert!(Field::Branch24.encode(bl, 1 << 25).is_err());
        assert_eq!(arm_blx(0x22).unwrap(), 0xfb00_0008);
        assert!(is_arm_blx(0xfa00_0000));
        assert_eq!(arm_bl(0xfa12_3456), 0xeb12_3456);
    }

    #[test]
    fn thumb_branches_round_trip() {
        // bl . (addend -4), as `llvm-mc` writes it
        let bl = 0xf7ff_fffe;
        assert_eq!(Field::ThumbBranch24.decode(bl), -4);
        for value in [-4i64, 0x7c, -0x44, 0x00ff_fffe, -0x0100_0000] {
            let insn = Field::ThumbBranch24.encode(bl, value).unwrap();
            assert_eq!(Field::ThumbBranch24.decode(insn), value, "{value:#x}");
            assert!(!is_thumb_blx(insn));
        }
        assert!(Field::ThumbBranch24.encode(bl, 0x0100_0000).is_err());
        // blx from lld's output: f000 e83e is +0x7c
        assert_eq!(Field::ThumbBranch24.decode(0xf000_e83e), 0x7c);
        assert!(is_thumb_blx(0xf000_e83e));
        for value in [-2i64, 0x40, -0x10_0000, 0xf_fffe] {
            let insn = Field::ThumbBranch19.encode(0xf000_8000, value).unwrap();
            assert_eq!(Field::ThumbBranch19.decode(insn), value, "{value:#x}");
        }
        let b = Field::ThumbBranch11.encode(0xe7fe, 0x40).unwrap();
        assert_eq!(Field::ThumbBranch11.decode(b), 0x40);
        let b = Field::ThumbBranch8.encode(0xd0fe, -0x20).unwrap();
        assert_eq!(Field::ThumbBranch8.decode(b), -0x20);
        assert!(Field::ThumbBranch8.encode(0xd0fe, 0x100).is_err());
    }

    #[test]
    fn moves_round_trip() {
        let movw = Field::Movw.encode(0xe300_0000, 0x1234_5678).unwrap();
        assert_eq!(movw, 0xe305_0678);
        assert_eq!(Field::Movw.decode(movw), 0x5678);
        let movt = Field::Movt.encode(0xe340_0000, 0x1234_5678).unwrap();
        assert_eq!(movt, 0xe341_0234);
        let movw = Field::ThumbMovw.encode(0xf240_0000, 0xabcd).unwrap();
        assert_eq!(Field::ThumbMovw.decode(movw), -0x5433);
        assert_eq!(thumb_movw_ip(0xabcd) & 0x0f00, 0x0c00);
        let movt = Field::ThumbMovt.encode(0xf2c0_0000, 0x7fff_0000).unwrap();
        assert_eq!(Field::ThumbMovt.decode(movt), 0x7fff);
    }

    #[test]
    fn pc_relative_data_and_literals() {
        let prel = Field::Prel31.encode(0x8000_0000, -8).unwrap();
        assert_eq!(prel, 0xffff_fff8);
        assert_eq!(Field::Prel31.decode(prel), -8);
        assert!(Field::Prel31.encode(0, 1 << 30).is_err());
        let adr = Field::ThumbAdr.encode(0xf20f_0000, -0x123).unwrap();
        assert_eq!(Field::ThumbAdr.decode(adr), -0x123);
        let adr = Field::ThumbAdr.encode(adr, 0x7ff).unwrap();
        assert_eq!(Field::ThumbAdr.decode(adr), 0x7ff);
        let ldr = Field::ThumbLdrLiteral.encode(0xf8df_0000, -4).unwrap();
        assert_eq!(Field::ThumbLdrLiteral.decode(ldr), -4);
        let ldr = Field::LdrLiteral.encode(0xe59f_0000, -0x10).unwrap();
        assert_eq!(Field::LdrLiteral.decode(ldr), -0x10);
        let add = Field::AluPc { checked: true }
            .encode(0xe28f_0000, 0x3f0)
            .unwrap();
        assert_eq!(Field::AluPc { checked: true }.decode(add), 0x3f0);
        assert!(
            Field::AluPc { checked: true }
                .encode(0xe28f_0000, 0x101)
                .is_err()
        );
        let sub = Field::AluPc { checked: false }
            .encode(0xe28f_0000, -0x8)
            .unwrap();
        assert_eq!(Field::AluPc { checked: false }.decode(sub), -0x8);
    }

    #[test]
    fn thunks_and_plt() {
        let mut out = [0u8; 16];
        write_thunk(
            &mut out,
            0x1000,
            0x1234_5679,
            ThunkKind {
                thumb: false,
                pic: false,
            },
        )
        .unwrap();
        assert_eq!(read32(&out, 0), Some(0xe305_c679));
        assert_eq!(read32(&out, 4), Some(0xe341_c234));
        assert_eq!(read32(&out, 8), Some(BX_IP));
        write_thunk(
            &mut out,
            0x1000,
            0x2000,
            ThunkKind {
                thumb: true,
                pic: true,
            },
        )
        .unwrap();
        // movw ip, #0xff4 (0x2000 - 0x100c)
        assert_eq!(read_thumb32(&out, 0), Some(thumb_movw_ip(0xff4)));
        assert_eq!(read16(&out, 8), Some(THUMB_ADD_IP_PC));
        assert_eq!(read16(&out, 10), Some(THUMB_BX_IP));
        assert_eq!(
            plt_header(0x102f0, 0x303c4),
            [
                0xe52d_e004,
                0xe59f_e004,
                0xe08f_e00e,
                0xe5be_f008,
                0x303c4 - 0x10300
            ]
        );
        // lld's entry at 0x10310 for slot 0x303d0: add ip, pc, #0, #12;
        // add ip, ip, #32, #20; ldr pc, [ip, #0xb8]!
        assert_eq!(
            plt_entry(0x10310, 0x303d0).unwrap(),
            [0xe28f_c600, 0xe28c_ca20, 0xe5bc_f0b8]
        );
        assert!(plt_entry(0x1000, 0x2000_1000).is_err());
    }
}
