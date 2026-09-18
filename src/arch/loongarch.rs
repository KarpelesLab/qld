//! LoongArch instruction encoding (LA64, little-endian).
//!
//! Everything here works on 32-bit little-endian instruction words and knows
//! nothing about relocation types: the ELF backend maps a relocation to a
//! [`Field`], computes the value the psABI prescribes, and asks this module
//! to pack it. The same encodings serve the PLT and the rewrites of TLS and
//! linker relaxation, which replace whole instructions.
//!
//! LoongArch builds 32-bit PC-relative addresses from a `pcalau12i`, which
//! adds a signed 20-bit page count to the PC's page (`Page(x)` is
//! `x & !0xfff`, as for AArch64 `adrp`), and a following instruction that
//! adds a *signed* 12-bit offset. Because the low part is sign-extended, the
//! high part must be rounded ([`page_delta`] implements the psABI
//! algorithm, including the extreme code model's 64-bit form).
//!
//! Instruction formats, by the fields relocations patch:
//!
//! | Format | Immediate bits | Used by |
//! | --- | --- | --- |
//! | `1RI20` | `si20` at 5..25 | `lu12i.w`, `lu32i.d`, `pcaddi`, `pcalau12i`, `pcaddu12i`, `pcaddu18i` |
//! | `2RI12` | `si12`/`ui12` at 10..22 | `addi`, `ori`, `lu52i.d`, loads and stores |
//! | `2RI16` | `offs16` at 10..26 | `jirl`, `beq`/`bne`/`blt`/… |
//! | `1RI21` | `offs[15:0]` at 10..26, `offs[20:16]` at 0..5 | `beqz`, `bnez` |
//! | `I26` | `offs[15:0]` at 10..26, `offs[25:16]` at 0..10 | `b`, `bl` |

#![deny(clippy::arithmetic_side_effects)]

use super::Overflow;

/// The instruction word size: every LoongArch instruction is four bytes.
pub const INSN_SIZE: u64 = 4;

/// `nop` (`andi $zero, $zero, 0`).
pub const NOP: u32 = 0x0340_0000;
/// `break 0`.
pub const BREAK: u32 = 0x002a_0000;

/// Opcode of `pcaddi rd, si20`: `rd = PC + (si20 << 2)`.
pub const PCADDI: u32 = 0x1800_0000;
/// Opcode of `pcalau12i rd, si20`: `rd = Page(PC) + (si20 << 12)`.
pub const PCALAU12I: u32 = 0x1a00_0000;
/// Opcode of `pcaddu12i rd, si20`: `rd = PC + (si20 << 12)`.
pub const PCADDU12I: u32 = 0x1c00_0000;
/// Opcode of `pcaddu18i rd, si20`: `rd = PC + (si20 << 18)`.
pub const PCADDU18I: u32 = 0x1e00_0000;
/// Opcode of `lu12i.w rd, si20`.
pub const LU12I_W: u32 = 0x1400_0000;
/// Opcode of `addi.w rd, rj, si12`.
pub const ADDI_W: u32 = 0x0280_0000;
/// Opcode of `addi.d rd, rj, si12`.
pub const ADDI_D: u32 = 0x02c0_0000;
/// Opcode of `ori rd, rj, ui12`.
pub const ORI: u32 = 0x0380_0000;
/// Opcode of `ld.w rd, rj, si12`.
pub const LD_W: u32 = 0x2880_0000;
/// Opcode of `ld.d rd, rj, si12`.
pub const LD_D: u32 = 0x28c0_0000;
/// Opcode of `add.d rd, rj, rk`.
pub const ADD_D: u32 = 0x0010_8000;
/// Opcode of `sub.d rd, rj, rk`.
pub const SUB_D: u32 = 0x0011_8000;
/// Opcode of `srli.d rd, rj, ui6`.
pub const SRLI_D: u32 = 0x0045_0000;
/// Opcode of `jirl rd, rj, offs16`.
pub const JIRL: u32 = 0x4c00_0000;
/// Opcode of `b offs26`.
pub const B: u32 = 0x5000_0000;
/// Opcode of `bl offs26`.
pub const BL: u32 = 0x5400_0000;

/// `$zero`.
pub const R_ZERO: u32 = 0;
/// `$ra`, the return address.
pub const R_RA: u32 = 1;
/// `$tp`, the thread pointer.
pub const R_TP: u32 = 2;
/// `$a0`, the first argument and return value.
pub const R_A0: u32 = 4;
/// `$t0`.
pub const R_T0: u32 = 12;
/// `$t1`.
pub const R_T1: u32 = 13;
/// `$t2`.
pub const R_T2: u32 = 14;
/// `$t3`.
pub const R_T3: u32 = 15;

