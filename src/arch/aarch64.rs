//! AArch64 instruction encoding.
//!
//! Everything here works on 32-bit little-endian instruction words and knows
//! nothing about relocation types: the ELF backend maps a relocation to a
//! [`Field`], computes the value the ABI prescribes, and asks this module to
//! pack it into the instruction. The same encodings serve range-extension
//! thunks, PLT entries and the TLS relaxations, which rewrite whole
//! instructions rather than patching a field.
//!
//! Naming follows the "ELF for the Arm 64-bit Architecture" relocation
//! table: `Page(x)` is `x & ~0xfff`, `G(x)` a GOT entry, and a field marked
//! `_NC` is written without an overflow check.

#![deny(clippy::arithmetic_side_effects)]

use super::Overflow;

/// The size of a page for `adrp` and the `Page(x)` operator of the ABI.
pub const PAGE: u64 = 0x1000;

/// The instruction word size: every AArch64 instruction is four bytes.
pub const INSN_SIZE: u64 = 4;

/// The bits `Page(x)` clears.
const PAGE_MASK: i64 = (PAGE - 1) as i64;

/// `Page(address)`: the address with its low 12 bits cleared.
#[must_use]
pub const fn page(address: u64) -> u64 {
    address & !(PAGE - 1)
}

/// `nop`.
pub const NOP: u32 = 0xd503_201f;
/// `bti c`: the landing pad an indirect call may branch to.
pub const BTI_C: u32 = 0xd503_245f;
/// `bti jc`: a landing pad for both indirect calls and jumps.
pub const BTI_JC: u32 = 0xd503_24df;
/// `mrs <Xd>, tpidr_el0` with `Xd` = x0: reads the thread pointer.
pub const MRS_TPIDR_X0: u32 = 0xd53b_d040;
/// `autia1716`: authenticate x17 with x16 as the modifier.
pub const AUTIA1716: u32 = 0xd503_219f;
/// `br x17`.
pub const BR_X17: u32 = 0xd61f_0220;
/// `br x16`.
pub const BR_X16: u32 = 0xd61f_0200;

/// The reach of a `b`/`bl` instruction: ±128 MiB.
pub const BRANCH26_REACH: i64 = 1 << 27;
/// The reach of `adr`: ±1 MiB.
pub const ADR_REACH: i64 = 1 << 20;
/// The reach of `adrp`: ±4 GiB.
pub const ADRP_REACH: i64 = 1 << 32;

/// Whether a `b`/`bl` at `from` can reach `to` directly.
#[must_use]
pub fn branch_in_range(from: u64, to: u64) -> bool {
    let delta = (to as i64).wrapping_sub(from as i64);
    (-BRANCH26_REACH..BRANCH26_REACH).contains(&delta)
}

/// Whether `value` fits `bits` bits as a signed number.
#[must_use]
pub fn fits_signed(value: i64, bits: u32) -> bool {
    if bits >= 64 {
        return true;
    }
    let Some(limit) = 1i64.checked_shl(bits.wrapping_sub(1)) else {
        return true;
    };
    value >= limit.wrapping_neg() && value < limit
}

/// Whether `value` fits `bits` bits as an unsigned number.
#[must_use]
pub fn fits_unsigned(value: i64, bits: u32) -> bool {
    if bits >= 64 {
        return true;
    }
    let Some(limit) = 1i64.checked_shl(bits) else {
        return true;
    };
    value >= 0 && value < limit
}

/// Whether `value` fits `bits` bits either way, as the ABI's "checked" data
/// relocations accept (`readelf` shows GNU ld accepting both).
#[must_use]
pub fn fits_either(value: i64, bits: u32) -> bool {
    fits_signed(value, bits) || fits_unsigned(value, bits)
}

/// How the `movz`/`movk` half-word of a `MOVW` group is checked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MovwCheck {
    /// `_NC`: no check.
    None,
    /// The value must fit `shift + 16` bits, unsigned.
    Unsigned,
    /// The value must fit `shift + 16` bits, signed; a negative value turns
    /// `movz` into `movn` and is written inverted.
    Signed,
}

