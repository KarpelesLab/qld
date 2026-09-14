//! x86-64 relocations for static executables: classification, relaxation
//! and application.
//!
//! [`classify`] turns a relocation type (plus the instruction bytes in front
//! of it, for relaxations) into a [`Kind`]: what value to compute and which
//! instruction rewrite, if any, applies. The relocation scan uses it to find
//! GOT and IFUNC needs; the writer uses the same answer to patch the output,
//! so both always agree. Relaxations follow the psABI and lld:
//!
//! - `GOTPCRELX`/`REX_GOTPCRELX` with addend −4: `mov` → `lea`,
//!   `call *` → `addr32 call`, `jmp *` → `jmp; nop`, and (REX only) `test`
//!   and binary operators → immediate forms;
//! - TLS general-dynamic, local-dynamic, initial-exec and TLS descriptors →
//!   local-exec, since a static executable has exactly one TLS block at a
//!   known offset from the thread pointer.

#![deny(clippy::arithmetic_side_effects)]

use crate::elf::read::consts::x86_64::*;

/// What a relocation computes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Nothing to do.
    None,
    /// `S + A`.
    Abs,
    /// `S + A - P`.
    Pc,
    /// `GOT entry + A - P`: needs a GOT entry for the symbol.
    GotPc,
    /// Relaxed `GOTPCRELX`: `S + A - P`, with the instruction rewritten.
    RelaxGotPc,
    /// Relaxed `REX_GOTPCRELX` to an immediate operand: `S`.
    RelaxGotPcNoPic,
    /// `GOT entry - GOT base + A` (`GOT32`, `GOT64`): needs a GOT entry.
    GotEntry,
    /// `S + A - GOT base` (`GOTOFF64`, `PLTOFF64`).
    GotRel,
    /// `GOT base + A - P` (`GOTPC32`, `GOTPC64`).
    GotBasePc,
    /// `Z + A`.
    Size,
    /// `S + A - TP`.
    TpOff,
    /// `S + A - TLS block start` in non-allocated sections, `S + A - TP` in
    /// allocated ones (local-dynamic code relaxed to local-exec).
    DtpOff,
    /// General-dynamic → local-exec; the next relocation is consumed.
    GdToLe,
    /// Local-dynamic → local-exec; the next relocation is consumed.
    LdToLe,
    /// Initial-exec → local-exec.
    IeToLe,
    /// TLS descriptor → local-exec.
    DescToLe,
    /// TLS descriptor call → `nop`.
    DescCallToLe,
}

impl Kind {
    /// Whether the relocation needs a GOT entry for its symbol.
    #[must_use]
    pub fn needs_got(self) -> bool {
        matches!(self, Self::GotPc | Self::GotEntry)
    }

    /// Whether the relocation uses the GOT base (so `_GLOBAL_OFFSET_TABLE_`
    /// must exist).
    #[must_use]
    pub fn uses_got_base(self) -> bool {
        matches!(self, Self::GotEntry | Self::GotRel | Self::GotBasePc)
    }

    /// Whether the relocation consumes the relocation that follows it (the
    /// call to `__tls_get_addr`).
    #[must_use]
    pub fn skips_next(self) -> bool {
        matches!(self, Self::GdToLe | Self::LdToLe)
    }

    /// Whether the relocation is a TLS access.
    #[must_use]
    pub fn is_tls(self) -> bool {
        matches!(
            self,
            Self::TpOff
                | Self::DtpOff
                | Self::GdToLe
                | Self::LdToLe
                | Self::IeToLe
                | Self::DescToLe
                | Self::DescCallToLe
        )
    }
}

/// How the computed value is stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Width {
    /// Nothing is written.
    None,
    /// 8 bytes.
    W64,
    /// 4 bytes, zero-extended by the processor.
    U32,
    /// 4 bytes, sign-extended.
    I32,
    /// 2 bytes, signed or unsigned.
    Any16,
    /// 2 bytes, signed.
    I16,
    /// 1 byte, signed or unsigned.
    Any8,
    /// 1 byte, signed.
    I8,
}

/// A classified relocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Class {
    /// What to compute.
    pub kind: Kind,
    /// How to store it.
    pub width: Width,
}

