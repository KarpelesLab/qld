//! RISC-V instruction encoding.
//!
//! Everything here works on little-endian bytes and knows nothing about
//! relocation types: the ELF backend maps a relocation to a [`Field`],
//! computes the value the psABI prescribes, and asks this module to pack it.
//! The same encodings build the PLT and the instructions linker relaxation
//! rewrites.
//!
//! Naming follows the RISC-V psABI: `%hi(x)` is `(x + 0x800) >> 12`, the
//! upper 20 bits a `lui`/`auipc` adds, rounded so that the sign-extended
//! `%lo(x)` of the paired instruction completes the value.

#![deny(clippy::arithmetic_side_effects)]

use super::Overflow;

/// `addi x0, x0, 0`.
pub const NOP: u32 = 0x0000_0013;
/// `c.nop`.
pub const C_NOP: u16 = 0x0001;
/// `c.j` with a zero offset.
pub const C_J: u16 = 0xa001;
/// `jal` with a zero offset and `rd` = x0.
pub const JAL: u32 = 0x0000_006f;

/// Opcode of `addi`.
pub const ADDI: u32 = 0x13;
/// Opcode of `auipc`.
pub const AUIPC: u32 = 0x17;
/// Opcode of `jalr`.
pub const JALR: u32 = 0x67;
/// Opcode and function of `ld`.
pub const LD: u32 = 0x3003;
/// Opcode of `lui`.
pub const LUI: u32 = 0x37;
/// Opcode and function of `srli`.
pub const SRLI: u32 = 0x5013;
/// Opcode and function of `sub`.
pub const SUB: u32 = 0x4000_0033;

/// The zero register.
pub const X0: u32 = 0;
/// The return address register.
pub const RA: u32 = 1;
/// The global pointer.
pub const GP: u32 = 3;
/// The thread pointer.
pub const TP: u32 = 4;
/// Temporary `t0`.
pub const T0: u32 = 5;
/// Temporary `t1`.
pub const T1: u32 = 6;
/// Temporary `t2`.
pub const T2: u32 = 7;
/// Argument register `a0`.
pub const A0: u32 = 10;
/// Temporary `t3`.
pub const T3: u32 = 28;

/// The offset glibc and musl add to a `DTPREL` value: `__tls_get_addr`
/// returns the module's block plus the offset plus 0x800, so offsets are
/// stored minus 0x800 to use the whole signed 12-bit range.
pub const DTP_OFFSET: u64 = 0x800;

/// `%hi(value)`: the 20 bits `lui`/`auipc` load, rounded for the signed low
/// part.
#[must_use]
pub const fn hi20(value: u64) -> u32 {
    (value.wrapping_add(0x800) >> 12) as u32 & 0xf_ffff
}

/// `%lo(value)`: the low 12 bits, which the instruction sign-extends.
#[must_use]
pub const fn lo12(value: u64) -> u32 {
    value as u32 & 0xfff
}

/// Whether `value` fits `bits` bits as a signed number.
#[must_use]
pub fn fits_signed(value: i64, bits: u32) -> bool {
    if bits >= 64 {
        return true;
    }
    let Some(limit) = 1i64.checked_shl(bits.saturating_sub(1)) else {
        return true;
    };
    value >= limit.wrapping_neg() && value < limit
}

/// An I-type instruction.
#[must_use]
pub const fn itype(op: u32, rd: u32, rs1: u32, imm: u32) -> u32 {
    op | ((rd & 31) << 7) | ((rs1 & 31) << 15) | ((imm & 0xfff) << 20)
}

/// An R-type instruction.
#[must_use]
pub const fn rtype(op: u32, rd: u32, rs1: u32, rs2: u32) -> u32 {
    op | ((rd & 31) << 7) | ((rs1 & 31) << 15) | ((rs2 & 31) << 20)
}

/// A U-type instruction; `imm` holds the upper 20 bits.
#[must_use]
pub const fn utype(op: u32, rd: u32, imm: u32) -> u32 {
    op | ((rd & 31) << 7) | ((imm & 0xf_ffff) << 12)
}