/// The reach of `b`/`bl`: ±128 MiB.
pub const B26_REACH: i64 = 1 << 27;
/// The reach of `pcaddi`: ±2 MiB.
pub const PCADDI_REACH: i64 = 1 << 21;

/// `Page(address)`: the address with its low 12 bits cleared.
#[must_use]
pub const fn page(address: u64) -> u64 {
    address & !0xfff
}

/// The value the psABI's `pcalau12i` relocations compute: the page delta
/// from the `pcalau12i` at `pc` to `dest`, adjusted so that adding the
/// sign-extended low 12 bits of `dest` lands on it, with bits 32..64
/// adjusted for the `lu32i.d`/`lu52i.d` pair of the extreme code model.
///
/// `pc` is the address of the `pcalau12i` itself; relocations on the later
/// instructions of an extreme-model sequence subtract their distance from
/// it first.
#[must_use]
pub fn page_delta(dest: u64, pc: u64) -> u64 {
    let mut result = page(dest).wrapping_sub(page(pc));
    if dest & 0x800 != 0 {
        result = result.wrapping_add(0x1000).wrapping_sub(0x1_0000_0000);
    }
    if result & 0x8000_0000 != 0 {
        result = result.wrapping_add(0x1_0000_0000);
    }
    result
}

/// The `si20` of a `pcaddu12i` that, with a sign-extended 12-bit offset,
/// reaches `offset` bytes from itself.
#[must_use]
pub const fn pcrel_hi20(offset: u32) -> u32 {
    offset.wrapping_add(0x800) >> 12
}

/// A relocatable field of an instruction (or of data).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    /// Bits 12..32 of the value in a `1RI20` immediate (`lu12i.w`,
    /// `pcalau12i`).
    Hi20,
    /// Bits 12..32 of the value plus `0x800` in a `1RI20` immediate: the
    /// high part of a pair whose low part is sign-extended (`le_hi20_r`).
    Hi20Round,
    /// Bits 32..52 of the value in a `1RI20` immediate (`lu32i.d`).
    Lo20,
    /// Bits 0..12 of the value in a `2RI12` immediate (`addi`, `ori`,
    /// loads and stores).
    Lo12,
    /// Bits 52..64 of the value in a `2RI12` immediate (`lu52i.d`).
    Hi12,
    /// An 18-bit byte offset, in words, in a `2RI16` branch.
    B16,
    /// A 23-bit byte offset, in words, in a `1RI21` branch.
    B21,
    /// A 28-bit byte offset, in words, in `b`/`bl`.
    B26,
    /// A 22-bit byte offset, in words, in `pcaddi`.
    Pcrel20S2,
    /// The low 12 bits of the value, sign-extended and in words, in the
    /// `offs16` of a `jirl` (a `pcalau12i` + `jirl` call).
    JirlLo12,
    /// A `pcaddu18i` + `jirl` pair (eight bytes) reaching ±128 GiB.
    Call36,
    /// The same pair, which may become `bl`/`b` + `nop` (linker
    /// relaxation).
    Call36Relax,
    /// The `lu12i.w` of a local-exec `lu12i.w; add.d …, $tp; addi.d`
    /// sequence ([`Field::Hi20Round`]), which becomes a `nop` when the
    /// offset fits 12 bits.
    TpHi20Relax,
    /// The `add.d …, $tp` of that sequence: a `nop` when the offset fits 12
    /// bits, unchanged otherwise.
    TpAddRelax,
    /// Its last instruction ([`Field::Lo12`]), which then addresses from
    /// `$tp` directly.
    TpLo12Relax,
    /// The low six bits of a byte (`DW_CFA_advance_loc`).
    Data6,
    /// A ULEB128 number, keeping its encoded length.
    Uleb128,
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

/// Bits `lsb..lsb + bits` of `value`.
fn bits_of(value: u64, lsb: u32, bits: u32) -> u32 {
    let mask = 1u64
        .checked_shl(bits)
        .map_or(u64::MAX, |b| b.wrapping_sub(1));
    (value.checked_shr(lsb).unwrap_or(0) & mask) as u32
}