/// Why a relocation cannot be handled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClassifyError {
    /// The type is unknown or not valid in a relocatable object.
    Unsupported,
    /// A TLS relaxation found an instruction it cannot rewrite.
    BadTlsInstruction,
}

const fn class(kind: Kind, width: Width) -> Class {
    Class { kind, width }
}

/// Reads byte `offset - back` of `data`, if it exists.
fn byte_before(data: &[u8], offset: u64, back: u64) -> Option<u8> {
    let at = usize::try_from(offset.checked_sub(back)?).ok()?;
    data.get(at).copied()
}

fn byte_after(data: &[u8], offset: u64, forward: u64) -> Option<u8> {
    let at = usize::try_from(offset.checked_add(forward)?).ok()?;
    data.get(at).copied()
}

/// Classifies relocation `r_type` at `offset` in section `data`.
///
/// `relax_got` says whether GOT-indirect accesses to the symbol may be
/// rewritten into direct ones (the symbol is not an IFUNC and `--no-relax`
/// was not given). TLS accesses are always relaxed: a static executable
/// cannot do anything else.
///
/// # Errors
///
/// [`ClassifyError`] for unsupported types and unrecognized TLS code.
pub fn classify(
    r_type: u32,
    addend: i64,
    data: &[u8],
    offset: u64,
    relax_got: bool,
) -> Result<Class, ClassifyError> {
    use Kind as K;
    use Width as W;
    Ok(match r_type {
        R_X86_64_NONE | R_X86_64_GNU_VTINHERIT | R_X86_64_GNU_VTENTRY => class(K::None, W::None),
        R_X86_64_64 => class(K::Abs, W::W64),
        R_X86_64_32 => class(K::Abs, W::U32),
        R_X86_64_32S => class(K::Abs, W::I32),
        R_X86_64_16 => class(K::Abs, W::Any16),
        R_X86_64_8 => class(K::Abs, W::Any8),
        R_X86_64_PC64 => class(K::Pc, W::W64),
        R_X86_64_PC32 | R_X86_64_PLT32 => class(K::Pc, W::I32),
        R_X86_64_PC16 => class(K::Pc, W::I16),
        R_X86_64_PC8 => class(K::Pc, W::I8),
        R_X86_64_GOTPCREL => class(K::GotPc, W::I32),
        R_X86_64_GOTPCREL64 => class(K::GotPc, W::W64),
        R_X86_64_GOTPCRELX | R_X86_64_REX_GOTPCRELX => {
            let op = byte_before(data, offset, 2);
            let modrm = byte_before(data, offset, 1);
            let relaxable = relax_got && addend == -4;
            match (op, modrm) {
                (Some(0x8b), _) if relaxable => class(K::RelaxGotPc, W::I32),
                (Some(0xff), Some(0x15 | 0x25)) if relaxable => class(K::RelaxGotPc, W::I32),
                (Some(0x85 | 0x03 | 0x0b | 0x13 | 0x1b | 0x23 | 0x2b | 0x33 | 0x3b), _)
                    if relaxable
                        && r_type == R_X86_64_REX_GOTPCRELX
                        && byte_before(data, offset, 3).is_some() =>
                {
                    class(K::RelaxGotPcNoPic, W::I32)
                }
                _ => class(K::GotPc, W::I32),
            }
        }
        R_X86_64_GOT32 => class(K::GotEntry, W::I32),
        R_X86_64_GOT64 | R_X86_64_GOTPLT64 => class(K::GotEntry, W::W64),
        R_X86_64_GOTOFF64 | R_X86_64_PLTOFF64 => class(K::GotRel, W::W64),
        R_X86_64_GOTPC32 => class(K::GotBasePc, W::I32),
        R_X86_64_GOTPC64 => class(K::GotBasePc, W::W64),
        R_X86_64_SIZE32 => class(K::Size, W::U32),
        R_X86_64_SIZE64 => class(K::Size, W::W64),
        R_X86_64_TPOFF32 => class(K::TpOff, W::I32),
        R_X86_64_TPOFF64 => class(K::TpOff, W::W64),
        R_X86_64_DTPOFF32 => class(K::DtpOff, W::I32),
        R_X86_64_DTPOFF64 => class(K::DtpOff, W::W64),
        R_X86_64_TLSGD => {
            // 66 48 8d 3d <x@tlsgd>, then 66 66 48 e8 <__tls_get_addr@plt>
            // or 66 48 ff 15 <__tls_get_addr@gotpcrel>.
            let call = (byte_after(data, offset, 6), byte_after(data, offset, 7));
            let lea = byte_before(data, offset, 4);
            match (lea, call) {
                (Some(0x66), (Some(0x48), Some(0xe8)) | (Some(0xff), Some(0x15))) => {
                    class(K::GdToLe, W::None)
                }
                _ => return Err(ClassifyError::BadTlsInstruction),
            }
        }
        R_X86_64_TLSLD => {
            let after = (byte_after(data, offset, 4), byte_after(data, offset, 5));
            match after {
                (Some(0xe8), _) | (Some(0xff), Some(0x15)) => class(K::LdToLe, W::None),
                _ => return Err(ClassifyError::BadTlsInstruction),
            }
        }
        R_X86_64_GOTTPOFF => class(K::IeToLe, W::None),
        R_X86_64_GOTPC32_TLSDESC => class(K::DescToLe, W::None),
        R_X86_64_TLSDESC_CALL => class(K::DescCallToLe, W::None),
        _ => return Err(ClassifyError::Unsupported),
    })
}

