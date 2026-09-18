//! PowerPC64 (little-endian, ELFv2) instruction encoding.
//!
//! Everything here works on little-endian instruction words and knows
//! nothing about relocation types: the ELF backend maps a relocation to a
//! [`Field`], computes the value the ABI prescribes, and asks this module to
//! pack it into the instruction. The PLT call stubs, the lazy-binding
//! resolver (`.glink`), the range-extension thunks and the TLS relaxations
//! are written from the encodings here too.
//!
//! Naming follows the "64-bit ELF V2 ABI Specification": `#lo(x)` is the
//! low 16 bits, `#hi(x)` the next 16, `#ha(x)` the next 16 adjusted for the
//! sign of `#lo(x)` (so that `addis` + a signed 16-bit displacement adds
//! back to `x`), and `#higher`, `#highest` and their `a` forms the bits
//! above.
//!
//! A 16-bit field of a little-endian instruction is at the instruction's
//! own address, so a relocation's offset is always the start of the word.
//! A prefixed (Power10) instruction is two words, the prefix first, and is
//! handled as one 64-bit value `prefix << 32 | suffix`.

#![deny(clippy::arithmetic_side_effects)]

/// `nop` (`ori 0, 0, 0`).
pub const NOP: u32 = 0x6000_0000;
/// `ld r2, 24(r1)`: restores the TOC pointer after a call through a stub.
pub const LD_R2_24_R1: u32 = 0xe841_0018;
/// `std r2, 24(r1)`: saves the TOC pointer in the caller's frame.
pub const STD_R2_24_R1: u32 = 0xf841_0018;
/// `mtctr r12`.
pub const MTCTR_R12: u32 = 0x7d89_03a6;
/// `bctr`.
pub const BCTR: u32 = 0x4e80_0420;
/// `b .` with a zero displacement.
pub const B: u32 = 0x4800_0000;
/// `addis r3, r13, 0`: the thread pointer, in local-exec TLS.
pub const ADDIS_R3_R13: u32 = 0x3c6d_0000;
/// `addi r3, r3, 0`.
pub const ADDI_R3_R3: u32 = 0x3863_0000;
/// `addi r3, r3, 4096`: the start of the TLS block plus the DTV bias.
pub const ADDI_R3_R3_4096: u32 = 0x3863_1000;
/// `add r3, r3, r13`: an initial-exec offset added to the thread pointer.
pub const ADD_R3_R3_R13: u32 = 0x7c63_6a14;
/// `addis rN, r13, 0` without the destination register.
pub const ADDIS_R13: u32 = 0x3c0d_0000;
/// `paddi r3, r13, 0, 0`.
pub const PADDI_R3_R13: u64 = 0x0600_0000_386d_0000;
/// `paddi r3, r13, 4096, 0`.
pub const PADDI_R3_R13_4096: u64 = 0x0600_0000_386d_1000;
/// `paddi rN, r13, 0, 0` without the destination register.
pub const PADDI_R13: u64 = 0x0600_0000_380d_0000;
/// `pld r3, 0(0), 1`: a PC-relative load of r3.
pub const PLD_R3: u64 = 0x0410_0000_e460_0000;

/// The reach of a `b`/`bl` instruction: ±32 MiB.
pub const BRANCH24_REACH: i64 = 1 << 25;
/// The reach of a conditional branch (`bc`): ±32 KiB.
pub const BRANCH14_REACH: i64 = 1 << 15;

/// The offset of the TOC base (`.TOC.`) from the start of `.got`, so that
/// a signed 16-bit displacement reaches 64 KiB of TOC.
pub const TOC_BIAS: u64 = 0x8000;
/// The thread pointer (`r13`) is this far past the start of the
/// executable's TLS block.
pub const TP_OFFSET: u64 = 0x7000;
/// The dynamic thread vector points this far past the start of each
/// module's TLS block, so `@dtprel` values are biased by it.
pub const DTV_OFFSET: u64 = 0x8000;