/// A relocatable field of one instruction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    /// 26-bit branch offset in instruction units (`b`, `bl`).
    Branch26,
    /// 19-bit offset in instruction units (`b.cond`, `cbz`, literal `ldr`).
    Branch19,
    /// 14-bit offset in instruction units (`tbz`, `tbnz`).
    Branch14,
    /// 21-bit byte offset (`adr`).
    Adr21,
    /// 21-bit page offset (`adrp`), checked against ±4 GiB.
    Adrp21,
    /// 21-bit page offset (`adrp`), unchecked.
    Adrp21Nc,
    /// 12-bit unsigned `add` immediate holding the low 12 bits of the value.
    Add12,
    /// 12-bit unsigned `add` immediate; the whole value must fit.
    Add12Checked,
    /// 12-bit `add` immediate holding bits 12..24 of the value (the
    /// instruction already carries `lsl #12`).
    AddHi12,
    /// Scaled 12-bit load/store offset holding the low 12 bits of the
    /// value; `scale` is the log2 of the access size.
    LdSt {
        /// Log2 of the access width in bytes (0 for `ldrb`, 3 for `ldr x`).
        scale: u8,
        /// Whether the value must fit 12 bits plus the scale.
        checked: bool,
    },
    /// A doubleword load/store offset whose value must fit 15 bits
    /// (`LD64_GOTPAGE_LO15`).
    LdSt15,
    /// The 16-bit immediate of `movz`/`movk` at `shift`.
    Movw {
        /// Left shift of the half-word: 0, 16, 32 or 48.
        shift: u8,
        /// How the value is checked.
        check: MovwCheck,
    },
    /// 32 bits of data (little-endian), checked as signed or unsigned.
    Data32,
    /// 32 bits of data, checked as signed.
    Data32Signed,
    /// 16 bits of data, checked as signed or unsigned.
    Data16,
}

/// Reads the instruction word at `at` in `data`.
#[must_use]
pub fn read_insn(data: &[u8], at: usize) -> Option<u32> {
    data.get(at..)
        .and_then(|rest| rest.first_chunk::<4>())
        .map(|word| u32::from_le_bytes(*word))
}

/// Writes instruction word `insn` at `at` in `data`.
pub fn write_insn(data: &mut [u8], at: usize, insn: u32) -> Option<()> {
    let slot = data
        .get_mut(at..)
        .and_then(|rest| rest.first_chunk_mut::<4>())?;
    *slot = insn.to_le_bytes();
    Some(())
}

fn insert(insn: u32, value: u32, lsb: u32, bits: u32) -> u32 {
    let mask = match 1u32.checked_shl(bits) {
        Some(bit) => bit.wrapping_sub(1),
        None => u32::MAX,
    };
    let shifted_mask = mask.checked_shl(lsb).unwrap_or(0);
    (insn & !shifted_mask) | ((value & mask).checked_shl(lsb).unwrap_or(0))
}

/// The register number an instruction's `Rd`/`Rt` field holds.
#[must_use]
pub const fn destination_register(insn: u32) -> u32 {
    insn & 0x1f
}

impl Field {
    /// The number of bytes the field occupies (four for every instruction
    /// field, two for a 16-bit data field).
    #[must_use]
    pub const fn bytes(self) -> usize {
        match self {
            Self::Data16 => 2,
            _ => 4,
        }
    }

    /// Whether the field is a data word rather than an instruction field.
    #[must_use]
    pub const fn is_data(self) -> bool {
        matches!(self, Self::Data32 | Self::Data32Signed | Self::Data16)
    }

    /// Packs `value` into instruction `insn`.
    ///
    /// For data fields the "instruction" is the little-endian word itself.
    ///
    /// # Errors
    ///
    /// [`Overflow`] when the value does not fit a checked field, or is not
    /// aligned as the field requires.
    #[allow(clippy::too_many_lines)]
    pub fn encode(self, insn: u32, value: i64) -> Result<u32, Overflow> {
        match self {
            Self::Branch26 => {
                if value & 3 != 0 || !fits_signed(value, 28) {
                    return Err(Overflow);
                }
                Ok(insert(insn, (value >> 2) as u32, 0, 26))
            }
            Self::Branch19 => {
                if value & 3 != 0 || !fits_signed(value, 21) {
                    return Err(Overflow);
                }
                Ok(insert(insn, (value >> 2) as u32, 5, 19))
            }
            Self::Branch14 => {
                if value & 3 != 0 || !fits_signed(value, 16) {
                    return Err(Overflow);
                }
                Ok(insert(insn, (value >> 2) as u32, 5, 14))
            }
            Self::Adr21 => {
                if !fits_signed(value, 21) {
                    return Err(Overflow);
                }
                let low = (value as u32) & 3;
                let high = (value >> 2) as u32;
                Ok(insert(insert(insn, high, 5, 19), low, 29, 2))
            }
            Self::Adrp21 | Self::Adrp21Nc => {
                if value & PAGE_MASK != 0 {
                    return Err(Overflow);
                }
                if self == Self::Adrp21 && !fits_signed(value, 33) {
                    return Err(Overflow);
                }
                let pages = value >> 12;
                let low = (pages as u32) & 3;
                let high = (pages >> 2) as u32;
                Ok(insert(insert(insn, high, 5, 19), low, 29, 2))
            }
            Self::Add12 => Ok(insert(insn, (value as u32) & 0xfff, 10, 12)),
            Self::Add12Checked => {
                if !fits_unsigned(value, 12) {
                    return Err(Overflow);
                }
                Ok(insert(insn, value as u32, 10, 12))
            }
            Self::AddHi12 => {
                if !fits_unsigned(value, 24) {
                    return Err(Overflow);
                }
                Ok(insert(insn, (value >> 12) as u32, 10, 12))
            }
            Self::LdSt { scale, checked } => {
                let scale = u32::from(scale).min(4);
                let alignment = 1i64.checked_shl(scale).unwrap_or(1).wrapping_sub(1);
                if value & alignment != 0 {
                    return Err(Overflow);
                }
                if checked && !fits_unsigned(value, scale.wrapping_add(12)) {
                    return Err(Overflow);
                }
                let low = (value as u64 & 0xfff) >> scale;
                Ok(insert(insn, low as u32, 10, 12))
            }
            Self::LdSt15 => {
                if value & 7 != 0 || !fits_unsigned(value, 15) {
                    return Err(Overflow);
                }
                Ok(insert(insn, (value >> 3) as u32, 10, 12))
            }
            Self::Movw { shift, check } => {
                let shift = u32::from(shift).min(48);
                let bits = shift.wrapping_add(16);
                let mut insn = insn;
                let mut value = value;
                match check {
                    MovwCheck::None => {}
                    MovwCheck::Unsigned => {
                        if !fits_unsigned(value, bits) {
                            return Err(Overflow);
                        }
                    }
                    MovwCheck::Signed => {
                        if !fits_signed(value, bits) {
                            return Err(Overflow);
                        }
                        if value < 0 {
                            // movz -> movn, which loads the inverted value.
                            insn &= !(1 << 30);
                            value = !value;
                        }
                    }
                }
                let half = (value as u64).checked_shr(shift).unwrap_or(0) & 0xffff;
                Ok(insert(insn, half as u32, 5, 16))
            }
            Self::Data32 => {
                if !fits_either(value, 32) {
                    return Err(Overflow);
                }
                Ok(value as u32)
            }
            Self::Data32Signed => {
                if !fits_signed(value, 32) {
                    return Err(Overflow);
                }
                Ok(value as u32)
            }
            Self::Data16 => {
                if !fits_either(value, 16) {
                    return Err(Overflow);
                }
                Ok((value as u32) & 0xffff)
            }
        }
    }
}

