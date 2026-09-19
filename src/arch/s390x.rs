//! s390x (z/Architecture) instruction encoding, big-endian.
//!
//! Everything here works on the big-endian bytes of instructions and knows
//! nothing about relocation types: the ELF backend maps a relocation to a
//! [`Field`], computes the value the ABI prescribes, and asks this module to
//! pack it. The same module holds the PLT templates and the rewrites of TLS
//! relaxation and of GOT loads turned into `larl`, which replace whole
//! instructions.
//!
//! Instructions are 2, 4 or 6 bytes long, and PC-relative operands count
//! halfwords (`*DBL` relocations: the byte offset shifted right by one), so
//! a PC-relative target must be even. The fields relocations patch:
//!
//! | Field | Bits | Used by |
//! | --- | --- | --- |
//! | [`Field::Imm12`] | low 12 bits of a halfword | `D2` of `RX`/`RS` forms (`lg %r1,sym@GOT(%r12)`) |
//! | [`Field::Disp20`] | `DL2` (12) and `DH2` (8) of a word | `RXY`/`RSY` forms (`lg`, `lay`) |
//! | [`Field::Pc12Dbl`] | low 12 bits of a halfword, in halfwords | `bprp` |
//! | [`Field::Pc16Dbl`] | a halfword, in halfwords | `brc`, `bras`, `j` |
//! | [`Field::Pc24Dbl`] | three bytes, in halfwords | `bprp`, `bpp` |
//! | [`Field::Pc32Dbl`] | a word, in halfwords | `larl`, `brasl`, `jg`, `lgrl`, `exrl` |

#![deny(clippy::arithmetic_side_effects)]

use super::Overflow;

/// Size of the PLT header (`PLT0`).
pub const PLT_HEADER_SIZE: u64 = 32;
/// Size of a PLT entry (also an IFUNC stub).
pub const PLT_ENTRY_SIZE: u64 = 32;
/// Offset in a PLT entry of the lazy-binding code (`basr %r1,%r0`), which a
/// `.got.plt` slot points at until the dynamic linker binds it.
pub const PLT_LAZY_OFFSET: u64 = 14;

/// The PLT header: saves the relocation offset the entry loaded, passes the
/// link map from the GOT and jumps to the resolver.
///
/// ```text
/// stg   %r1,56(%r15)
/// larl  %r1,_GLOBAL_OFFSET_TABLE_
/// mvc   48(8,%r15),8(%r1)
/// lg    %r1,16(%r1)
/// br    %r1
/// nopr; nopr; nopr
/// ```
pub const PLT_HEADER: [u8; 32] = [
    0xe3, 0x10, 0xf0, 0x38, 0x00, 0x24, // stg %r1,56(%r15)
    0xc0, 0x10, 0x00, 0x00, 0x00, 0x00, // larl %r1,.
    0xd2, 0x07, 0xf0, 0x30, 0x10, 0x08, // mvc 48(8,%r15),8(%r1)
    0xe3, 0x10, 0x10, 0x10, 0x00, 0x04, // lg %r1,16(%r1)
    0x07, 0xf1, // br %r1
    0x07, 0x00, 0x07, 0x00, 0x07, 0x00, // nopr (x3)
];

/// A PLT entry: jumps through its `.got.plt` slot, which initially points
/// back at the second half, where the offset of the entry's `JMP_SLOT`
/// relocation is loaded before going to the header.
///
/// ```text
/// larl  %r1,<slot>
/// lg    %r1,0(%r1)
/// br    %r1
/// basr  %r1,%r0
/// lgf   %r1,12(%r1)
/// jg    <PLT0>
/// .long <offset of the relocation in .rela.plt>
/// ```
pub const PLT_ENTRY: [u8; 32] = [
    0xc0, 0x10, 0x00, 0x00, 0x00, 0x00, // larl %r1,.
    0xe3, 0x10, 0x10, 0x00, 0x00, 0x04, // lg %r1,0(%r1)
    0x07, 0xf1, // br %r1
    0x0d, 0x10, // basr %r1,%r0
    0xe3, 0x10, 0x10, 0x0c, 0x00, 0x14, // lgf %r1,12(%r1)
    0xc0, 0xf4, 0x00, 0x00, 0x00, 0x00, // jg first plt
    0x00, 0x00, 0x00, 0x00, // .long 0
];

/// `brcl 0,.`: a six-byte no-op, which replaces the call to
/// `__tls_get_offset` of general- and local-dynamic code relaxed to
/// local-exec.
pub const BRCL_NOP: [u8; 6] = [0xc0, 0x04, 0x00, 0x00, 0x00, 0x00];