fn insert(insn: u32, value: u32, lsb: u32, bits: u32) -> u32 {
    let mask = match 1u32.checked_shl(bits) {
        Some(bit) => bit.wrapping_sub(1),
        None => u32::MAX,
    };
    let shifted_mask = mask.checked_shl(lsb).unwrap_or(0);
    (insn & !shifted_mask) | ((value & mask).checked_shl(lsb).unwrap_or(0))
}

/// The `rd` field of an instruction.
#[must_use]
pub const fn rd(insn: u32) -> u32 {
    insn & 0x1f
}

/// The `rj` field of an instruction.
#[must_use]
pub const fn rj(insn: u32) -> u32 {
    (insn >> 5) & 0x1f
}

/// Replaces the `rj` field of `insn`.
#[must_use]
pub const fn with_rj(insn: u32, rj: u32) -> u32 {
    (insn & !(0x1f << 5)) | ((rj & 0x1f) << 5)
}

/// Whether `insn` is a `pcalau12i`.
#[must_use]
pub const fn is_pcalau12i(insn: u32) -> bool {
    insn & 0xfe00_0000 == PCALAU12I
}

/// Whether `insn` is a `jirl`.
#[must_use]
pub const fn is_jirl(insn: u32) -> bool {
    insn & 0xfc00_0000 == JIRL
}

/// Whether `insn` is `ld.d` (or, for 32-bit code, `ld.w`).
#[must_use]
pub const fn is_ld_word(insn: u32) -> bool {
    let op = insn & 0xffc0_0000;
    op == LD_D || op == LD_W
}

/// Whether `insn` is `addi.d` (or `addi.w`).
#[must_use]
pub const fn is_addi(insn: u32) -> bool {
    let op = insn & 0xffc0_0000;
    op == ADDI_D || op == ADDI_W
}

/// An instruction with a `1RI20` immediate: `op rd, si20`.
#[must_use]
pub const fn ri20(op: u32, rd: u32, si20: u32) -> u32 {
    op | (rd & 0x1f) | ((si20 & 0xf_ffff) << 5)
}

/// An instruction with a `2RI12` immediate: `op rd, rj, imm12`.
#[must_use]
pub const fn rri12(op: u32, rd: u32, rj: u32, imm12: u32) -> u32 {
    op | (rd & 0x1f) | ((rj & 0x1f) << 5) | ((imm12 & 0xfff) << 10)
}

/// A three-register instruction: `op rd, rj, rk`.
#[must_use]
pub const fn rrr(op: u32, rd: u32, rj: u32, rk: u32) -> u32 {
    op | (rd & 0x1f) | ((rj & 0x1f) << 5) | ((rk & 0x1f) << 10)
}

/// `jirl rd, rj, 0`.
#[must_use]
pub const fn jirl(rd: u32, rj: u32) -> u32 {
    JIRL | (rd & 0x1f) | ((rj & 0x1f) << 5)
}

impl Field {
    /// The number of bytes the field occupies (the minimum, for a ULEB128
    /// number).
    #[must_use]
    pub const fn bytes(self) -> usize {
        match self {
            Self::Call36 | Self::Call36Relax => 8,
            Self::Data6 | Self::Uleb128 => 1,
            _ => 4,
        }
    }

    /// Whether the field is data rather than an instruction field.
    #[must_use]
    pub const fn is_data(self) -> bool {
        matches!(self, Self::Data6 | Self::Uleb128)
    }

    /// Packs `value` into the single instruction `insn`.
    ///
    /// # Errors
    ///
    /// [`Overflow`] when the value does not fit a checked field or is not
    /// aligned as the field requires, and for the fields that are not one
    /// instruction or pick their instruction from the value ([`call36`] and
    /// the relaxable forms, which the ELF backend writes).
    pub fn encode(self, insn: u32, value: i64) -> Result<u32, Overflow> {
        let raw = value as u64;
        let branch = |bits: u32| {
            if value & 3 != 0 || !fits_signed(value, bits) {
                Err(Overflow)
            } else {
                Ok((value >> 2) as u32)
            }
        };
        match self {
            Self::Hi20 => Ok(insert(insn, bits_of(raw, 12, 20), 5, 20)),
            Self::Hi20Round => Ok(insert(
                insn,
                bits_of(raw.wrapping_add(0x800), 12, 20),
                5,
                20,
            )),
            Self::Lo20 => Ok(insert(insn, bits_of(raw, 32, 20), 5, 20)),
            Self::Lo12 => Ok(insert(insn, bits_of(raw, 0, 12), 10, 12)),
            Self::Hi12 => Ok(insert(insn, bits_of(raw, 52, 12), 10, 12)),
            Self::B16 => Ok(insert(insn, branch(18)?, 10, 16)),
            Self::B21 => {
                let offs = branch(23)?;
                Ok(insert(insert(insn, offs, 10, 16), offs >> 16, 0, 5))
            }
            Self::B26 => {
                let offs = branch(28)?;
                Ok(insert(insert(insn, offs, 10, 16), offs >> 16, 0, 10))
            }
            Self::Pcrel20S2 => Ok(insert(insn, branch(22)?, 5, 20)),
            Self::JirlLo12 => {
                if value & 3 != 0 {
                    return Err(Overflow);
                }
                // Sign-extend the low 12 bits, then drop the two zero bits.
                let low = ((raw << 52) as i64) >> 52;
                Ok(insert(insn, (low >> 2) as u32, 10, 16))
            }
            Self::Call36
            | Self::Call36Relax
            | Self::TpHi20Relax
            | Self::TpAddRelax
            | Self::TpLo12Relax
            | Self::Data6
            | Self::Uleb128 => Err(Overflow),
        }
    }
}