/// Why a value could not be packed into a field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncodeError {
    /// The value does not fit, or is not aligned as the field requires.
    Overflow,
    /// The instruction is not one the field can rewrite.
    BadInstruction,
}

/// `#lo(v)`.
#[must_use]
pub const fn lo(value: u64) -> u32 {
    (value & 0xffff) as u32
}

/// `#hi(v)`.
#[must_use]
pub const fn hi(value: u64) -> u32 {
    ((value >> 16) & 0xffff) as u32
}

/// `#ha(v)`: `#hi` adjusted for the sign of `#lo`.
#[must_use]
pub const fn ha(value: u64) -> u32 {
    ((value.wrapping_add(0x8000) >> 16) & 0xffff) as u32
}

/// Whether `#ha(v)` is zero for the full 64-bit value, so that the
/// `addis` of a pair adds nothing and can become a `nop`.
#[must_use]
pub const fn ha_is_zero(value: u64) -> bool {
    value.wrapping_add(0x8000) >> 16 == 0
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

/// Whether `value` fits `bits` bits, signed or unsigned.
#[must_use]
pub fn fits_either(value: i64, bits: u32) -> bool {
    if fits_signed(value, bits) {
        return true;
    }
    match 1i64.checked_shl(bits) {
        Some(limit) => value >= 0 && value < limit,
        None => true,
    }
}

/// The primary opcode (bits 0-5) of an instruction.
#[must_use]
pub const fn primary_opcode(insn: u32) -> u32 {
    insn >> 26
}

/// Whether a direct branch at `from` reaches `to`.
#[must_use]
pub fn branch24_in_range(from: u64, to: u64) -> bool {
    let delta = (to as i64).wrapping_sub(from as i64);
    (-BRANCH24_REACH..BRANCH24_REACH).contains(&delta)
}

/// The byte offset from a function's global entry point to its local one,
/// from the three `st_other` bits the ELFv2 ABI reserves for it. Values 0
/// and 1 mean both entry points coincide; 7 is reserved.
#[must_use]
pub const fn local_entry_offset(st_other: u8) -> u64 {
    match (st_other >> 5) & 7 {
        v @ 2..=6 => 1 << v,
        _ => 0,
    }
}

/// Whether the function with `st_other` treats `r2` as caller-saved: it
/// may clobber the TOC pointer (ELFv2 `st_other` value 1).
#[must_use]
pub const fn clobbers_toc(st_other: u8) -> bool {
    (st_other >> 5) & 7 == 1
}

/// Whether the DS-form field of `insn` is really a DQ form (`lxv`,
/// `stxv`, `lq`, `lxvp`, `stxvp`), which keeps four low bits.
#[must_use]
pub const fn is_dq_form(insn: u32) -> bool {
    match primary_opcode(insn) {
        6 | 56 => true,
        61 => insn & 3 == 1,
        _ => false,
    }
}

/// Whether `insn` is an update-form load or store (`lbzu`, `ldu`, …),
/// whose base register cannot be replaced by `r2`.
#[must_use]
pub const fn is_update_form(insn: u32) -> bool {
    match primary_opcode(insn) {
        33 | 35 | 41 | 43 | 49 | 51 | 37 | 39 | 45 | 53 | 55 => true,
        58 | 62 => insn & 3 == 1,
        _ => false,
    }
}

/// A relocatable field of one instruction (or data word).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    /// A 16-bit value, signed or unsigned (`ADDR16`, `TOC16`, `GOT16`).
    Half16,
    /// A signed 16-bit value (`TPREL16`, `DTPREL16`).
    Half16Signed,
    /// `#lo`.
    Lo,
    /// `#hi`, checked to fit 32 bits signed.
    Hi,
    /// `#ha`, checked to fit 32 bits signed.
    Ha,
    /// `#hi`, unchecked (`_HIGH`).
    High,
    /// `#ha`, unchecked (`_HIGHA`).
    Higha,
    /// Bits 32-47.
    Higher,
    /// Bits 32-47, adjusted.
    Highera,
    /// Bits 48-63.
    Highest,
    /// Bits 48-63, adjusted.
    Highesta,
    /// A DS- or DQ-form displacement, checked to fit 16 bits signed.
    Ds,
    /// `#lo` in a DS- or DQ-form displacement.
    LoDs,
    /// `#ha` of a TOC-relative value: the `addis` becomes a `nop` when it
    /// would add zero.
    HaToc,
    /// `#lo` of a TOC-relative value: when `#ha` is zero, the base register
    /// becomes `r2` (the `addis` that computed it became a `nop`).
    LoToc,
    /// [`Field::LoToc`] for a DS-form instruction.
    LoDsToc,
    /// A TOC-indirect `ld` turned into the `addi` of a TOC-relative
    /// address, then packed as [`Field::LoToc`].
    LoDsToAddi,
    /// A 24-bit branch displacement (`b`, `bl`).
    Rel24,
    /// A 14-bit conditional branch displacement (`bc`).
    Rel14,
    /// A 24-bit absolute branch target (`ba`).
    Addr24,
    /// A 14-bit absolute conditional branch target (`bca`).
    Addr14,
    /// The 34-bit displacement of a prefixed instruction.
    Prefixed34,
    /// General-dynamic → initial-exec: `addi r3, rA, …` becomes
    /// `ld r3, #lo(…)(rA)`.
    LdR3LoDs,
    /// General-dynamic → initial-exec, PC-relative: the `paddi` becomes
    /// `pld r3, …@pcrel`.
    PldR3,
    /// A GOT-indirect `pld` relaxed to the `paddi` of a PC-relative
    /// address.
    PldToPaddi,
    /// The `paddi` of a relaxed GOT access merged with the load or store
    /// the value (an offset) locates; written by the ELF backend, which
    /// sees both instructions.
    PcrelOpt,
}