/// The `rd` field of a 32-bit instruction.
#[must_use]
pub const fn rd(insn: u32) -> u32 {
    (insn >> 7) & 31
}

/// Replaces the `rs1` field of a 32-bit instruction.
#[must_use]
pub const fn with_rs1(insn: u32, rs1: u32) -> u32 {
    (insn & !(31 << 15)) | ((rs1 & 31) << 15)
}

/// Sets the 12-bit immediate of an I-type instruction.
#[must_use]
pub const fn set_lo12_i(insn: u32, imm: u32) -> u32 {
    (insn & 0xf_ffff) | ((imm & 0xfff) << 20)
}

/// Sets the 12-bit immediate of an S-type instruction.
#[must_use]
pub const fn set_lo12_s(insn: u32, imm: u32) -> u32 {
    (insn & 0x01ff_f07f) | (((imm >> 5) & 0x7f) << 25) | ((imm & 0x1f) << 7)
}

/// Bits `high..=low` of `value`, shifted down.
const fn bits(value: u64, high: u32, low: u32) -> u32 {
    let width = high.wrapping_sub(low).wrapping_add(1);
    ((value >> low) & (1u64 << width).wrapping_sub(1)) as u32
}

/// Reads the 32-bit word at `at`.
#[must_use]
pub fn read32(data: &[u8], at: usize) -> Option<u32> {
    data.get(at..)
        .and_then(|rest| rest.first_chunk::<4>())
        .map(|word| u32::from_le_bytes(*word))
}

/// Writes the 32-bit word `value` at `at`.
pub fn write32(data: &mut [u8], at: usize, value: u32) -> Option<()> {
    let slot = data
        .get_mut(at..)
        .and_then(|rest| rest.first_chunk_mut::<4>())?;
    *slot = value.to_le_bytes();
    Some(())
}

/// Reads the 16-bit word at `at`.
#[must_use]
pub fn read16(data: &[u8], at: usize) -> Option<u16> {
    data.get(at..)
        .and_then(|rest| rest.first_chunk::<2>())
        .map(|word| u16::from_le_bytes(*word))
}

/// Writes the 16-bit word `value` at `at`.
pub fn write16(data: &mut [u8], at: usize, value: u16) -> Option<()> {
    let slot = data
        .get_mut(at..)
        .and_then(|rest| rest.first_chunk_mut::<2>())?;
    *slot = value.to_le_bytes();
    Some(())
}

/// Why a field could not be written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldError {
    /// The value does not fit, or is misaligned for a branch.
    Overflow,
    /// The field extends past the end of the section.
    OutOfBounds,
}

impl From<Overflow> for FieldError {
    fn from(_: Overflow) -> Self {
        Self::Overflow
    }
}