/// Packs `value` into a `pcaddu18i` + `jirl` pair.
///
/// # Errors
///
/// [`Overflow`] when the value is not word-aligned or out of the pair's
/// ±128 GiB reach (shifted by `0x20000`, because `jirl` sign-extends its
/// part).
pub fn call36(pcaddu18i: u32, jirl: u32, value: i64) -> Result<[u32; 2], Overflow> {
    let adjusted = value.checked_add(0x2_0000).ok_or(Overflow)?;
    if value & 3 != 0 || !fits_signed(adjusted, 38) {
        return Err(Overflow);
    }
    let hi20 = bits_of(adjusted as u64, 18, 20);
    let lo16 = bits_of(value as u64, 2, 16);
    Ok([insert(pcaddu18i, hi20, 5, 20), insert(jirl, lo16, 10, 16)])
}

/// Reads the ULEB128 number at the start of `data`: its value and length.
/// A number longer than ten bytes, or truncated, is `None`.
#[must_use]
pub fn read_uleb128(data: &[u8]) -> Option<(u64, usize)> {
    let mut value = 0u64;
    for (index, byte) in data.iter().enumerate().take(10) {
        let shift = u32::try_from(index).ok()?.checked_mul(7)?;
        value |= u64::from(byte & 0x7f).checked_shl(shift).unwrap_or(0);
        if byte & 0x80 == 0 {
            return Some((value, index.checked_add(1)?));
        }
    }
    None
}