/// `movz <Xd>, #imm16, lsl #shift`.
#[must_use]
pub fn movz(rd: u32, imm16: u32, shift: u32) -> u32 {
    0xd280_0000 | (rd & 0x1f) | ((imm16 & 0xffff) << 5) | (((shift / 16) & 3) << 21)
}

/// `movk <Xd>, #imm16, lsl #shift`.
#[must_use]
pub fn movk(rd: u32, imm16: u32, shift: u32) -> u32 {
    0xf280_0000 | (rd & 0x1f) | ((imm16 & 0xffff) << 5) | (((shift / 16) & 3) << 21)
}

/// `adrp <Xd>, #0`, before the page offset is packed in.
#[must_use]
pub fn adrp(rd: u32) -> u32 {
    0x9000_0000 | (rd & 0x1f)
}

/// `add <Xd>, <Xn>, #0`, before the immediate is packed in.
#[must_use]
pub fn add_imm(rd: u32, rn: u32) -> u32 {
    0x9100_0000 | (rd & 0x1f) | ((rn & 0x1f) << 5)
}

/// `ldr <Xt>, [<Xn>, #0]`, before the offset is packed in.
#[must_use]
pub fn ldr_offset(rt: u32, rn: u32) -> u32 {
    0xf940_0000 | (rt & 0x1f) | ((rn & 0x1f) << 5)
}

/// `br <Xn>`.
#[must_use]
pub fn br(rn: u32) -> u32 {
    0xd61f_0000 | ((rn & 0x1f) << 5)
}

/// `adr <Xd>, #0`, before the offset is packed in.
#[must_use]
pub fn adr(rd: u32) -> u32 {
    0x1000_0000 | (rd & 0x1f)
}

/// `b #0`, before the offset is packed in.
pub const B: u32 = 0x1400_0000;

/// Whether `insn` is `adrp`.
#[must_use]
pub const fn is_adrp(insn: u32) -> bool {
    insn & 0x9f00_0000 == 0x9000_0000
}

/// Whether `insn` is a 64-bit `add <Xd>, <Xn>, #imm` without a shift.
#[must_use]
pub const fn is_add_imm64(insn: u32) -> bool {
    insn & 0xffc0_0000 == 0x9100_0000
}

/// Whether `insn` is a load or store with a scaled 12-bit unsigned offset
/// (`ldr <Xt>, [<Xn>, #imm]` and its relatives).
#[must_use]
pub const fn is_load_store_unsigned(insn: u32) -> bool {
    insn & 0x3b00_0000 == 0x3900_0000
}

/// The register number an instruction's `Rn` (base) field holds.
#[must_use]
pub const fn base_register(insn: u32) -> u32 {
    (insn >> 5) & 0x1f
}

