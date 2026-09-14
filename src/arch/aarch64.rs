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
    fn thunks_reach_their_target() {
        let words = thunk(0x1000, 0x8000_2000).unwrap();
        assert_eq!(words[2], BR_X16);
        // adrp x16, 0x80002000 - 0x1000 pages
        assert_eq!(words[0] & 0x1f, 16);
        assert_eq!(words[1] & 0x1f, 16);
        assert_eq!(thunk(0, 0x1_0000_0000), Err(Overflow));
    }
}