/// A relocatable field: data, an instruction immediate, or a field whose
/// contents the relocation combines with (the label arithmetic of
/// `R_RISCV_ADD*`/`SUB*`/`SET*`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    /// 32 bits of data, unchecked (`R_RISCV_32`).
    Word32,
    /// 32 bits of data, checked as signed (`32_PCREL`, `PLT32`, `SET32`).
    Word32Signed,
    /// 64 bits of data.
    Word64,
    /// `%hi` of the value in a `lui`/`auipc`, checked to fit.
    Hi20,
    /// `%lo` of the value in an I-type immediate.
    Lo12I,
    /// `%lo` of the value in an S-type immediate.
    Lo12S,
    /// The whole value in an I-type immediate with `rs1` set to x0.
    X0RelI,
    /// The whole value in an S-type immediate with `rs1` set to x0.
    X0RelS,
    /// The whole value in an I-type immediate with `rs1` set to `gp`.
    GpRelI,
    /// The whole value in an S-type immediate with `rs1` set to `gp`.
    GpRelS,
    /// A 13-bit conditional branch offset (`beq`, …).
    Branch,
    /// A 21-bit `jal` offset.
    Jal,
    /// A 9-bit `c.beqz`/`c.bnez` offset.
    RvcBranch,
    /// A 12-bit `c.j`/`c.jal` offset.
    RvcJump,
    /// An `auipc` + `jalr` pair (eight bytes).
    Call,
    /// Adds the value to 8 bits of data.
    Add8,
    /// Adds the value to 16 bits of data.
    Add16,
    /// Adds the value to 32 bits of data.
    Add32,
    /// Adds the value to 64 bits of data.
    Add64,
    /// Subtracts the value from the low 6 bits of a byte.
    Sub6,
    /// Subtracts the value from 8 bits of data.
    Sub8,
    /// Subtracts the value from 16 bits of data.
    Sub16,
    /// Subtracts the value from 32 bits of data.
    Sub32,
    /// Subtracts the value from 64 bits of data.
    Sub64,
    /// Sets the low 6 bits of a byte.
    Set6,
    /// Sets 8 bits of data.
    Set8,
    /// Sets 16 bits of data.
    Set16,
    /// Sets 32 bits of data.
    Set32,
    /// Overwrites a ULEB128 number, keeping its length.
    SetUleb128,
    /// Subtracts the value from a ULEB128 number, keeping its length.
    SubUleb128,
    /// 32 bits holding the value minus [`DTP_OFFSET`].
    Dtprel32,
    /// 64 bits holding the value minus [`DTP_OFFSET`].
    Dtprel64,
}

/// Rewrites the ULEB128 number at the start of `data` with `value`, keeping
/// its encoded length. Returns whether the value fit.
fn overwrite_uleb128(data: &mut [u8], value: u64) -> Result<bool, FieldError> {
    let mut rest = value;
    let mut index = 0usize;
    loop {
        let byte = data.get_mut(index).ok_or(FieldError::OutOfBounds)?;
        let more = *byte & 0x80 != 0;
        let low = (rest & 0x7f) as u8;
        rest >>= 7;
        *byte = if more { low | 0x80 } else { low };
        if !more {
            return Ok(rest == 0);
        }
        index = index.checked_add(1).ok_or(FieldError::OutOfBounds)?;
    }
}

/// Reads the ULEB128 number at the start of `data` (wrapping past 64 bits).
fn read_uleb128(data: &[u8]) -> Result<u64, FieldError> {
    let mut value = 0u64;
    let mut shift = 0u32;
    for &byte in data {
        if shift < 64 {
            value |= u64::from(byte & 0x7f).checked_shl(shift).unwrap_or(0);
        }
        if byte & 0x80 == 0 {
            return Ok(value);
        }
        shift = shift.saturating_add(7);
    }
    Err(FieldError::OutOfBounds)
}

impl Field {
    /// The number of bytes the field occupies (the maximum for ULEB128).
    #[must_use]
    pub const fn bytes(self) -> usize {
        match self {
            Self::Add8 | Self::Sub6 | Self::Sub8 | Self::Set6 | Self::Set8 => 1,
            Self::RvcBranch | Self::RvcJump | Self::Add16 | Self::Sub16 | Self::Set16 => 2,
            Self::Word64
            | Self::Add64
            | Self::Sub64
            | Self::Dtprel64
            | Self::Call
            | Self::SetUleb128
            | Self::SubUleb128 => 8,
            _ => 4,
        }
    }

    /// Whether the field is plain data that a dynamic relocation could
    /// supply instead (`R_RISCV_32`, `R_RISCV_64`).
    #[must_use]
    pub const fn is_data(self) -> bool {
        matches!(self, Self::Word32 | Self::Word64)
    }

    /// Whether the field takes part in label arithmetic: a difference of
    /// two addresses that is final at link time and never becomes a dynamic
    /// relocation.
    #[must_use]
    pub const fn is_label_math(self) -> bool {
        matches!(
            self,
            Self::Add8
                | Self::Add16
                | Self::Add32
                | Self::Add64
                | Self::Sub6
                | Self::Sub8
                | Self::Sub16
                | Self::Sub32
                | Self::Sub64
                | Self::Set6
                | Self::Set8
                | Self::Set16
                | Self::Set32
                | Self::SetUleb128
                | Self::SubUleb128
        )
    }