/// Why a relocation could not be written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApplyError {
    /// The value does not fit the field.
    Overflow,
    /// The field lies outside the section.
    OutOfBounds,
    /// A relaxation found an instruction it cannot rewrite.
    BadInstruction,
}

fn slot<const N: usize>(out: &mut [u8], at: u64) -> Result<&mut [u8; N], ApplyError> {
    let start = usize::try_from(at).map_err(|_| ApplyError::OutOfBounds)?;
    out.get_mut(start..)
        .and_then(|rest| rest.first_chunk_mut::<N>())
        .ok_or(ApplyError::OutOfBounds)
}

fn get(out: &[u8], at: u64, back: u64) -> Result<u8, ApplyError> {
    byte_before(out, at, back).ok_or(ApplyError::BadInstruction)
}

fn put(out: &mut [u8], at: u64, back: i64, value: u8) -> Result<(), ApplyError> {
    let position = at.checked_add_signed(back).ok_or(ApplyError::OutOfBounds)?;
    *slot::<1>(out, position)? = [value];
    Ok(())
}

/// Writes `value` into the field of width `width` at `offset`, checking that
/// it fits.
///
/// # Errors
///
/// [`ApplyError::Overflow`] or [`ApplyError::OutOfBounds`].
pub fn write_value(
    out: &mut [u8],
    offset: u64,
    width: Width,
    value: u64,
) -> Result<(), ApplyError> {
    let signed = value as i64;
    match width {
        Width::None => {}
        Width::W64 => *slot::<8>(out, offset)? = value.to_le_bytes(),
        Width::U32 => {
            let v = u32::try_from(value).map_err(|_| ApplyError::Overflow)?;
            *slot::<4>(out, offset)? = v.to_le_bytes();
        }
        Width::I32 => {
            let v = i32::try_from(signed).map_err(|_| ApplyError::Overflow)?;
            *slot::<4>(out, offset)? = v.to_le_bytes();
        }
        Width::Any16 => {
            if i16::try_from(signed).is_err() && u16::try_from(value).is_err() {
                return Err(ApplyError::Overflow);
            }
            *slot::<2>(out, offset)? = (value as u16).to_le_bytes();
        }
        Width::I16 => {
            let v = i16::try_from(signed).map_err(|_| ApplyError::Overflow)?;
            *slot::<2>(out, offset)? = v.to_le_bytes();
        }
        Width::Any8 => {
            if i8::try_from(signed).is_err() && u8::try_from(value).is_err() {
                return Err(ApplyError::Overflow);
            }
            *slot::<1>(out, offset)? = [value as u8];
        }
        Width::I8 => {
            let v = i8::try_from(signed).map_err(|_| ApplyError::Overflow)?;
            *slot::<1>(out, offset)? = [v as u8];
        }
    }
    Ok(())
}

fn write_i32(out: &mut [u8], at: u64, value: i64) -> Result<(), ApplyError> {
    let v = i32::try_from(value).map_err(|_| ApplyError::Overflow)?;
    *slot::<4>(out, at)? = v.to_le_bytes();
    Ok(())
}