impl Field {
    /// The number of bytes the field occupies: two for a plain 16-bit
    /// field, four for a field that reads or rewrites its instruction,
    /// eight for a prefixed instruction.
    #[must_use]
    pub const fn bytes(self) -> usize {
        match self {
            Self::Half16
            | Self::Half16Signed
            | Self::Lo
            | Self::Hi
            | Self::Ha
            | Self::High
            | Self::Higha
            | Self::Higher
            | Self::Highera
            | Self::Highest
            | Self::Highesta => 2,
            Self::Prefixed34 | Self::PldR3 | Self::PldToPaddi | Self::PcrelOpt => 8,
            _ => 4,
        }
    }

    /// Packs `value` into the 16-bit field `old` (a [`Field::bytes`] of 2).
    ///
    /// # Errors
    ///
    /// [`EncodeError::Overflow`] when the value does not fit.
    pub fn encode16(self, old: u16, value: i64) -> Result<u16, EncodeError> {
        let _ = old;
        let v = value as u64;
        let half = match self {
            Self::Half16 => {
                if !fits_either(value, 16) {
                    return Err(EncodeError::Overflow);
                }
                lo(v)
            }
            Self::Half16Signed => {
                if !fits_signed(value, 16) {
                    return Err(EncodeError::Overflow);
                }
                lo(v)
            }
            Self::Lo => lo(v),
            Self::Hi => {
                if !fits_signed(value, 32) {
                    return Err(EncodeError::Overflow);
                }
                hi(v)
            }
            Self::Ha => {
                if !fits_signed(value.wrapping_add(0x8000), 32) {
                    return Err(EncodeError::Overflow);
                }
                ha(v)
            }
            Self::High => hi(v),
            Self::Higha => ha(v),
            Self::Higher => ((v >> 32) & 0xffff) as u32,
            Self::Highera => ((v.wrapping_add(0x8000) >> 32) & 0xffff) as u32,
            Self::Highest => (v >> 48) as u32,
            Self::Highesta => (v.wrapping_add(0x8000) >> 48) as u32,
            _ => return Err(EncodeError::BadInstruction),
        };
        Ok(half as u16)
    }