/// `lg %r2,0(%r2,%r12)`: loads the thread pointer offset from the GOT entry
/// whose GOT offset is in `%r2`, replacing the call to `__tls_get_offset`
/// of general-dynamic code relaxed to initial-exec.
pub const LG_R2_GOT: [u8; 6] = [0xe3, 0x22, 0xc0, 0x00, 0x00, 0x04];

/// The two-byte no-op (`nopr %r7`) the architecture's code fill repeats.
pub const NOPR: [u8; 2] = [0x07, 0x07];

/// A relocatable field of an instruction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    /// A 12-bit unsigned displacement in the low bits of a halfword.
    Imm12,
    /// A signed 20-bit displacement split into `DL` (bits 4..16 of the
    /// word) and `DH` (bits 16..24), of the four bytes starting at the base
    /// register.
    Disp20,
    /// A 13-bit signed byte offset, in halfwords, in the low 12 bits of a
    /// halfword.
    Pc12Dbl,
    /// A 17-bit signed byte offset, in halfwords, in a halfword.
    Pc16Dbl,
    /// A 25-bit signed byte offset, in halfwords, in three bytes.
    Pc24Dbl,
    /// A 33-bit signed byte offset, in halfwords, in a word.
    Pc32Dbl,
}

impl Field {
    /// Number of bytes the field's container occupies.
    #[must_use]
    pub const fn bytes(self) -> usize {
        match self {
            Self::Imm12 | Self::Pc12Dbl | Self::Pc16Dbl => 2,
            Self::Pc24Dbl => 3,
            Self::Disp20 | Self::Pc32Dbl => 4,
        }
    }

    /// Whether the field counts halfwords.
    #[must_use]
    pub const fn is_pc_dbl(self) -> bool {
        matches!(
            self,
            Self::Pc12Dbl | Self::Pc16Dbl | Self::Pc24Dbl | Self::Pc32Dbl
        )
    }
}

/// Whether `value` fits `bits` bits, as a signed or as an unsigned number
/// (GNU ld's `complain_overflow_bitfield`).
#[must_use]
pub fn fits_either(value: i64, bits: u32) -> bool {
    let Some(limit) = 1i64.checked_shl(bits) else {
        return true;
    };
    let half = limit >> 1;
    value >= half.wrapping_neg() && value < limit
}

/// Whether `value` fits `bits` bits as a signed number.
#[must_use]
pub fn fits_signed(value: i64, bits: u32) -> bool {
    let Some(limit) = 1i64.checked_shl(bits.saturating_sub(1)) else {
        return true;
    };
    value >= limit.wrapping_neg() && value < limit
}

/// Packs `value` into `field`, whose container is `bytes` (exactly
/// [`Field::bytes`] long), keeping the other bits.
///
/// # Errors
///
/// [`Overflow`] when the value does not fit, or when a halfword-counted
/// offset is odd (GNU ld's "misaligned symbol").
pub fn encode(field: Field, bytes: &mut [u8], value: i64) -> Result<(), Overflow> {
    if field.is_pc_dbl() && value & 1 != 0 {
        return Err(Overflow);
    }
    let halves = value >> 1;
    match field {
        Field::Imm12 | Field::Pc12Dbl => {
            let v = if field == Field::Imm12 { value } else { halves };
            if !fits_either(v, 12) {
                return Err(Overflow);
            }
            let slot = bytes.first_chunk_mut::<2>().ok_or(Overflow)?;
            let old = u16::from_be_bytes(*slot);
            *slot = ((old & 0xf000) | (v as u16 & 0x0fff)).to_be_bytes();
        }
        Field::Pc16Dbl => {
            if !fits_either(halves, 16) {
                return Err(Overflow);
            }
            let slot = bytes.first_chunk_mut::<2>().ok_or(Overflow)?;
            *slot = (halves as u16).to_be_bytes();
        }
        Field::Pc24Dbl => {
            if !fits_either(halves, 24) {
                return Err(Overflow);
            }
            let slot = bytes.first_chunk_mut::<3>().ok_or(Overflow)?;
            let [_, a, b, c] = (halves as u32).to_be_bytes();
            *slot = [a, b, c];
        }
        Field::Pc32Dbl => {
            if !fits_either(halves, 32) {
                return Err(Overflow);
            }
            let slot = bytes.first_chunk_mut::<4>().ok_or(Overflow)?;
            *slot = (halves as u32).to_be_bytes();
        }
        Field::Disp20 => {
            if !fits_signed(value, 20) {
                return Err(Overflow);
            }
            let slot = bytes.first_chunk_mut::<4>().ok_or(Overflow)?;
            let old = u32::from_be_bytes(*slot);
            let v = value as u32;
            let packed = ((v & 0xfff) << 16) | ((v & 0xf_f000) >> 4);
            *slot = ((old & 0xf000_00ff) | packed).to_be_bytes();
        }
    }
    Ok(())
}