/// Rewrites a relaxed `GOTPCRELX` instruction and writes `value`, which is
/// `S + A - P` for [`Kind::RelaxGotPc`] and `S + A` for
/// [`Kind::RelaxGotPcNoPic`].
///
/// # Errors
///
/// [`ApplyError`] if the instruction is not one that was classified as
/// relaxable, or the value does not fit.
pub fn relax_got(out: &mut [u8], offset: u64, kind: Kind, value: i64) -> Result<(), ApplyError> {
    let op = get(out, offset, 2)?;
    let modrm = get(out, offset, 1)?;
    if kind == Kind::RelaxGotPcNoPic {
        let rex = get(out, offset, 3)?;
        // The immediate is sign-extended to 64 bits and replaces the address
        // loaded from the GOT: undo the -4 of the PC-relative form.
        let value = value.checked_add(4).ok_or(ApplyError::Overflow)?;
        if op == 0x85 {
            // test %reg, foo@GOTPCREL(%rip) -> test $foo, %reg
            put(out, offset, -1, 0xc0 | ((modrm & 0x38) >> 3))?;
            put(out, offset, -3, (rex & !0x4) | ((rex & 0x4) >> 2))?;
            put(out, offset, -2, 0xf7)?;
        } else {
            // binop foo@GOTPCREL(%rip), %reg -> binop $foo, %reg
            put(out, offset, -1, 0xc0 | ((modrm & 0x38) >> 3) | (op & 0x3c))?;
            put(out, offset, -3, (rex & !0x4) | ((rex & 0x4) >> 2))?;
            put(out, offset, -2, 0x81)?;
        }
        return write_i32(out, offset, value);
    }
    match (op, modrm) {
        (0x8b, _) => {
            put(out, offset, -2, 0x8d)?;
            write_i32(out, offset, value)
        }
        (0xff, 0x15) => {
            // call *foo@GOTPCREL(%rip) -> addr32 call foo
            put(out, offset, -2, 0x67)?;
            put(out, offset, -1, 0xe8)?;
            write_i32(out, offset, value)
        }
        (0xff, 0x25) => {
            // jmp *foo@GOTPCREL(%rip) -> jmp foo; nop
            put(out, offset, -2, 0xe9)?;
            put(out, offset, 3, 0x90)?;
            let at = offset.checked_sub(1).ok_or(ApplyError::OutOfBounds)?;
            write_i32(out, at, value.checked_add(1).ok_or(ApplyError::Overflow)?)
        }
        _ => Err(ApplyError::BadInstruction),
    }
}

/// `mov %fs:0, %rax` followed by `lea x@tpoff(%rax), %rax`.
const GD_TO_LE: [u8; 16] = [
    0x64, 0x48, 0x8b, 0x04, 0x25, 0, 0, 0, 0, 0x48, 0x8d, 0x80, 0, 0, 0, 0,
];