    /// Packs `value` into instruction `insn` (a [`Field::bytes`] of 4).
    ///
    /// # Errors
    ///
    /// [`EncodeError::Overflow`] when the value does not fit or is
    /// misaligned, [`EncodeError::BadInstruction`] when a rewriting field
    /// finds an instruction it cannot rewrite.
    pub fn encode32(self, insn: u32, value: i64) -> Result<u32, EncodeError> {
        let v = value as u64;
        let ds_mask = if is_dq_form(insn) { 0xf } else { 0x3 };
        let low = |insn: u32, half: u32| (insn & 0xffff_0000) | (half & 0xffff);
        match self {
            Self::Ds => {
                if !fits_signed(value, 16) || lo(v) & ds_mask != 0 {
                    return Err(EncodeError::Overflow);
                }
                Ok(low(insn, (insn & ds_mask) | lo(v)))
            }
            Self::LoDs => {
                if lo(v) & ds_mask != 0 {
                    return Err(EncodeError::Overflow);
                }
                Ok(low(insn, (insn & ds_mask) | lo(v)))
            }
            Self::HaToc => {
                if ha_is_zero(v) {
                    return Ok(NOP);
                }
                if !fits_signed(value.wrapping_add(0x8000), 32) {
                    return Err(EncodeError::Overflow);
                }
                Ok(low(insn, ha(v)))
            }
            Self::LoToc => {
                if ha_is_zero(v) {
                    if is_update_form(insn) {
                        return Err(EncodeError::BadInstruction);
                    }
                    return Ok((insn & 0xffe0_0000) | 0x0002_0000 | lo(v));
                }
                Ok(low(insn, lo(v)))
            }
            Self::LoDsToc => {
                if lo(v) & ds_mask != 0 {
                    return Err(EncodeError::Overflow);
                }
                if ha_is_zero(v) {
                    if is_update_form(insn) {
                        return Err(EncodeError::BadInstruction);
                    }
                    return Ok((insn & (0xffe0_0000 | ds_mask)) | 0x0002_0000 | lo(v));
                }
                Ok(low(insn, (insn & ds_mask) | lo(v)))
            }
            Self::LoDsToAddi => {
                // `ld rT, x(rA)` -> `addi rT, rA, x`.
                if primary_opcode(insn) != 58 {
                    return Err(EncodeError::BadInstruction);
                }
                Self::LoToc.encode32((insn & 0x03ff_ffff) | 0x3800_0000, value)
            }
            Self::Rel24 | Self::Addr24 => {
                let fits = if self == Self::Rel24 {
                    fits_signed(value, 26)
                } else {
                    fits_signed(value, 26) || (0..1 << 26).contains(&value)
                };
                if value & 3 != 0 || !fits {
                    return Err(EncodeError::Overflow);
                }
                Ok((insn & !0x03ff_fffc) | (v as u32 & 0x03ff_fffc))
            }
            Self::Rel14 => {
                if value & 3 != 0 || !fits_signed(value, 16) {
                    return Err(EncodeError::Overflow);
                }
                Ok((insn & !0xfffc) | (v as u32 & 0xfffc))
            }
            Self::Addr14 => {
                if value & 3 != 0 {
                    return Err(EncodeError::Overflow);
                }
                Ok((insn & !0xfffc) | (v as u32 & 0xfffc))
            }
            Self::LdR3LoDs => {
                // `addi r3, rA, …` -> `ld r3, …(rA)`.
                let ld = 0xe860_0000 | (insn & 0x001f_0000);
                Self::LoDs.encode32(ld, value)
            }
            _ => Err(EncodeError::BadInstruction),
        }
    }

    /// Packs `value` into prefixed instruction `insn` (`prefix << 32 |
    /// suffix`, a [`Field::bytes`] of 8).
    ///
    /// # Errors
    ///
    /// [`EncodeError::Overflow`] when the value does not fit 34 bits,
    /// [`EncodeError::BadInstruction`] when the instruction is not the one
    /// a relaxation expects.
    pub fn encode64(self, insn: u64, value: i64) -> Result<u64, EncodeError> {
        let insn = match self {
            Self::Prefixed34 => insn,
            Self::PldR3 => PLD_R3,
            Self::PldToPaddi => {
                if insn & 0xfc00_0000 != 0xe400_0000 {
                    return Err(EncodeError::BadInstruction);
                }
                (insn & !0xff00_0000_fc00_0000) | 0x0600_0000_3800_0000
            }
            _ => return Err(EncodeError::BadInstruction),
        };
        prefixed34(insn, value)
    }
}