/// Stores the halfword-counted offset from `from` to `to` into the `larl`
/// or `jg` operand at `at` of `bytes` (a [`Field::Pc32Dbl`]).
fn put_pc32dbl(bytes: &mut [u8], at: usize, from: u64, to: u64) -> Result<(), Overflow> {
    let slot = bytes.get_mut(at..).ok_or(Overflow)?;
    encode(Field::Pc32Dbl, slot, to.wrapping_sub(from) as i64)
}

/// The PLT header at `plt`, which addresses the GOT at `got` (the GOT
/// pointer, `_GLOBAL_OFFSET_TABLE_`).
///
/// # Errors
///
/// [`Overflow`] when the GOT is out of `larl`'s reach or odd.
pub fn plt_header(plt: u64, got: u64) -> Result<[u8; 32], Overflow> {
    let mut code = PLT_HEADER;
    put_pc32dbl(&mut code, 8, plt.wrapping_add(6), got)?;
    Ok(code)
}

/// A PLT entry at `entry` that jumps through the GOT word at `slot` and
/// falls back to the header at `plt`, passing `rela_offset`, the offset of
/// its relocation in `.rela.plt`.
///
/// # Errors
///
/// [`Overflow`] when the slot or the header is out of reach.
pub fn plt_entry(entry: u64, slot: u64, plt: u64, rela_offset: u32) -> Result<[u8; 32], Overflow> {
    let mut code = PLT_ENTRY;
    put_pc32dbl(&mut code, 2, entry, slot)?;
    put_pc32dbl(&mut code, 24, entry.wrapping_add(22), plt)?;
    let offset = code.get_mut(28..32).ok_or(Overflow)?;
    offset.copy_from_slice(&rela_offset.to_be_bytes());
    Ok(code)
}

/// Rewrites the initial-exec load `lg %rx,0(%ry,%r12)` (or with the index
/// and base swapped, or either of them 0) that a `R_390_TLS_LOAD` marks
/// into `sllg %rx,%ry,0`, which keeps the offset the relaxed literal pool
/// entry now holds: initial-exec relaxed to local-exec, as GNU ld does.
///
/// Returns `None` for any other instruction.
#[must_use]
pub fn ie_load_to_le(insn: [u8; 6]) -> Option<[u8; 6]> {
    let [op, rx, base, _, dh, op2] = insn;
    if op != 0xe3 || dh != 0 || op2 != 0x04 {
        return None;
    }
    // GNU ld's four cases, in its order; the displacement is not checked.
    let x2 = rx & 0x0f;
    let b2 = base >> 4;
    let ry = if b2 == 0 {
        x2
    } else if x2 == 0 {
        b2
    } else if b2 == 12 {
        x2
    } else if x2 == 12 {
        b2
    } else {
        return None;
    };
    // sllg %rx,%ry,0: RSY-a, 0xeb r1 r3 b2 d2 dh2 0x0d.
    Some([0xeb, (rx & 0xf0) | ry, 0x00, 0x00, 0x00, 0x0d])
}

/// Whether the six bytes are a `brasl %r14,…`, the call to
/// `__tls_get_offset` that TLS relaxation replaces.
#[must_use]
pub fn is_brasl_r14(insn: &[u8]) -> bool {
    insn.first_chunk::<2>() == Some(&[0xc0, 0xe5])
}

/// The opcode of `larl rx` built from the register byte of a GOT load: the
/// two bytes `0xc0, rx << 4`.
#[must_use]
pub const fn larl(register_byte: u8) -> [u8; 2] {
    [0xc0, register_byte & 0xf0]
}

/// Whether the two bytes before a `R_390_GOTENT` field are those of
/// `lgrl rx,…` (`0xc4, rx << 4 | 8`), which can become `larl`.
#[must_use]
pub fn is_lgrl(opcode: [u8; 2]) -> bool {
    opcode[0] == 0xc4 && opcode[1] & 0x0f == 0x08
}

/// Whether the six bytes starting two before a `R_390_GOT20` field are
/// `lg rx,disp(%r12)` with no index (`0xe3, rx << 4, 0xc…, …, …, 0x04`),
/// which can become `larl`.
///
/// Like GNU ld, the index register is not checked.
#[must_use]
pub fn is_lg_got(insn: &[u8]) -> bool {
    match insn.first_chunk::<6>() {
        Some(&[op, _, base, _, _, op2]) => op == 0xe3 && base & 0xf0 == 0xc0 && op2 == 0x04,
        None => false,
    }
}