/// The byte offset an `adrp` adds to the page of its own address.
#[must_use]
pub fn adrp_offset(insn: u32) -> i64 {
    let low = i64::from((insn >> 29) & 3);
    let high = i64::from((insn >> 5) & 0x7ffff);
    // Sign-extend the 21-bit page count, then scale it to bytes.
    let pages = ((high << 2) | low) << 43 >> 43;
    pages << 12
}

/// The unshifted 12-bit immediate of an `add`.
#[must_use]
pub const fn add_immediate(insn: u32) -> u64 {
    ((insn >> 10) & 0xfff) as u64
}

// ---- Cortex-A53 errata ----
//
// Both workarounds replace one instruction of an affected sequence with a
// branch to a patch that holds that instruction followed by a branch back,
// which breaks the sequence. Detection follows lld for 843419 (the
// sequence and its limits are the same in GNU ld and gold) and GNU ld for
// 835769, which lld does not implement.

/// The size of an erratum patch: the moved instruction and `b` back.
pub const ERRATUM_PATCH_SIZE: u64 = 8;

/// Whether `insn` is in the load/store encoding class.
const fn is_load_store_class(insn: u32) -> bool {
    insn & 0x0a00_0000 == 0x0800_0000
}

const fn is_st1_multiple_opcode(insn: u32) -> bool {
    matches!(insn & 0xf000, 0x2000 | 0x6000 | 0x7000 | 0xa000)
}

const fn is_st1_single_opcode(insn: u32) -> bool {
    matches!(insn & 0x0040_e000, 0 | 0x4000 | 0x8000)
}

const fn is_st1_multiple(insn: u32) -> bool {
    insn & 0xbfff_0000 == 0x0c00_0000 && is_st1_multiple_opcode(insn)
}

const fn is_st1_multiple_post(insn: u32) -> bool {
    insn & 0xbfe0_0000 == 0x0c80_0000 && is_st1_multiple_opcode(insn)
}

const fn is_st1_single(insn: u32) -> bool {
    insn & 0xbfff_0000 == 0x0d00_0000 && is_st1_single_opcode(insn)
}

const fn is_st1_single_post(insn: u32) -> bool {
    insn & 0xbfe0_0000 == 0x0d80_0000 && is_st1_single_opcode(insn)
}

const fn is_st1(insn: u32) -> bool {
    is_st1_multiple(insn)
        || is_st1_multiple_post(insn)
        || is_st1_single(insn)
        || is_st1_single_post(insn)
}

const fn is_load_store_exclusive(insn: u32) -> bool {
    insn & 0x3f00_0000 == 0x0800_0000
}

const fn is_load_exclusive(insn: u32) -> bool {
    insn & 0x3f40_0000 == 0x0840_0000
}

const fn is_load_literal(insn: u32) -> bool {
    insn & 0x3b00_0000 == 0x1800_0000
}

const fn is_stnp(insn: u32) -> bool {
    insn & 0x3bc0_0000 == 0x2800_0000
}

const fn is_stp_post(insn: u32) -> bool {
    insn & 0x3bc0_0000 == 0x2880_0000
}

const fn is_stp_offset(insn: u32) -> bool {
    insn & 0x3bc0_0000 == 0x2900_0000
}

const fn is_stp_pre(insn: u32) -> bool {
    insn & 0x3bc0_0000 == 0x2980_0000
}

const fn is_stp(insn: u32) -> bool {
    is_stp_post(insn) || is_stp_offset(insn) || is_stp_pre(insn)
}

const fn is_load_store_unscaled(insn: u32) -> bool {
    insn & 0x3b00_0c00 == 0x3800_0000
}

const fn is_load_store_post(insn: u32) -> bool {
    insn & 0x3b20_0c00 == 0x3800_0400
}

const fn is_load_store_unprivileged(insn: u32) -> bool {
    insn & 0x3b20_0c00 == 0x3800_0800
}

const fn is_load_store_pre(insn: u32) -> bool {
    insn & 0x3b20_0c00 == 0x3800_0c00
}

const fn is_load_store_register_offset(insn: u32) -> bool {
    insn & 0x3b20_0c00 == 0x3820_0800
}

const fn is_single_register_load_store(insn: u32) -> bool {
    is_load_store_unscaled(insn)
        || is_load_store_post(insn)
        || is_load_store_unprivileged(insn)
        || is_load_store_pre(insn)
        || is_load_store_register_offset(insn)
        || is_load_store_unsigned(insn)
}

/// Whether `insn` is an Armv8.0 load that is not a structure load.
const fn is_non_structure_load(insn: u32) -> bool {
    if is_load_exclusive(insn) || is_load_literal(insn) {
        return true;
    }
    if !is_single_register_load_store(insn) {
        return false;
    }
    let size = (insn >> 30) & 3;
    let vector = (insn >> 26) & 1;
    let opc = (insn >> 22) & 3;
    // opc 0 stores; opc 2 is a store for 128-bit vectors and a prefetch for
    // 64-bit integer registers.
    opc != 0 && !(size == 0 && vector == 1 && opc == 2) && !(size == 3 && vector == 0 && opc == 2)
}