/// Relaxes a TLS access to local-exec. `tpoff` is `S + A - TP`.
///
/// # Errors
///
/// [`ApplyError`] for unrecognized instruction sequences.
pub fn relax_tls(out: &mut [u8], offset: u64, kind: Kind, tpoff: i64) -> Result<(), ApplyError> {
    let start = |back: u64| offset.checked_sub(back).ok_or(ApplyError::BadInstruction);
    let copy = |out: &mut [u8], at: u64, bytes: &[u8]| -> Result<(), ApplyError> {
        let at = usize::try_from(at).map_err(|_| ApplyError::OutOfBounds)?;
        let end = at.checked_add(bytes.len()).ok_or(ApplyError::OutOfBounds)?;
        out.get_mut(at..end)
            .ok_or(ApplyError::BadInstruction)?
            .copy_from_slice(bytes);
        Ok(())
    };
    let plus4 = tpoff.checked_add(4).ok_or(ApplyError::Overflow)?;
    match kind {
        Kind::GdToLe => {
            // .byte 0x66; lea x@tlsgd(%rip),%rdi; then either .word 0x6666;
            // rex64; call __tls_get_addr@plt, or .byte 0x66; rex64;
            // call *__tls_get_addr@GOTPCREL(%rip). Both are 16 bytes.
            copy(out, start(4)?, &GD_TO_LE)?;
            write_i32(
                out,
                offset.checked_add(8).ok_or(ApplyError::OutOfBounds)?,
                plus4,
            )
        }
        Kind::LdToLe => {
            // lea x@tlsld(%rip),%rdi; call __tls_get_addr ->
            // data16 data16 data16 mov %fs:0,%rax
            const LD: [u8; 12] = [0x66, 0x66, 0x66, 0x64, 0x48, 0x8b, 0x04, 0x25, 0, 0, 0, 0];
            if byte_after(out, offset, 4) == Some(0xe8) {
                copy(out, start(3)?, &LD)
            } else {
                put(out, offset, -3, 0x66)?;
                copy(out, start(2)?, &LD)
            }
        }
        Kind::IeToLe => {
            let reg = get(out, offset, 1)? >> 3;
            let prefix = get(out, offset, 3)?;
            let op = get(out, offset, 2)?;
            match (prefix, op, reg) {
                (0x48, 0x03, 4) => {
                    // addq foo@gottpoff(%rip),%rsp -> addq $foo,%rsp
                    copy(out, start(3)?, &[0x48, 0x81, 0xc4])?;
                }
                (0x4c, 0x03, 4) => {
                    // addq foo@gottpoff(%rip),%r12 -> addq $foo,%r12
                    copy(out, start(3)?, &[0x49, 0x81, 0xc4])?;
                }
                (0x4c, 0x03, _) => {
                    copy(out, start(3)?, &[0x4d, 0x8d])?;
                    put(out, offset, -1, 0x80 | (reg << 3) | reg)?;
                }
                (0x48, 0x03, _) => {
                    copy(out, start(3)?, &[0x48, 0x8d])?;
                    put(out, offset, -1, 0x80 | (reg << 3) | reg)?;
                }
                (0x4c, 0x8b, _) => {
                    copy(out, start(3)?, &[0x49, 0xc7])?;
                    put(out, offset, -1, 0xc0 | reg)?;
                }
                (0x48, 0x8b, _) => {
                    copy(out, start(3)?, &[0x48, 0xc7])?;
                    put(out, offset, -1, 0xc0 | reg)?;
                }
                _ => return Err(ApplyError::BadInstruction),
            }
            write_i32(out, offset, plus4)
        }
        Kind::DescToLe => {
            // lea x@tlsdesc(%rip), %reg -> mov $x@tpoff, %reg
            let rex = get(out, offset, 3)?;
            let op = get(out, offset, 2)?;
            let modrm = get(out, offset, 1)?;
            if rex & 0xfb != 0x48 || op != 0x8d || modrm & 0xc7 != 0x05 {
                return Err(ApplyError::BadInstruction);
            }
            put(out, offset, -3, 0x48 | ((rex >> 2) & 1))?;
            put(out, offset, -2, 0xc7)?;
            put(out, offset, -1, 0xc0 | ((modrm >> 3) & 7))?;
            write_i32(out, offset, plus4)
        }
        Kind::DescCallToLe => {
            // call *(%rax) -> xchg %ax,%ax
            copy(out, offset, &[0x66, 0x90])
        }
        _ => Err(ApplyError::BadInstruction),
    }
}

/// Size of an IFUNC PLT stub.
pub const IPLT_ENTRY_SIZE: u64 = 16;