    /// Packs `value` into the field at the start of `data`.
    ///
    /// # Errors
    ///
    /// [`FieldError::Overflow`] when the value does not fit (or a branch
    /// target is misaligned), [`FieldError::OutOfBounds`] when `data` is
    /// too short.
    #[allow(clippy::too_many_lines)]
    pub fn apply(self, data: &mut [u8], value: u64) -> Result<(), FieldError> {
        let signed = value as i64;
        let check = |bits: u32| {
            if fits_signed(signed, bits) {
                Ok(())
            } else {
                Err(FieldError::Overflow)
            }
        };
        let aligned = || {
            if value & 1 == 0 {
                Ok(())
            } else {
                Err(FieldError::Overflow)
            }
        };
        let oob = FieldError::OutOfBounds;
        let get32 = |data: &[u8]| read32(data, 0).ok_or(oob);
        let get16 = |data: &[u8]| read16(data, 0).ok_or(oob);
        let put32 = |data: &mut [u8], v: u32| write32(data, 0, v).ok_or(oob);
        let put16 = |data: &mut [u8], v: u16| write16(data, 0, v).ok_or(oob);
        let byte = |data: &mut [u8]| -> Result<u8, FieldError> { data.first().copied().ok_or(oob) };
        let put8 = |data: &mut [u8], v: u8| -> Result<(), FieldError> {
            *data.first_mut().ok_or(oob)? = v;
            Ok(())
        };
        let get64 = |data: &[u8]| {
            data.first_chunk::<8>()
                .map(|w| u64::from_le_bytes(*w))
                .ok_or(oob)
        };
        let put64 = |data: &mut [u8], v: u64| -> Result<(), FieldError> {
            *data.first_chunk_mut::<8>().ok_or(oob)? = v.to_le_bytes();
            Ok(())
        };
        // `(value + 0x800) >> 12`, sign-extended, must fit 20 bits.
        let hi_fits = || check_hi(value);
        match self {
            Self::Word32 => put32(data, value as u32),
            Self::Word32Signed | Self::Set32 => {
                check(32)?;
                put32(data, value as u32)
            }
            Self::Word64 => put64(data, value),
            Self::Hi20 => {
                hi_fits()?;
                let insn = get32(data)?;
                put32(data, (insn & 0xfff) | (hi20(value) << 12))
            }
            Self::Lo12I => {
                let insn = get32(data)?;
                put32(data, set_lo12_i(insn, lo12(value)))
            }
            Self::Lo12S => {
                let insn = get32(data)?;
                put32(data, set_lo12_s(insn, lo12(value)))
            }
            Self::X0RelI | Self::X0RelS | Self::GpRelI | Self::GpRelS => {
                check(12)?;
                let base = if matches!(self, Self::X0RelI | Self::X0RelS) {
                    X0
                } else {
                    GP
                };
                let insn = with_rs1(get32(data)?, base);
                let insn = if matches!(self, Self::X0RelI | Self::GpRelI) {
                    set_lo12_i(insn, value as u32)
                } else {
                    set_lo12_s(insn, value as u32)
                };
                put32(data, insn)
            }
            Self::Branch => {
                check(13)?;
                aligned()?;
                let insn = get32(data)? & 0x01ff_f07f;
                put32(
                    data,
                    insn | (bits(value, 12, 12) << 31)
                        | (bits(value, 10, 5) << 25)
                        | (bits(value, 4, 1) << 8)
                        | (bits(value, 11, 11) << 7),
                )
            }
            Self::Jal => {
                check(21)?;
                aligned()?;
                let insn = get32(data)? & 0xfff;
                put32(
                    data,
                    insn | (bits(value, 20, 20) << 31)
                        | (bits(value, 10, 1) << 21)
                        | (bits(value, 11, 11) << 20)
                        | (bits(value, 19, 12) << 12),
                )
            }
            Self::RvcBranch => {
                check(9)?;
                aligned()?;
                let insn = u32::from(get16(data)? & 0xe383);
                let insn = insn
                    | (bits(value, 8, 8) << 12)
                    | (bits(value, 4, 3) << 10)
                    | (bits(value, 7, 6) << 5)
                    | (bits(value, 2, 1) << 3)
                    | (bits(value, 5, 5) << 2);
                put16(data, insn as u16)
            }
            Self::RvcJump => {
                check(12)?;
                aligned()?;
                let insn = u32::from(get16(data)? & 0xe003);
                let insn = insn
                    | (bits(value, 11, 11) << 12)
                    | (bits(value, 4, 4) << 11)
                    | (bits(value, 9, 8) << 9)
                    | (bits(value, 10, 10) << 8)
                    | (bits(value, 6, 6) << 7)
                    | (bits(value, 7, 7) << 6)
                    | (bits(value, 3, 1) << 3)
                    | (bits(value, 5, 5) << 2);
                put16(data, insn as u16)
            }
            Self::Call => {
                hi_fits()?;
                let auipc = get32(data)?;
                let jalr = read32(data, 4).ok_or(oob)?;
                put32(data, (auipc & 0xfff) | (hi20(value) << 12))?;
                write32(data, 4, set_lo12_i(jalr, lo12(value))).ok_or(oob)
            }
            Self::Add8 => {
                let v = byte(data)?.wrapping_add(value as u8);
                put8(data, v)
            }
            Self::Add16 => {
                let v = get16(data)?.wrapping_add(value as u16);
                put16(data, v)
            }
            Self::Add32 => {
                let v = get32(data)?.wrapping_add(value as u32);
                put32(data, v)
            }
            Self::Add64 => {
                let v = get64(data)?.wrapping_add(value);
                put64(data, v)
            }
            Self::Sub6 => {
                let old = byte(data)?;
                put8(
                    data,
                    (old & 0xc0) | ((old & 0x3f).wrapping_sub(value as u8) & 0x3f),
                )
            }
            Self::Sub8 => {
                let v = byte(data)?.wrapping_sub(value as u8);
                put8(data, v)
            }
            Self::Sub16 => {
                let v = get16(data)?.wrapping_sub(value as u16);
                put16(data, v)
            }
            Self::Sub32 => {
                let v = get32(data)?.wrapping_sub(value as u32);
                put32(data, v)
            }
            Self::Sub64 => {
                let v = get64(data)?.wrapping_sub(value);
                put64(data, v)
            }
            Self::Set6 => {
                let old = byte(data)?;
                put8(data, (old & 0xc0) | (value as u8 & 0x3f))
            }
            Self::Set8 => put8(data, value as u8),
            Self::Set16 => put16(data, value as u16),
            Self::SetUleb128 => {
                if overwrite_uleb128(data, value)? {
                    Ok(())
                } else {
                    Err(FieldError::Overflow)
                }
            }
            Self::SubUleb128 => {
                let old = read_uleb128(data)?;
                if overwrite_uleb128(data, old.wrapping_sub(value))? {
                    Ok(())
                } else {
                    Err(FieldError::Overflow)
                }
            }
            Self::Dtprel32 => put32(data, value.wrapping_sub(DTP_OFFSET) as u32),
            Self::Dtprel64 => put64(data, value.wrapping_sub(DTP_OFFSET)),
        }
    }
}