/// Packs a 34-bit signed displacement into prefixed instruction `insn`.
///
/// # Errors
///
/// [`EncodeError::Overflow`] when `value` does not fit.
pub fn prefixed34(insn: u64, value: i64) -> Result<u64, EncodeError> {
    if !fits_signed(value, 34) {
        return Err(EncodeError::Overflow);
    }
    let v = value as u64;
    Ok((insn & !0x0003_ffff_0000_ffff) | ((v & 0x3_ffff_0000) << 16) | (v & 0xffff))
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

/// Reads the prefixed instruction at `at`: `prefix << 32 | suffix`.
#[must_use]
pub fn read_prefixed(data: &[u8], at: usize) -> Option<u64> {
    let prefix = read_insn(data, at)?;
    let suffix = read_insn(data, at.checked_add(4)?)?;
    Some((u64::from(prefix) << 32) | u64::from(suffix))
}

/// Writes prefixed instruction `insn` (`prefix << 32 | suffix`) at `at`.
pub fn write_prefixed(data: &mut [u8], at: usize, insn: u64) -> Option<()> {
    write_insn(data, at, (insn >> 32) as u32)?;
    write_insn(data, at.checked_add(4)?, insn as u32)
}

/// Converts the X-form load, store or `add` of an initial-exec access
/// (`lwzx r3, r9, x@tls`) to the D-form with the same registers, whose
/// displacement then takes `x@tprel@l`. Returns the new instruction and
/// whether it is DS-form.
#[must_use]
pub fn x_to_d_form(insn: u32) -> Option<(u32, bool)> {
    if primary_opcode(insn) != 31 {
        return None;
    }
    let (opcode, ds) = match (insn >> 1) & 0x3ff {
        87 => (34 << 26, false),       // lbzx -> lbz
        279 => (40 << 26, false),      // lhzx -> lhz
        23 => (32 << 26, false),       // lwzx -> lwz
        215 => (38 << 26, false),      // stbx -> stb
        407 => (44 << 26, false),      // sthx -> sth
        151 => (36 << 26, false),      // stwx -> stw
        343 => (42 << 26, false),      // lhax -> lha
        535 => (48 << 26, false),      // lfsx -> lfs
        599 => (50 << 26, false),      // lfdx -> lfd
        663 => (52 << 26, false),      // stfsx -> stfs
        727 => (54 << 26, false),      // stfdx -> stfd
        266 => (14 << 26, false),      // add -> addi
        341 => ((58 << 26) | 2, true), // lwax -> lwa
        21 => (58 << 26, true),        // ldx -> ld
        149 => (62 << 26, true),       // stdx -> std
        _ => return None,
    };
    Some((opcode | (insn & 0x03ff_0000), ds))
}

/// The prefixed PC-relative form of D-form load or store `access` (for
/// example `plwz rT, 0(0), 1` for `lwz rT, d(rA)`), with the displacement
/// cleared. `None` for an instruction that has none.
#[must_use]
pub fn pcrel_form(access: u32) -> Option<u64> {
    const MLS: u64 = 0x0610_0000_0000_0000;
    const EIGHT_LS: u64 = 0x0410_0000_0000_0000;
    // What of the access instruction the suffix keeps.
    const OPCODE_AND_RT: u64 = 0xffe0_0000;
    const RT: u64 = 0x03e0_0000;
    let opcode = access & 0xfc00_0000;
    let key = if matches!(
        opcode,
        0xe400_0000 | 0xe800_0000 | 0xf400_0000 | 0xf800_0000
    ) && !is_dq_form(access)
    {
        access & 0xfc00_0003
    } else if opcode == 0xf400_0000 {
        access & 0xfc00_0007
    } else if opcode == 0x1800_0000 {
        access & 0xfc00_000f
    } else {
        opcode
    };
    let (prefixed, mask, move_tx) = match key {
        // lbz, lhz, lwz, lha, lfs, lfd, stb, sth, stw, stfs, stfd.
        0x8800_0000 | 0xa000_0000 | 0x8000_0000 | 0xa800_0000 | 0xc000_0000 | 0xc800_0000
        | 0x9800_0000 | 0xb000_0000 | 0x9000_0000 | 0xd000_0000 | 0xd800_0000 => {
            (MLS, OPCODE_AND_RT, false)
        }
        0xe800_0002 => (EIGHT_LS | 0xa400_0000, RT, false), // lwa
        0xe800_0000 => (EIGHT_LS | 0xe400_0000, RT, false), // ld
        0xe400_0003 => (EIGHT_LS | 0xac00_0000, RT, false), // lxssp
        0xe400_0002 => (EIGHT_LS | 0xa800_0000, RT, false), // lxsd
        0xf400_0001 => (EIGHT_LS | 0xc800_0000, RT, true),  // lxv
        0x1800_0000 => (EIGHT_LS | 0xe800_0000, OPCODE_AND_RT, false), // lxvp
        0xf800_0000 => (EIGHT_LS | 0xf400_0000, RT, false), // std
        0xf400_0003 => (EIGHT_LS | 0xbc00_0000, RT, false), // stxssp
        0xf400_0002 => (EIGHT_LS | 0xb800_0000, RT, false), // stxsd
        0xf400_0005 => (EIGHT_LS | 0xd800_0000, RT, true),  // stxv
        0x1800_0001 => (EIGHT_LS | 0xf800_0000, OPCODE_AND_RT, false), // stxvp
        _ => return None,
    };
    let mut form = prefixed | (u64::from(access) & mask);
    if move_tx {
        // The TX/SX bit moves from bit 28 to bit 5 of the suffix.
        form |= (u64::from(access) & 0x8) << 23;
    }
    Some(form)
}

/// The displacement a prefixed PC-relative `paddi` and the load or store
/// that uses its result add up to.
#[must_use]
pub fn total_displacement(paddi: u64, access: u32) -> i64 {
    let disp34 = (((((paddi >> 16) & 0x3_ffff_0000) | (paddi & 0xffff)) << 30) as i64) >> 30;
    let mut disp16 = i64::from(access as u16 as i16);
    if is_dq_form(access) {
        disp16 &= !0xf;
    } else if matches!(
        access & 0xfc00_0003,
        0xe800_0002 | 0xe800_0000 | 0xf800_0000
    ) || (matches!(access & 0xfc00_0000, 0xe400_0000 | 0xf400_0000)
        && matches!(access & 3, 2 | 3))
    {
        disp16 &= !0x3;
    }
    disp34.wrapping_add(disp16)
}

/// The instructions of a PLT call stub that loads its target from the GOT
/// word `toc_offset` bytes from the TOC base: save `r2` in the caller's
/// frame, then `addis r12, r2, #ha; ld r12, #lo(r12); mtctr r12; bctr`.
/// The caller's `nop` after the `bl` becomes `ld r2, 24(r1)`.
///
/// # Errors
///
/// [`EncodeError::Overflow`] when the word is more than 2 GiB from the TOC
/// base or not word-aligned.
pub fn plt_call_stub(toc_offset: i64) -> Result<[u32; 5], EncodeError> {
    if !fits_signed(toc_offset.wrapping_add(0x8000), 32) || toc_offset & 3 != 0 {
        return Err(EncodeError::Overflow);
    }
    let v = toc_offset as u64;
    Ok([
        STD_R2_24_R1,
        0x3d82_0000 | ha(v),
        0xe98c_0000 | lo(v),
        MTCTR_R12,
        BCTR,
    ])
}

/// Size of a PLT call stub, in bytes.
pub const PLT_CALL_STUB_SIZE: u64 = 20;

/// Number of bytes a range-extension thunk occupies.
pub const THUNK_SIZE: u64 = 32;

/// The instructions of a range-extension thunk at `thunk` that branches to
/// `target`. The address is computed from the thunk's own (`bcl` reads the
/// program counter), not from the TOC pointer, so the thunk serves callers
/// that do not maintain `r2` as well, and it leaves `r12` holding the
/// target, as a global entry point expects. It clobbers `r0`, `r11` and
/// `r12`, which the ABI lets call linkage code use. This is GNU ld's
/// `long_branch_notoc` stub.
///
/// # Errors
///
/// [`EncodeError::Overflow`] when the target is more than 2 GiB away.
pub fn thunk(thunk: u64, target: u64) -> Result<[u32; 8], EncodeError> {
    let offset = (target as i64).wrapping_sub(thunk.wrapping_add(8) as i64);
    if !fits_signed(offset.wrapping_add(0x8000), 32) {
        return Err(EncodeError::Overflow);
    }
    let v = offset as u64;
    Ok([
        0x7d88_02a6,         // mflr r12
        0x429f_0005,         // bcl 20, 31, .+4
        0x7d68_02a6,         // mflr r11
        0x7d88_03a6,         // mtlr r12
        0x3d8b_0000 | ha(v), // addis r12, r11, #ha
        0x398c_0000 | lo(v), // addi r12, r12, #lo
        MTCTR_R12,
        BCTR,
    ])
}

/// Writes the words of [`thunk`] into `out` at `at`.
///
/// # Errors
///
/// [`EncodeError::Overflow`] when the target is out of range or `out` is
/// too short.
pub fn write_thunk(out: &mut [u8], at: u64, address: u64, target: u64) -> Result<(), EncodeError> {
    write_words(out, at, &thunk(address, target)?)
}

/// Writes `words` into `out` starting at `at`.
///
/// # Errors
///
/// [`EncodeError::Overflow`] when `out` is too short.
pub fn write_words(out: &mut [u8], at: u64, words: &[u32]) -> Result<(), EncodeError> {
    let at = usize::try_from(at).map_err(|_| EncodeError::Overflow)?;
    for (index, word) in words.iter().enumerate() {
        let offset = index
            .checked_mul(4)
            .and_then(|o| o.checked_add(at))
            .ok_or(EncodeError::Overflow)?;
        write_insn(out, offset, *word).ok_or(EncodeError::Overflow)?;
    }
    Ok(())
}

/// Size of the lazy-binding resolver at the start of `.glink`.
pub const GLINK_HEADER_SIZE: u64 = 60;

/// The lazy-binding resolver: computes the PLT index from the address of
/// the lazy entry the call stub jumped to (in `r12`), then jumps to
/// `_dl_runtime_resolve` through the first `.plt` word with the link map
/// (the second word) in `r11`. `got_plt_delta` is the address of `.plt`
/// minus that of the resolver's third instruction; it is stored in the last
/// eight bytes.
#[must_use]
pub fn glink_header(got_plt_delta: i64) -> ([u32; 13], u64) {
    (
        [
            0x7c08_02a6, // mflr r0
            0x429f_0005, // bcl 20, 31, .+4
            0x7d68_02a6, // mflr r11
            0x7c08_03a6, // mtlr r0
            0x7d8b_6050, // subf r12, r11, r12
            0x380c_ffcc, // addi r0, r12, -52
            0x7800_f082, // rldicl r0, r0, 62, 2
            0xe98b_002c, // ld r12, 44(r11)
            0x7d6c_5a14, // add r11, r12, r11
            0xe98b_0000, // ld r12, 0(r11)
            0xe96b_0008, // ld r11, 8(r11)
            MTCTR_R12,
            BCTR,
        ],
        got_plt_delta as u64,
    )
}

/// A lazy `.glink` entry `offset` bytes after the start of `.glink`: a
/// branch back to the resolver.
///
/// # Errors
///
/// [`EncodeError::Overflow`] when the resolver is out of reach.
pub fn glink_entry(offset: u64) -> Result<u32, EncodeError> {
    let back = (offset as i64).wrapping_neg();
    Field::Rel24.encode32(B, back)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adjusted_halves() {
        assert_eq!(ha(0x1_8000), 2);
        assert_eq!(ha(0x1_7fff), 1);
        assert_eq!(lo(0x1_8000), 0x8000);
        assert_eq!(ha((-4i64) as u64), 0);
        assert!(ha_is_zero((-0x8000i64) as u64));
        assert!(!ha_is_zero(0x8000));
        assert_eq!(local_entry_offset(3 << 5), 8);
        assert_eq!(local_entry_offset(2 << 5), 4);
        assert_eq!(local_entry_offset(1 << 5), 0);
        assert_eq!(local_entry_offset(7 << 5), 0);
    }

    #[test]
    fn branch_fields() {
        let bl = 0x4800_0001;
        assert_eq!(Field::Rel24.encode32(bl, 0x100).unwrap(), 0x4800_0101);
        assert_eq!(Field::Rel24.encode32(bl, -4).unwrap(), 0x4bff_fffd);
        assert_eq!(Field::Rel24.encode32(bl, 2), Err(EncodeError::Overflow));
        assert_eq!(
            Field::Rel24.encode32(bl, BRANCH24_REACH),
            Err(EncodeError::Overflow)
        );
        // bc 12, 2, .+8
        assert_eq!(Field::Rel14.encode32(0x4182_0000, 8).unwrap(), 0x4182_0008);
    }

    #[test]
    fn toc_optimization() {
        // addis r3, r2, 0 -> nop; ld r3, 8(r3) -> ld r3, 8(r2).
        assert_eq!(Field::HaToc.encode32(0x3c62_0000, 8).unwrap(), NOP);
        assert_eq!(
            Field::LoDsToc.encode32(0xe863_0000, 8).unwrap(),
            0xe862_0008
        );
        // Out of the 32 KiB window, the pair stays.
        assert_eq!(
            Field::HaToc.encode32(0x3c62_0000, 0x1_0010).unwrap(),
            0x3c62_0001
        );
        assert_eq!(
            Field::LoDsToc.encode32(0xe863_0000, 0x1_0010).unwrap(),
            0xe863_0010
        );
        // ld r3, x(r3) -> addi r3, r2, x
        assert_eq!(
            Field::LoDsToAddi.encode32(0xe863_0000, -16).unwrap(),
            0x3862_fff0
        );
        assert_eq!(
            Field::LoDsToAddi.encode32(0x3863_0000, 0),
            Err(EncodeError::BadInstruction)
        );
        // A misaligned DS displacement.
        assert_eq!(
            Field::LoDs.encode32(0xe863_0000, 6),
            Err(EncodeError::Overflow)
        );
    }

    #[test]
    fn prefixed_displacement() {
        // pld r3, 0(0), 1 with a displacement of -8.
        let pld = 0x0410_0000_e460_0000;
        let packed = Field::Prefixed34.encode64(pld, -8).unwrap();
        assert_eq!(packed, 0x0413_ffff_e460_fff8);
        let paddi = Field::PldToPaddi.encode64(pld, 0x12_3456).unwrap();
        assert_eq!(paddi, 0x0610_0012_3860_3456);
        assert_eq!(
            Field::Prefixed34.encode64(pld, 1 << 33),
            Err(EncodeError::Overflow)
        );
    }

    #[test]
    fn stubs() {
        let stub = plt_call_stub(-0x7ff0).unwrap();
        assert_eq!(
            stub,
            [0xf841_0018, 0x3d82_0000, 0xe98c_8010, MTCTR_R12, BCTR]
        );
        let thunk = thunk(0x1000_0000, 0x2000_0008).unwrap();
        assert_eq!(thunk[4], 0x3d8b_1000);
        assert_eq!(thunk[5], 0x398c_0000);
        assert_eq!(glink_entry(64).unwrap(), 0x4bff_ffc0);
        assert_eq!(x_to_d_form(0x7c69_6a14), Some((0x3869_0000, false)));
        assert_eq!(x_to_d_form(0x7c69_682a), Some((0xe869_0000, true)));
    }
}