/// Fills `out` with the architecture's two-byte no-ops. An odd length
/// leaves the last byte 0x07 too.
pub fn write_nops(out: &mut [u8]) {
    out.fill(NOPR[0]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields_pack_big_endian() {
        let mut word = [0xc0, 0x10, 0, 0, 0, 0];
        encode(Field::Pc32Dbl, &mut word[2..], 0x1000).unwrap();
        assert_eq!(word, [0xc0, 0x10, 0, 0, 0x08, 0]);
        encode(Field::Pc32Dbl, &mut word[2..], -4).unwrap();
        assert_eq!(word[2..], [0xff, 0xff, 0xff, 0xfe]);
        assert!(encode(Field::Pc32Dbl, &mut word[2..], 3).is_err());
        assert!(encode(Field::Pc32Dbl, &mut word[2..], 1 << 34).is_err());

        let mut half = [0xa7, 0xf4];
        encode(Field::Pc16Dbl, &mut half, -2).unwrap();
        assert_eq!(half, [0xff, 0xff]);

        // lg %r1,0(%r12) with a 20-bit displacement of 0x12345.
        let mut rxy = [0xc0, 0x00, 0x00, 0x04];
        encode(Field::Disp20, &mut rxy, 0x1_2345).unwrap();
        assert_eq!(rxy, [0xc3, 0x45, 0x12, 0x04]);
        encode(Field::Disp20, &mut rxy, -1).unwrap();
        assert_eq!(rxy, [0xcf, 0xff, 0xff, 0x04]);
        assert!(encode(Field::Disp20, &mut rxy, 0x8_0000).is_err());

        let mut d12 = [0xc0, 0x00];
        encode(Field::Imm12, &mut d12, 0xabc).unwrap();
        assert_eq!(d12, [0xca, 0xbc]);
        assert!(encode(Field::Imm12, &mut d12, 0x1000).is_err());

        let mut three = [0; 3];
        encode(Field::Pc24Dbl, &mut three, 0x10).unwrap();
        assert_eq!(three, [0, 0, 8]);
        let mut short = [0u8; 1];
        assert!(encode(Field::Pc32Dbl, &mut short, 0).is_err());
    }

    #[test]
    fn plt_matches_gnu_ld() {
        // GNU ld's __cxa_finalize@plt at 0x5e8, slot 0x1fc0, PLT0 at 0x5c8.
        let entry = plt_entry(0x5e8, 0x1fc0, 0x5c8, 0).unwrap();
        assert_eq!(
            entry,
            [
                0xc0, 0x10, 0x00, 0x00, 0x0c, 0xec, 0xe3, 0x10, 0x10, 0x00, 0x00, 0x04, 0x07, 0xf1,
                0x0d, 0x10, 0xe3, 0x10, 0x10, 0x0c, 0x00, 0x14, 0xc0, 0xf4, 0xff, 0xff, 0xff, 0xe5,
                0x00, 0x00, 0x00, 0x00
            ]
        );
        let header = plt_header(0x5c8, 0x1fa8).unwrap();
        assert_eq!(header[6..12], [0xc0, 0x10, 0x00, 0x00, 0x0c, 0xed]);
    }

    #[test]
    fn tls_rewrites() {
        // lg %r1,0(%r1,%r12) -> sllg %r1,%r1,0
        assert_eq!(
            ie_load_to_le([0xe3, 0x11, 0xc0, 0x00, 0x00, 0x04]),
            Some([0xeb, 0x11, 0x00, 0x00, 0x00, 0x0d])
        );
        // lg %r3,0(%r12,%r5) -> sllg %r3,%r5,0
        assert_eq!(
            ie_load_to_le([0xe3, 0x3c, 0x50, 0x00, 0x00, 0x04]),
            Some([0xeb, 0x35, 0x00, 0x00, 0x00, 0x0d])
        );
        assert_eq!(ie_load_to_le([0xe3, 0x11, 0xc0, 0x08, 0x01, 0x04]), None);
        assert_eq!(ie_load_to_le([0xe3, 0x13, 0x50, 0x00, 0x00, 0x04]), None);
        assert!(is_brasl_r14(&[0xc0, 0xe5, 0, 0, 0, 0]));
        assert!(is_lgrl([0xc4, 0x28]));
        assert!(!is_lgrl([0xc4, 0x2d]));
        assert!(is_lg_got(&[0xe3, 0x10, 0xc0, 0x10, 0x00, 0x04]));
        assert_eq!(larl(0x28), [0xc0, 0x20]);
    }
}