/// Whether `%hi(value)` fits the 20-bit `lui`/`auipc` immediate, as a
/// signed number.
///
/// # Errors
///
/// [`FieldError::Overflow`] when it does not.
pub fn check_hi(value: u64) -> Result<(), FieldError> {
    let hi = (value.wrapping_add(0x800) as i64) >> 12;
    if fits_signed(hi, 20) {
        Ok(())
    } else {
        Err(FieldError::Overflow)
    }
}

/// Writes the ULEB128 value `value` over the number at the start of
/// `data`, keeping its length.
///
/// # Errors
///
/// [`FieldError::Overflow`] when the value needs more bytes than the
/// number has.
pub fn write_uleb128(data: &mut [u8], value: u64) -> Result<(), FieldError> {
    if overwrite_uleb128(data, value)? {
        Ok(())
    } else {
        Err(FieldError::Overflow)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word(field: Field, insn: u32, value: u64) -> u32 {
        let mut data = insn.to_le_bytes().to_vec();
        data.extend_from_slice(&[0; 4]);
        field.apply(&mut data, value).unwrap();
        read32(&data, 0).unwrap()
    }

    #[test]
    fn hi_lo_split_rounds_the_upper_part() {
        assert_eq!(hi20(0x1800), 2);
        assert_eq!(lo12(0x1800), 0x800);
        assert_eq!(hi20(0x17ff), 1);
        // lui a0, %hi(0x12345678)
        assert_eq!(word(Field::Hi20, 0x0000_0537, 0x1234_5678), 0x1234_5537);
        // addi a0, a0, %lo(0x12345678)
        assert_eq!(word(Field::Lo12I, 0x0005_0513, 0x1234_5678), 0x6785_0513);
        // sw a1, %lo(0x12345678)(a0)
        assert_eq!(word(Field::Lo12S, 0x00b5_2023, 0x1234_5678), 0x66b5_2c23);
    }

    #[test]
    fn branches_encode_their_offsets() {
        // jal ra, +0x800
        assert_eq!(word(Field::Jal, 0x0000_00ef, 0x800), 0x0010_00ef);
        // beq a0, a1, -4
        assert_eq!(
            word(Field::Branch, 0x00b5_0063, (-4i64) as u64),
            0xfeb5_0ee3
        );
        let mut data = C_J.to_le_bytes().to_vec();
        Field::RvcJump.apply(&mut data, 0x7fe).unwrap();
        assert_eq!(read16(&data, 0), Some(0xaffd));
        assert_eq!(
            Field::Jal.apply(&mut [0; 4], 1 << 20),
            Err(FieldError::Overflow)
        );
        assert_eq!(Field::Jal.apply(&mut [0; 4], 3), Err(FieldError::Overflow));
    }

    #[test]
    fn call_patches_auipc_and_jalr() {
        // auipc ra, 0; jalr ra, 0(ra)
        let mut data = [0x97, 0x00, 0x00, 0x00, 0xe7, 0x80, 0x00, 0x00];
        Field::Call.apply(&mut data, 0x1_2800).unwrap();
        assert_eq!(read32(&data, 0), Some(0x0001_3097));
        assert_eq!(read32(&data, 4), Some(0x8000_80e7));
    }

    #[test]
    fn label_arithmetic_combines_with_the_contents() {
        let mut data = 10u32.to_le_bytes();
        Field::Add32.apply(&mut data, 0x1000).unwrap();
        Field::Sub32.apply(&mut data, 0x0f00).unwrap();
        assert_eq!(u32::from_le_bytes(data), 0x10a);
        let mut byte = [0x40 | 0x3f];
        Field::Set6.apply(&mut byte, 0x5).unwrap();
        assert_eq!(byte, [0x45]);
        Field::Sub6.apply(&mut byte, 0x6).unwrap();
        assert_eq!(byte, [0x40 | 0x3f]);
    }

    #[test]
    fn uleb128_keeps_its_length() {
        let mut data = [0x80, 0x80, 0x00, 0xff];
        Field::SetUleb128.apply(&mut data, 0x1234).unwrap();
        assert_eq!(data, [0xb4, 0xa4, 0x00, 0xff]);
        Field::SubUleb128.apply(&mut data, 0x1200).unwrap();
        assert_eq!(data, [0xb4, 0x80, 0x00, 0xff]);
        let mut short = [0x00];
        assert_eq!(
            Field::SetUleb128.apply(&mut short, 0x80),
            Err(FieldError::Overflow)
        );
        assert_eq!(
            Field::SetUleb128.apply(&mut [0x80], 1),
            Err(FieldError::OutOfBounds)
        );
    }
}