/// Re-encodes `value` into the `data.len()` bytes a ULEB128 number already
/// occupies, dropping the bits that do not fit (as lld does).
pub fn write_uleb128(data: &mut [u8], value: u64) {
    let count = data.len();
    let mut rest = value;
    for (index, byte) in data.iter_mut().enumerate() {
        let more = if index.checked_add(1) == Some(count) {
            0
        } else {
            0x80
        };
        *byte = (rest & 0x7f) as u8 | more;
        rest >>= 7;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What `pcalau12i` + `addi.d` (normal code model) compute from the
    /// fields of `delta`, and what the extreme model's `pcalau12i t0;
    /// addi.d t1, $zero; lu32i.d t1; lu52i.d t1, t1; add.d t0, t0, t1`
    /// computes.
    fn simulate(pc: u64, dest: u64) -> (u64, u64) {
        let delta = page_delta(dest, pc);
        let hi20 = (delta >> 12) & 0xf_ffff;
        let t0 = page(pc).wrapping_add((((hi20 << 44) as i64) >> 32) as u64);
        let lo12 = (((dest << 52) as i64) >> 52) as u64;
        let normal = t0.wrapping_add(lo12);
        let lo20 = (delta >> 32) & 0xf_ffff;
        let hi12 = delta >> 52;
        let mut t1 = lo12 & 0xffff_ffff;
        t1 |= ((((lo20 << 44) as i64) >> 12) as u64) & !0xffff_ffff;
        t1 = (t1 & ((1 << 52) - 1)) | (hi12 << 52);
        (normal, t0.wrapping_add(t1))
    }

    #[test]
    fn page_delta_rounds_for_the_signed_low_part() {
        // Same page, low part positive.
        assert_eq!(page_delta(0x1234, 0x1000), 0);
        // Bit 11 set: the addi subtracts, so the high part rounds up.
        assert_eq!(page_delta(0x1800, 0x1000) & 0xffff_ffff, 0x1000);
        // Backwards.
        assert_eq!(page_delta(0x1000, 0x5000) as i64 as i32, -0x4000);
        for (pc, dest) in [
            (0x1000, 0x1800),
            (0x1000, 0x17ff),
            (0x1_2000_0000, 0x1_2000_0800),
            (0x1_2000_4000, 0x1_2000_0010),
            (0x1_2000_4000, 0x1_2000_0ff0),
            (0, 0x7fff_f7fc),
        ] {
            assert_eq!(simulate(pc, dest), (dest, dest), "{pc:#x} -> {dest:#x}");
        }
        // Out of the normal model's reach, the extreme model still gets
        // there.
        for (pc, dest) in [
            (0, 0x7fff_f800),
            (0x1000, 0x12_3456_789a),
            (0x7654_3210_0000, 0x1000_0800),
            (0x1000, 0xffff_8000_0000_0800),
        ] {
            assert_eq!(simulate(pc, dest).1, dest, "{pc:#x} -> {dest:#x}");
        }
    }

    #[test]
    fn immediates_are_placed() {
        // pcalau12i $a0, 0x12345
        let insn = Field::Hi20
            .encode(ri20(PCALAU12I, R_A0, 0), 0x1234_5000)
            .unwrap();
        assert_eq!(insn, 0x1a24_68a4);
        // addi.d $a0, $a0, 0x678
        let insn = Field::Lo12
            .encode(rri12(ADDI_D, R_A0, R_A0, 0), 0x1234_5678)
            .unwrap();
        assert_eq!(insn, 0x02d9_e084);
        assert_eq!(
            Field::Lo20
                .encode(0x1600_0004, 0x000a_bcde_0000_0000)
                .unwrap()
                >> 5,
            (0x1600_0004 >> 5) | 0xabcde
        );
        assert_eq!(
            Field::Hi12
                .encode(0x0300_0084, 0x7ff0_0000_0000_0000u64 as i64)
                .unwrap()
                >> 10
                & 0xfff,
            0x7ff
        );
    }

    #[test]
    fn branches_are_checked_and_split() {
        // bl 8
        assert_eq!(Field::B26.encode(BL, 8).unwrap(), 0x5400_0800);
        // bl -4: offs26 = 0x3ffffff
        assert_eq!(Field::B26.encode(BL, -4).unwrap(), 0x57ff_ffff);
        assert_eq!(Field::B26.encode(BL, 2), Err(Overflow));
        assert_eq!(Field::B26.encode(BL, B26_REACH), Err(Overflow));
        assert!(Field::B26.encode(BL, B26_REACH - 4).is_ok());
        // beqz $a0, 0x100000 - 4
        let beqz = 0x4000_0080;
        let encoded = Field::B21.encode(beqz, 0xf_fffc).unwrap();
        assert_eq!((encoded >> 10) & 0xffff, 0xffff);
        assert_eq!(encoded & 0x1f, 0x3);
        assert_eq!(Field::B21.encode(beqz, 1 << 22), Err(Overflow));
        assert_eq!(Field::B16.encode(0x5800_0000, 1 << 17), Err(Overflow));
        assert_eq!(
            Field::Pcrel20S2.encode(PCADDI, 8).unwrap(),
            PCADDI | (2 << 5)
        );
    }

    #[test]
    fn call36_compensates_for_the_signed_jirl_offset() {
        let words = call36(ri20(PCADDU18I, R_RA, 0), jirl(R_RA, R_RA), 0x2_0000).unwrap();
        // 0x20000 = (1 << 18) - 0x20000: hi20 = 1, jirl offset = -0x20000.
        assert_eq!((words[0] >> 5) & 0xf_ffff, 1);
        assert_eq!((words[1] >> 10) & 0xffff, 0x8000);
        assert_eq!(call36(PCADDU18I, JIRL, 2), Err(Overflow));
        assert_eq!(call36(PCADDU18I, JIRL, 1 << 40), Err(Overflow));
    }

    #[test]
    fn uleb128_keeps_its_length() {
        let mut data = [0x80, 0x00];
        assert_eq!(read_uleb128(&data), Some((0, 2)));
        write_uleb128(&mut data, 0x81);
        assert_eq!(data, [0x81, 0x01]);
        assert_eq!(read_uleb128(&data), Some((0x81, 2)));
        assert_eq!(read_uleb128(&[0x80]), None);
    }
}