/// Writes an IFUNC PLT stub at address `stub` that jumps through the GOT slot
/// at address `slot`: `endbr64` would not fit IBT-less PLTs, so this is
/// `jmp *slot(%rip)` padded with `nop`s.
///
/// # Errors
///
/// [`ApplyError::Overflow`] if the slot is out of range of the stub.
pub fn write_iplt(out: &mut [u8], stub: u64, slot_address: u64) -> Result<(), ApplyError> {
    let entry = slot::<16>(out, 0)?;
    *entry = [
        0xff, 0x25, 0, 0, 0, 0, 0x0f, 0x1f, 0x44, 0, 0, 0x66, 0x0f, 0x1f, 0x44, 0,
    ];
    let displacement = (slot_address as i64)
        .wrapping_sub(stub as i64)
        .wrapping_sub(6);
    write_i32(out, 2, displacement)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relaxes_mov_to_lea() {
        // mov foo@GOTPCREL(%rip), %rax
        let mut code = vec![0x48, 0x8b, 0x05, 0, 0, 0, 0];
        let class = classify(R_X86_64_REX_GOTPCRELX, -4, &code, 3, true).unwrap();
        assert_eq!(class.kind, Kind::RelaxGotPc);
        relax_got(&mut code, 3, class.kind, 0x100).unwrap();
        assert_eq!(code, [0x48, 0x8d, 0x05, 0, 1, 0, 0]);
        let class = classify(R_X86_64_REX_GOTPCRELX, -4, &code, 3, false).unwrap();
        assert_eq!(class.kind, Kind::GotPc);
    }

    #[test]
    fn relaxes_call_and_jmp() {
        let mut call = vec![0xff, 0x15, 0, 0, 0, 0];
        let class = classify(R_X86_64_GOTPCRELX, -4, &call, 2, true).unwrap();
        relax_got(&mut call, 2, class.kind, -8).unwrap();
        assert_eq!(call, [0x67, 0xe8, 0xf8, 0xff, 0xff, 0xff]);
        let mut jmp = vec![0xff, 0x25, 0, 0, 0, 0];
        relax_got(&mut jmp, 2, Kind::RelaxGotPc, 0x10).unwrap();
        assert_eq!(jmp, [0xe9, 0x11, 0, 0, 0, 0x90]);
    }

    #[test]
    fn relaxes_initial_exec() {
        // movq foo@gottpoff(%rip), %rax
        let mut code = vec![0x48, 0x8b, 0x05, 0, 0, 0, 0];
        relax_tls(&mut code, 3, Kind::IeToLe, -0x14).unwrap();
        assert_eq!(code, [0x48, 0xc7, 0xc0, 0xf0, 0xff, 0xff, 0xff]);
        let mut bad = vec![0x0f, 0x0b, 0x05, 0, 0, 0, 0];
        assert_eq!(
            relax_tls(&mut bad, 3, Kind::IeToLe, 0),
            Err(ApplyError::BadInstruction)
        );
    }

    #[test]
    fn relaxes_general_dynamic() {
        let mut code = vec![
            0x66, 0x48, 0x8d, 0x3d, 0, 0, 0, 0, 0x66, 0x66, 0x48, 0xe8, 0, 0, 0, 0,
        ];
        let class = classify(R_X86_64_TLSGD, -4, &code, 4, true).unwrap();
        let plt = classify(
            R_X86_64_TLSGD,
            -4,
            &[
                0x66, 0x48, 0x8d, 0x3d, 0, 0, 0, 0, 0x66, 0x48, 0xff, 0x15, 0, 0, 0, 0,
            ],
            4,
            true,
        );
        assert_eq!(plt.map(|c| c.kind), Ok(Kind::GdToLe));
        assert_eq!(class.kind, Kind::GdToLe);
        relax_tls(&mut code, 4, Kind::GdToLe, -8 - 4).unwrap();
        assert_eq!(&code[..12], &GD_TO_LE[..12]);
        assert_eq!(&code[12..], &(-8i32).to_le_bytes());
    }

    #[test]
    fn value_widths_are_checked() {
        let mut buf = [0u8; 8];
        assert_eq!(
            write_value(&mut buf, 0, Width::U32, 1 << 32),
            Err(ApplyError::Overflow)
        );
        assert_eq!(write_value(&mut buf, 0, Width::I32, (-1i64) as u64), Ok(()));
        assert_eq!(buf[..4], [0xff; 4]);
        assert_eq!(
            write_value(&mut buf, 6, Width::U32, 0),
            Err(ApplyError::OutOfBounds)
        );
        assert_eq!(write_value(&mut buf, 0, Width::Any8, 0xff), Ok(()));
        assert_eq!(
            write_value(&mut buf, 0, Width::I8, 0xff),
            Err(ApplyError::Overflow)
        );
    }

    #[test]
    fn unsupported_types_are_reported() {
        assert_eq!(
            classify(R_X86_64_COPY, 0, &[], 0, true),
            Err(ClassifyError::Unsupported)
        );
        assert_eq!(
            classify(R_X86_64_TLSGD, -4, &[0; 8], 4, true),
            Err(ClassifyError::BadTlsInstruction)
        );
    }
}