const fn has_writeback(insn: u32) -> bool {
    is_load_store_pre(insn)
        || is_load_store_post(insn)
        || is_stp_pre(insn)
        || is_stp_post(insn)
        || is_st1_single_post(insn)
        || is_st1_multiple_post(insn)
}

const fn load_store_writes(insn: u32, register: u32) -> bool {
    (is_non_structure_load(insn) && destination_register(insn) == register)
        || (has_writeback(insn) && base_register(insn) == register)
}

/// Whether `insn` is a branch, which ends a straight-line sequence.
const fn is_branch(insn: u32) -> bool {
    insn & 0xfe00_0000 == 0xd600_0000
        || insn & 0xfe00_0000 == 0x5400_0000
        || insn & 0x7c00_0000 == 0x1400_0000
        || insn & 0x7c00_0000 == 0x3400_0000
}

/// Whether `adrp`, `second` and `last` form the Cortex-A53 erratum 843419
/// sequence: an `adrp` writing `xn`, a load or store that does not write
/// `xn`, and a load or store with an unsigned offset based on `xn`.
#[must_use]
pub const fn is_843419_sequence(adrp: u32, second: u32, last: u32) -> bool {
    if !is_adrp(adrp) {
        return false;
    }
    let register = destination_register(adrp);
    is_load_store_class(second)
        && (is_load_store_exclusive(second)
            || is_load_literal(second)
            || is_single_register_load_store(second)
            || is_stp(second)
            || is_stnp(second)
            || is_st1(second))
        && !load_store_writes(second, register)
        && is_load_store_unsigned(last)
        && base_register(last) == register
}

/// The offsets (from the start of `code`) of the instructions erratum
/// 843419 affects in `code[start..end]`, when `code` starts at `address`:
/// the last load or store of each sequence whose `adrp` is at a page
/// offset of `0xff8` or `0xffc`. This is lld's scan.
#[must_use]
pub fn scan_843419(code: &[u8], address: u64, start: u64, end: u64) -> Vec<u64> {
    let mut sites = Vec::new();
    let end = end.min(u64::try_from(code.len()).unwrap_or(u64::MAX));
    let word = |offset: u64| {
        usize::try_from(offset)
            .ok()
            .and_then(|o| read_insn(code, o))
    };
    let mut offset = start;
    while offset < end {
        // Advance to the next page offset of at least 0xff8.
        let page_offset = address.wrapping_add(offset) & 0xfff;
        if page_offset < 0xff8 {
            offset = offset.saturating_add(0xff8u64.wrapping_sub(page_offset));
        }
        let Some(left) = end.checked_sub(offset).filter(|&left| left >= 12) else {
            break;
        };
        let (Some(first), Some(second), Some(third)) = (
            word(offset),
            word(offset.saturating_add(4)),
            word(offset.saturating_add(8)),
        ) else {
            break;
        };
        if is_843419_sequence(first, second, third) {
            sites.push(offset.saturating_add(8));
        } else if left > 12
            && !is_branch(third)
            && let Some(fourth) = word(offset.saturating_add(12))
            && is_843419_sequence(first, second, fourth)
        {
            sites.push(offset.saturating_add(12));
        }
        offset = if address.wrapping_add(offset) & 0xfff == 0xff8 {
            offset.saturating_add(4)
        } else {
            offset.saturating_add(0xffc)
        };
    }
    sites
}

/// Whether `insn` is a 64-bit multiply-accumulate (`madd`, `msub`,
/// `smaddl`, `smsubl`, `umaddl`, `umsubl`), not a plain multiply.
#[must_use]
pub const fn is_multiply_accumulate(insn: u32) -> bool {
    let op31 = (insn >> 21) & 7;
    insn & 0xff00_0000 == 0x9b00_0000 && matches!(op31, 0 | 1 | 5) && (insn >> 10) & 0x1f != 0x1f
}

/// What a memory access instruction transfers, for erratum 835769.
struct MemoryAccess {
    rt: u32,
    rt2: u32,
    pair: bool,
    load: bool,
}

/// Decodes `insn` as a memory access, as GNU ld's `aarch64_mem_op_p` does.
const fn memory_access(insn: u32) -> Option<MemoryAccess> {
    if insn & 0x0a00_0000 != 0x0800_0000 {
        return None;
    }
    let rt = insn & 0x1f;
    let rt2_field = (insn >> 10) & 0x1f;
    let load_bit = (insn >> 22) & 1 == 1;
    if insn & 0x3f00_0000 == 0x0800_0000 {
        // Exclusive: a pair when bit 21 is set.
        let pair = (insn >> 21) & 1 == 1;
        let rt2 = if pair { rt2_field } else { rt };
        return Some(MemoryAccess {
            rt,
            rt2,
            pair,
            load: load_bit,
        });
    }
    let masked = insn & 0x3b80_0000;
    if matches!(
        masked,
        0x2800_0000 | 0x2880_0000 | 0x2900_0000 | 0x2980_0000
    ) {
        return Some(MemoryAccess {
            rt,
            rt2: rt2_field,
            pair: true,
            load: load_bit,
        });
    }
    let single = insn & 0x3b20_0c00;
    if insn & 0x3b00_0000 == 0x1800_0000
        || matches!(
            single,
            0x3800_0000 | 0x3800_0400 | 0x3800_0800 | 0x3800_0c00 | 0x3820_0800
        )
        || insn & 0x3b00_0000 == 0x3900_0000
    {
        let opc = (insn >> 22) & 3;
        let vector = (insn >> 26) & 1;
        let opc_v = opc | (vector << 2);
        return Some(MemoryAccess {
            rt,
            rt2: rt,
            pair: false,
            load: matches!(opc_v, 1 | 2 | 3 | 5 | 7),
        });
    }
    if insn & 0xbfbf_0000 == 0x0c00_0000 || insn & 0xbfa0_0000 == 0x0c80_0000 {
        // Advanced SIMD load/store multiple structures.
        let count = match (insn >> 12) & 0xf {
            0 | 2 => 3,
            4 | 6 => 2,
            7 => 0,
            8 | 10 => 1,
            _ => return None,
        };
        return Some(MemoryAccess {
            rt,
            rt2: rt.wrapping_add(count),
            pair: false,
            load: load_bit,
        });
    }
    if insn & 0xbf9f_0000 == 0x0d00_0000 || insn & 0xbf80_0000 == 0x0d80_0000 {
        // Advanced SIMD load/store single structure.
        let r = (insn >> 21) & 1;
        let count = match (insn >> 13) & 7 {
            0 | 2 | 4 | 6 => r,
            _ => {
                if r == 0 {
                    2
                } else {
                    3
                }
            }
        };
        return Some(MemoryAccess {
            rt,
            rt2: rt.wrapping_add(count),
            pair: false,
            load: load_bit,
        });
    }
    None
}

/// Whether `first` followed by `second` is the Cortex-A53 erratum 835769
/// sequence: a memory access, then a 64-bit multiply-accumulate that does
/// not depend on a register the access loaded. GNU ld's test.
#[must_use]
pub fn is_835769_sequence(first: u32, second: u32) -> bool {
    if !is_multiply_accumulate(second) {
        return false;
    }
    let Some(access) = memory_access(first) else {
        return false;
    };
    // A SIMD access is independent of the multiply-accumulate.
    if (first >> 26) & 1 == 1 {
        return true;
    }
    let rn = (second >> 5) & 0x1f;
    let ra = (second >> 10) & 0x1f;
    let rm = (second >> 16) & 0x1f;
    let uses = |r: u32| r == rn || r == rm || r == ra;
    // A true dependency on a loaded register makes the sequence safe.
    !(access.load && (uses(access.rt) || (access.pair && uses(access.rt2))))
}

/// The offsets of the multiply-accumulate instructions erratum 835769
/// affects in `code[start..end]`.
#[must_use]
pub fn scan_835769(code: &[u8], start: u64, end: u64) -> Vec<u64> {
    let end = end.min(u64::try_from(code.len()).unwrap_or(u64::MAX));
    let word = |offset: u64| {
        usize::try_from(offset)
            .ok()
            .and_then(|o| read_insn(code, o))
    };
    let mut sites = Vec::new();
    let mut offset = start;
    while offset.saturating_add(4) < end {
        let next = offset.saturating_add(4);
        if let (Some(first), Some(second)) = (word(offset), word(next))
            && is_835769_sequence(first, second)
        {
            sites.push(next);
        }
        offset = next;
    }
    sites
}

/// The two words of an erratum patch at `patch` that runs `moved` (the
/// instruction it replaces at `site`) and branches back after `site`.
///
/// # Errors
///
/// [`Overflow`] when the patch is more than 128 MiB from the site.
pub fn erratum_patch(patch: u64, site: u64, moved: u32) -> Result<[u32; 2], Overflow> {
    let back = (site.wrapping_add(4) as i64).wrapping_sub(patch.wrapping_add(4) as i64);
    Ok([moved, Field::Branch26.encode(B, back)?])
}

/// The branch that replaces the instruction at `site` with a jump to its
/// erratum patch at `patch`.
///
/// # Errors
///
/// [`Overflow`] when the patch is more than 128 MiB from the site.
pub fn erratum_branch(site: u64, patch: u64) -> Result<u32, Overflow> {
    Field::Branch26.encode(B, (patch as i64).wrapping_sub(site as i64))
}

/// Number of bytes a range-extension thunk occupies.
pub const THUNK_SIZE: u64 = 12;

/// The instructions of a range-extension thunk at `thunk` that branches to
/// `target`: `adrp x16, target; add x16, x16, :lo12:target; br x16`.
///
/// This is GNU ld's `adrp` stub. It reaches ±4 GiB, needs no writable
/// memory and is position-independent, so one form serves every output
/// kind.
///
/// # Errors
///
/// [`Overflow`] when the target is more than 4 GiB away.
pub fn thunk(thunk: u64, target: u64) -> Result<[u32; 3], Overflow> {
    let pages = (page(target) as i64).wrapping_sub(page(thunk) as i64);
    let first = Field::Adrp21.encode(adrp(16), pages)?;
    let second = Field::Add12.encode(add_imm(16, 16), target as i64)?;
    Ok([first, second, BR_X16])
}

/// Writes the three words of [`thunk`] into `out`.
///
/// # Errors
///
/// [`Overflow`] when the target is out of range or `out` is too short.
pub fn write_thunk(out: &mut [u8], at: u64, address: u64, target: u64) -> Result<(), Overflow> {
    let words = thunk(address, target)?;
    let at = usize::try_from(at).map_err(|_| Overflow)?;
    for (index, word) in words.into_iter().enumerate() {
        let offset = index
            .checked_mul(4)
            .and_then(|o| o.checked_add(at))
            .ok_or(Overflow)?;
        write_insn(out, offset, word).ok_or(Overflow)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_fields_round_trip() {
        // `bl` with a zero offset.
        let bl = 0x9400_0000;
        assert_eq!(Field::Branch26.encode(bl, 0x1000).unwrap(), 0x9400_0400);
        assert_eq!(Field::Branch26.encode(bl, -4).unwrap(), 0x97ff_ffff);
        assert_eq!(Field::Branch26.encode(bl, 1), Err(Overflow));
        assert_eq!(Field::Branch26.encode(bl, BRANCH26_REACH), Err(Overflow));
        assert!(Field::Branch26.encode(bl, BRANCH26_REACH - 4).is_ok());
        // `b.eq` and `tbz x0, #0, .`.
        assert_eq!(Field::Branch19.encode(0x5400_0000, 8).unwrap(), 0x5400_0040);
        assert_eq!(Field::Branch14.encode(0x3600_0000, 8).unwrap(), 0x3600_0040);
    }

    #[test]
    fn adr_and_adrp_split_their_immediate() {
        // adr x0, . + 0x1001 -> immlo = 1, immhi = 0x400
        let adr = 0x1000_0000;
        let encoded = Field::Adr21.encode(adr, 0x1001).unwrap();
        assert_eq!((encoded >> 29) & 3, 1);
        assert_eq!((encoded >> 5) & 0x7ffff, 0x400);
        // adrp x0, page + 0x2000
        let encoded = Field::Adrp21.encode(adrp(0), 0x2000).unwrap();
        assert_eq!((encoded >> 5) & 0x7ffff, 0);
        assert_eq!((encoded >> 29) & 3, 2);
        assert_eq!(Field::Adrp21.encode(adrp(0), 0x800), Err(Overflow));
        assert_eq!(Field::Adrp21.encode(adrp(0), ADRP_REACH), Err(Overflow));
        assert!(Field::Adrp21Nc.encode(adrp(0), ADRP_REACH).is_ok());
    }

    #[test]
    fn load_store_offsets_are_scaled() {
        let ldr = ldr_offset(0, 0);
        assert_eq!(
            Field::LdSt {
                scale: 3,
                checked: false
            }
            .encode(ldr, 0x1008)
            .unwrap(),
            insert(ldr, 1, 10, 12)
        );
        assert_eq!(
            Field::LdSt {
                scale: 3,
                checked: false
            }
            .encode(ldr, 4),
            Err(Overflow)
        );
        assert_eq!(
            Field::LdSt {
                scale: 0,
                checked: false
            }
            .encode(0x3940_0000, 0x123)
            .unwrap(),
            insert(0x3940_0000, 0x123, 10, 12)
        );
    }

    #[test]
    fn movw_groups_check_and_invert() {
        let movz0 = movz(0, 0, 0);
        let encoded = Field::Movw {
            shift: 16,
            check: MovwCheck::None,
        }
        .encode(movz0, 0x1234_5678)
        .unwrap();
        assert_eq!((encoded >> 5) & 0xffff, 0x1234);
        // A negative signed group turns movz into movn.
        let encoded = Field::Movw {
            shift: 0,
            check: MovwCheck::Signed,
        }
        .encode(movz0, -1)
        .unwrap();
        assert_eq!(encoded & (1 << 30), 0);
        assert_eq!((encoded >> 5) & 0xffff, 0);
        assert_eq!(
            Field::Movw {
                shift: 0,
                check: MovwCheck::Unsigned,
            }
            .encode(movz0, 0x1_0000),
            Err(Overflow)
        );
    }

    #[test]
    fn adrp_and_add_immediates_decode() {
        for pages in [0i64, 1, -1, 0x7ffff, -0x10_0000] {
            let insn = Field::Adrp21.encode(adrp(3), pages << 12).unwrap();
            assert!(is_adrp(insn));
            assert_eq!(adrp_offset(insn), pages << 12, "{pages:#x} pages");
        }
        let add = Field::Add12.encode(add_imm(1, 1), 0x1234).unwrap();
        assert!(is_add_imm64(add));
        assert_eq!(add_immediate(add), 0x234);
        assert!(!is_add_imm64(0x1100_0000), "a 32-bit add");
        assert!(is_load_store_unsigned(ldr_offset(0, 1)));
        assert_eq!(base_register(ldr_offset(0, 7)), 7);
        assert!(!is_load_store_unsigned(add));
    }

    fn code(words: &[u32]) -> Vec<u8> {
        words.iter().flat_map(|w| w.to_le_bytes()).collect()
    }

    #[test]
    fn erratum_843419_sequences_are_found_at_page_ends() {
        let adrp_x0 = adrp(0);
        let ldr_x1_x2 = ldr_offset(1, 2);
        let ldr_x3_x0 = ldr_offset(3, 0);
        let ldr_x0_x2 = ldr_offset(0, 2);
        assert!(is_843419_sequence(adrp_x0, ldr_x1_x2, ldr_x3_x0));
        // The second instruction writes the base register.
        assert!(!is_843419_sequence(adrp_x0, ldr_x0_x2, ldr_x3_x0));
        // Another base register.
        assert!(!is_843419_sequence(adrp_x0, ldr_x1_x2, ldr_offset(3, 1)));
        // `adrp; ldr; ldr` at 0xff8 in a section at 0x1000: the site is the
        // last `ldr`.
        let mut words = vec![NOP; 0xff8 / 4];
        words.extend([adrp_x0, ldr_x1_x2, ldr_x3_x0, NOP]);
        let bytes = code(&words);
        let end = bytes.len() as u64;
        assert_eq!(scan_843419(&bytes, 0x1000, 0, end), [0x1000]);
        // Eight bytes later the `adrp` starts a page: nothing.
        assert!(scan_843419(&bytes, 0x1008, 0, end).is_empty());
        // Four bytes later it is at 0xffc: still affected.
        assert_eq!(scan_843419(&bytes, 0x1004, 0, end), [0x1000]);
        // Four instructions, and a branch in third place.
        let mut words = vec![NOP; 0xffc / 4];
        words.extend([adrp_x0, ldr_x1_x2, NOP, ldr_x3_x0]);
        assert_eq!(scan_843419(&code(&words), 0, 0, 0x100c), [0x1008]);
        words[0xffc / 4 + 2] = B;
        assert!(scan_843419(&code(&words), 0, 0, 0x100c).is_empty());
        // Truncated or empty ranges do not panic.
        assert!(scan_843419(&bytes[..0xffa], 0x1000, 0, end).is_empty());
        assert!(scan_843419(&bytes, 0x1000, end, 0).is_empty());
    }

    #[test]
    fn erratum_835769_sequences_follow_gnu_ld() {
        // madd x3, x4, x5, x6 and madd x3, x1, x5, x6.
        let madd = 0x9b05_1883;
        let madd_x1 = 0x9b05_1823;
        let ldr_x1 = ldr_offset(1, 2);
        assert!(is_multiply_accumulate(madd));
        assert!(!is_multiply_accumulate(0x9b05_7c83), "mul is madd with xzr");
        assert!(is_835769_sequence(ldr_x1, madd));
        // A load the multiply-accumulate depends on is safe.
        assert!(!is_835769_sequence(ldr_x1, madd_x1));
        // A store is not a dependency.
        assert!(is_835769_sequence(0xf900_0041, madd_x1));
        assert!(!is_835769_sequence(add_imm(1, 2), madd));
        assert_eq!(scan_835769(&code(&[ldr_x1, madd, NOP]), 0, 12), [4]);
        assert!(scan_835769(&code(&[ldr_x1, madd]), 0, 4).is_empty());
    }

    #[test]
    fn erratum_patches_branch_back() {
        let [moved, back] = erratum_patch(0x2000, 0x1000, 0xf940_0003).unwrap();
        assert_eq!(moved, 0xf940_0003);
        assert_eq!(back, Field::Branch26.encode(B, 0x1004 - 0x2004).unwrap());
        assert_eq!(
            erratum_branch(0x1000, 0x2000).unwrap(),
            Field::Branch26.encode(B, 0x1000).unwrap()
        );
        assert_eq!(erratum_branch(0, 0x1000_0000), Err(Overflow));
    }

    #[test]
    fn thunks_reach_their_target() {
        let words = thunk(0x1000, 0x8000_2000).unwrap();
        assert_eq!(words[2], BR_X16);
        // adrp x16, 0x80002000 - 0x1000 pages
        assert_eq!(words[0] & 0x1f, 16);
        assert_eq!(words[1] & 0x1f, 16);
        assert_eq!(thunk(0, 0x1_0000_0000), Err(Overflow));
    }
}
