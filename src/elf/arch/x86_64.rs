//! x86-64 relocations: classification, relaxation, application, and the
//! PLT entry encodings.
//!
//! [`classify`] turns a relocation type (plus the instruction bytes in front
//! of it, for relaxations) into a [`Kind`]: what value to compute and which
//! instruction rewrite, if any, applies. The relocation scan uses it to find
//! GOT, PLT and TLS needs; the writer uses the same answer to patch the
//! output, so both always agree. Relaxations follow the psABI and lld:
//!
//! - `GOTPCRELX`/`REX_GOTPCRELX` with addend −4: `mov` → `lea`,
//!   `call *` → `addr32 call`, `jmp *` → `jmp; nop`, and (REX only, in
//!   position-dependent output) `test` and binary operators → immediate
//!   forms;
//! - TLS in executables ([`TlsMode`]): general-dynamic, local-dynamic,
//!   initial-exec and TLS descriptors → local-exec when the variable is in
//!   the executable, which has exactly one TLS block at a known offset from
//!   the thread pointer; general-dynamic and descriptors → initial-exec when
//!   the variable is in a shared library. Shared objects keep the dynamic
//!   models.
//!
//! **x32** (`elf32_x86_64`) uses the same relocations in ELF32 objects, and
//! the same PLT, with 8-byte GOT and `.got.plt` entries. Its code differs in
//! the TLS sequences, which [`classify_x32`] and [`relax_tls_x32`] rewrite
//! as GNU ld does: the general-dynamic `lea` has no `0x66` prefix, so the
//! relaxed sequences are 15 bytes from 3 bytes before the relocation;
//! local-dynamic becomes `nopl` and `movl %fs:0, %eax`; initial-exec code
//! may use a `0x40` or `0x44` REX prefix, or none; descriptors are loaded
//! with `rex leal` and called with `call *(%eax)` (`67 ff 10`), which
//! becomes `nopl (%rax)`. In position-dependent output a `GOTPCRELX` load
//! becomes a `mov` of the address as a 32-bit immediate (REX.W cleared), as
//! GNU ld does, and `test` and binary operators take the immediate with or
//! without a REX prefix ([`relax_got_x32`]).

#![deny(clippy::arithmetic_side_effects)]

use crate::elf::read::consts::x86_64::*;

pub use super::{
    ApplyError, Class, ClassifyContext, ClassifyError, GotKind, Kind, RelaxValues, TlsMode, Width,
    write_value,
};

const fn class(kind: Kind, width: Width) -> Class {
    Class::new(kind, width)
}

const fn got(kind: Kind, width: Width, slot: GotKind) -> Class {
    Class::new(kind, width).through(slot)
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
/// `context.relax_got` says whether GOT-indirect accesses to the symbol may
/// be rewritten into direct ones (the symbol is defined in the output, not
/// preemptible, not an IFUNC, and `--no-relax` was not given).
///
/// # Errors
///
/// [`ClassifyError`] for unsupported types and unrecognized TLS code.
pub fn classify(
    r_type: u32,
    addend: i64,
    data: &[u8],
    offset: u64,
    context: ClassifyContext,
) -> Result<Class, ClassifyError> {
    use Kind as K;
    use Width as W;
    let relax_got = context.relax_got;
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
        R_X86_64_GOTPCREL => {
            // GNU ld also turns `mov foo@GOTPCREL(%rip), %reg` into `lea`
            // when the relocation predates GOTPCRELX (rustc emits these).
            let op = byte_before(data, offset, 2);
            let modrm = byte_before(data, offset, 1);
            if relax_got
                && context.code
                && addend == -4
                && op == Some(0x8b)
                && modrm.is_some_and(|m| m & 0xc7 == 0x05)
            {
                class(K::RelaxGotPc, W::I32)
            } else {
                class(K::Got, W::I32)
            }
        }
        R_X86_64_GOTPCREL64 => class(K::Got, W::W64),
        R_X86_64_GOTPCRELX | R_X86_64_REX_GOTPCRELX => {
            let op = byte_before(data, offset, 2);
            let modrm = byte_before(data, offset, 1);
            let relaxable = relax_got && addend == -4;
            match (op, modrm) {
                (Some(0x8b), _) if relaxable => class(K::RelaxGotPc, W::I32),
                (Some(0xff), Some(0x15 | 0x25)) if relaxable => class(K::RelaxGotPc, W::I32),
                (Some(0x85 | 0x03 | 0x0b | 0x13 | 0x1b | 0x23 | 0x2b | 0x33 | 0x3b), _)
                    if relaxable
                        && !context.pic
                        && r_type == R_X86_64_REX_GOTPCRELX
                        && byte_before(data, offset, 3).is_some() =>
                {
                    class(K::RelaxGotPcNoPic, W::I32)
                }
                _ => class(K::Got, W::I32),
            }
        }
        R_X86_64_GOT32 => class(K::GotSlotRel, W::I32),
        R_X86_64_GOT64 | R_X86_64_GOTPLT64 => class(K::GotSlotRel, W::W64),
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
            match context.tls {
                TlsMode::Dynamic => got(K::Got, W::I32, GotKind::TlsGd),
                mode => match (lea, call) {
                    (Some(0x66), (Some(0x48), Some(0xe8)) | (Some(0xff), Some(0x15))) => {
                        if mode == TlsMode::LocalExec {
                            class(K::GdToLe, W::None).skipping()
                        } else {
                            class(K::GdToIe, W::None).skipping()
                        }
                    }
                    _ => return Err(ClassifyError::BadTlsInstruction),
                },
            }
        }
        R_X86_64_TLSLD => {
            if context.tls_ld != TlsMode::LocalExec {
                return Ok(got(K::Got, W::I32, GotKind::TlsLd));
            }
            let after = (byte_after(data, offset, 4), byte_after(data, offset, 5));
            match after {
                (Some(0xe8), _) | (Some(0xff), Some(0x15)) => class(K::LdToLe, W::None).skipping(),
                _ => return Err(ClassifyError::BadTlsInstruction),
            }
        }
        R_X86_64_GOTTPOFF => match context.tls {
            TlsMode::LocalExec => class(K::IeToLe, W::None),
            _ => got(K::Got, W::I32, GotKind::TpOff),
        },
        R_X86_64_GOTPC32_TLSDESC => match context.tls {
            TlsMode::LocalExec => class(K::DescToLe, W::None),
            TlsMode::InitialExec => class(K::DescToIe, W::None),
            TlsMode::Dynamic => got(K::Got, W::I32, GotKind::TlsDesc),
        },
        R_X86_64_TLSDESC_CALL => match context.tls {
            TlsMode::Dynamic => class(K::None, W::None),
            _ => class(K::DescCallToLe, W::None),
        },
        _ => return Err(ClassifyError::Unsupported),
    })
}

/// Whether `op` (with its ModR/M byte after it) is one of the instructions
/// a `GOTPCRELX` relaxes to an immediate operand: `test`, or `adc`, `add`,
/// `and`, `cmp`, `or`, `sbb`, `sub` or `xor` into a register.
fn is_binop(op: u8) -> bool {
    matches!(
        op,
        0x85 | 0x03 | 0x0b | 0x13 | 0x1b | 0x23 | 0x2b | 0x33 | 0x3b
    )
}

/// Whether `byte` is a REX prefix. In 64-bit mode (x32 code runs in it)
/// 0x40–0x4f are always prefixes.
fn is_rex(byte: u8) -> bool {
    byte & 0xf0 == 0x40
}

/// Classifies x32 relocation `r_type` at `offset` in section `data`: as
/// [`classify`] does for x86-64, except for x32's general-dynamic sequence
/// and for `GOTPCRELX` relaxations in position-dependent output, which
/// GNU ld makes into immediate operands: loads as well as `test` and binary
/// operators, with or without a REX prefix.
///
/// A `GOTPCRELX` (not `REX_GOTPCRELX`) instruction takes an immediate only
/// when the byte before its opcode cannot be taken for a REX prefix, so
/// that [`relax_got_x32`], which sees the instruction but not the
/// relocation type, rewrites a REX prefix only where there is one; a load
/// then becomes a `lea` instead.
///
/// # Errors
///
/// [`ClassifyError`] for unsupported types and unrecognized TLS code.
pub fn classify_x32(
    r_type: u32,
    addend: i64,
    data: &[u8],
    offset: u64,
    context: ClassifyContext,
) -> Result<Class, ClassifyError> {
    use Kind as K;
    use Width as W;
    match r_type {
        R_X86_64_TLSGD => {
            // 48 8d 3d <x@tlsgd>, then 66 66 48 e8 <__tls_get_addr@plt> or
            // 66 48 ff 15 <__tls_get_addr@gotpcrel>: no 0x66 before the
            // `lea`, unlike x86-64.
            if context.tls == TlsMode::Dynamic {
                return Ok(got(K::Got, W::I32, GotKind::TlsGd));
            }
            let lea = [3, 2, 1].map(|back| byte_before(data, offset, back));
            let call = [4, 5, 6, 7].map(|forward| byte_after(data, offset, forward));
            let lea_ok = lea == [Some(0x48), Some(0x8d), Some(0x3d)];
            let call_ok = matches!(
                call,
                [Some(0x66), Some(0x66), Some(0x48), Some(0xe8)]
                    | [Some(0x66), Some(0x48), Some(0xff), Some(0x15)]
            );
            if !lea_ok || !call_ok {
                return Err(ClassifyError::BadTlsInstruction);
            }
            Ok(if context.tls == TlsMode::LocalExec {
                class(K::GdToLe, W::None).skipping()
            } else {
                class(K::GdToIe, W::None).skipping()
            })
        }
        R_X86_64_GOTPCRELX | R_X86_64_REX_GOTPCRELX => {
            let op = byte_before(data, offset, 2);
            let modrm = byte_before(data, offset, 1);
            let relaxable = context.relax_got && addend == -4;
            let prefix = byte_before(data, offset, 3);
            let immediate = relaxable
                && !context.pic
                && if r_type == R_X86_64_REX_GOTPCRELX {
                    prefix.is_some_and(is_rex)
                } else {
                    !prefix.is_some_and(is_rex)
                };
            Ok(match (op, modrm) {
                (Some(op), _) if immediate && (op == 0x8b || is_binop(op)) => {
                    class(K::RelaxGotPcNoPic, W::I32)
                }
                (Some(0x8b), _) if relaxable => class(K::RelaxGotPc, W::I32),
                (Some(0xff), Some(0x15 | 0x25)) if relaxable => class(K::RelaxGotPc, W::I32),
                _ => class(K::Got, W::I32),
            })
        }
        _ => classify(r_type, addend, data, offset, context),
    }
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

/// Fills `out` with the longest x86 no-op instructions, as BFD's
/// `bfd_arch_i386_fill` does.
pub fn write_nops(out: &mut [u8]) {
    const NOPS: [&[u8]; 10] = [
        &[0x90],
        &[0x66, 0x90],
        &[0x0f, 0x1f, 0x00],
        &[0x0f, 0x1f, 0x40, 0x00],
        &[0x0f, 0x1f, 0x44, 0x00, 0x00],
        &[0x66, 0x0f, 0x1f, 0x44, 0x00, 0x00],
        &[0x0f, 0x1f, 0x80, 0x00, 0x00, 0x00, 0x00],
        &[0x0f, 0x1f, 0x84, 0x00, 0x00, 0x00, 0x00, 0x00],
        &[0x66, 0x0f, 0x1f, 0x84, 0x00, 0x00, 0x00, 0x00, 0x00],
        &[0x66, 0x2e, 0x0f, 0x1f, 0x84, 0x00, 0x00, 0x00, 0x00, 0x00],
    ];
    let mut rest = out;
    while !rest.is_empty() {
        let n = rest.len().min(10);
        let nop = NOPS.get(n.saturating_sub(1)).copied().unwrap_or(&[0x90]);
        let (head, tail) = rest.split_at_mut(n.min(nop.len()));
        head.copy_from_slice(nop.get(..head.len()).unwrap_or_default());
        rest = tail;
    }
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

/// Rewrites a relaxed x32 `GOTPCRELX` instruction and writes `value`, as
/// [`relax_got`] does. For an immediate operand ([`Kind::RelaxGotPcNoPic`])
/// a REX prefix, if there is one, has its R bit moved to B; a load becomes
/// `movl $foo, %reg` with REX.W cleared (x32 addresses are 32 bits), while a
/// `test` or binary operator keeps its operand size. Without REX.W the
/// immediate is not extended, so any 32-bit address fits.
///
/// # Errors
///
/// [`ApplyError`] if the instruction is not one that was classified as
/// relaxable, or the value does not fit.
pub fn relax_got_x32(
    out: &mut [u8],
    offset: u64,
    kind: Kind,
    value: i64,
) -> Result<(), ApplyError> {
    if kind != Kind::RelaxGotPcNoPic {
        return relax_got(out, offset, kind, value);
    }
    let op = get(out, offset, 2)?;
    let modrm = get(out, offset, 1)?;
    // `classify_x32` relaxes a REX-less form only when this byte is not
    // a REX prefix.
    let mut rex = byte_before(out, offset, 3).filter(|&b| is_rex(b));
    let (op, modrm) = if op == 0x8b {
        // mov foo@GOTPCREL(%rip), %reg -> movl $foo, %reg
        rex = rex.map(|r| r & !0x08);
        (0xc7, 0xc0 | ((modrm & 0x38) >> 3))
    } else if op == 0x85 {
        // test %reg, foo@GOTPCREL(%rip) -> test $foo, %reg
        (0xf7, 0xc0 | ((modrm & 0x38) >> 3))
    } else if is_binop(op) {
        // binop foo@GOTPCREL(%rip), %reg -> binop $foo, %reg
        (0x81, 0xc0 | ((modrm & 0x38) >> 3) | (op & 0x38))
    } else {
        return Err(ApplyError::BadInstruction);
    };
    // The immediate replaces the address loaded from the GOT: undo the -4
    // of the PC-relative form.
    let value = value.checked_add(4).ok_or(ApplyError::Overflow)?;
    let wide = rex.is_some_and(|r| r & 0x08 != 0);
    let width = if wide { Width::I32 } else { Width::Any32 };
    write_value(out, offset, width, value as u64)?;
    if let Some(rex) = rex {
        put(out, offset, -3, (rex & !0x4) | ((rex & 0x4) >> 2))?;
    }
    put(out, offset, -2, op)?;
    put(out, offset, -1, modrm)
}

/// x32 general-dynamic → local-exec: `movl %fs:0, %eax` followed by
/// `lea x@tpoff(%rax), %rax`, from 3 bytes before the relocation.
const X32_GD_TO_LE: [u8; 15] = [
    0x64, 0x8b, 0x04, 0x25, 0, 0, 0, 0, 0x48, 0x8d, 0x80, 0, 0, 0, 0,
];

/// x32 general-dynamic → initial-exec: `movl %fs:0, %eax` followed by
/// `addq x@gottpoff(%rip), %rax`.
const X32_GD_TO_IE: [u8; 15] = [
    0x64, 0x8b, 0x04, 0x25, 0, 0, 0, 0, 0x48, 0x03, 0x05, 0, 0, 0, 0,
];

/// x32 local-dynamic → local-exec after a direct call: `nopl 0(%rax)` and
/// `movl %fs:0, %eax`.
const X32_LD_TO_LE: [u8; 12] = [0x0f, 0x1f, 0x40, 0x00, 0x64, 0x8b, 0x04, 0x25, 0, 0, 0, 0];

/// The same after an indirect call, one byte longer: `nopw 0(%rax)`.
const X32_LD_TO_LE_INDIRECT: [u8; 13] = [
    0x66, 0x0f, 0x1f, 0x40, 0x00, 0x64, 0x8b, 0x04, 0x25, 0, 0, 0, 0,
];

/// Relaxes an x32 TLS access to local-exec or initial-exec, with GNU ld's
/// x32 sequences (see the module documentation).
///
/// # Errors
///
/// [`ApplyError`] for unrecognized instruction sequences.
pub fn relax_tls_x32(
    out: &mut [u8],
    offset: u64,
    kind: Kind,
    values: RelaxValues,
) -> Result<(), ApplyError> {
    let start = |back: u64| offset.checked_sub(back).ok_or(ApplyError::BadInstruction);
    let after = |forward: u64| offset.checked_add(forward).ok_or(ApplyError::OutOfBounds);
    let plus4 = values.tpoff.checked_add(4).ok_or(ApplyError::Overflow);
    match kind {
        Kind::GdToLe => {
            copy_at(out, start(3)?, &X32_GD_TO_LE)?;
            write_i32(out, after(8)?, plus4?)
        }
        Kind::GdToIe => {
            copy_at(out, start(3)?, &X32_GD_TO_IE)?;
            // As on x86-64, the displacement is at P + 8 and relative to
            // P + 12.
            let got_pc = values.got_pc.checked_sub(8).ok_or(ApplyError::Overflow)?;
            write_i32(out, after(8)?, got_pc)
        }
        Kind::LdToLe => {
            if byte_after(out, offset, 4) == Some(0xff) {
                copy_at(out, start(3)?, &X32_LD_TO_LE_INDIRECT)
            } else {
                copy_at(out, start(3)?, &X32_LD_TO_LE)
            }
        }
        Kind::IeToLe => {
            // mov foo@gottpoff(%rip), %reg -> mov $foo, %reg
            // add foo@gottpoff(%rip), %reg -> lea foo(%reg), %reg
            // add foo@gottpoff(%rip), %esp or %r12d -> add $foo, %reg
            // with a REX prefix of 0x4c or 0x44 moving its R bit to B; any
            // other byte before the opcode is left alone.
            let prefix = byte_before(out, offset, 3);
            let op = get(out, offset, 2)?;
            let reg = (get(out, offset, 1)? >> 3) & 7;
            let (op, modrm, rex_w, rex) = match op {
                0x8b => (0xc7, 0xc0 | reg, 0x49, 0x41),
                0x03 if reg == 4 => (0x81, 0xc0 | reg, 0x49, 0x41),
                0x03 => (0x8d, 0x80 | reg | (reg << 3), 0x4d, 0x45),
                _ => return Err(ApplyError::BadInstruction),
            };
            match prefix {
                Some(0x4c) => put(out, offset, -3, rex_w)?,
                Some(0x44) => put(out, offset, -3, rex)?,
                _ => {}
            }
            put(out, offset, -2, op)?;
            put(out, offset, -1, modrm)?;
            write_i32(out, offset, plus4?)
        }
        Kind::DescToLe => {
            // rex leal x@tlsdesc(%rip), %reg -> rex movl $x@tpoff, %reg
            let rex = get(out, offset, 3)?;
            let op = get(out, offset, 2)?;
            let modrm = get(out, offset, 1)?;
            if !matches!(rex & 0xfb, 0x40 | 0x48) || op != 0x8d || modrm & 0xc7 != 0x05 {
                return Err(ApplyError::BadInstruction);
            }
            put(out, offset, -3, (rex & 0x48) | ((rex >> 2) & 1))?;
            put(out, offset, -2, 0xc7)?;
            put(out, offset, -1, 0xc0 | ((modrm >> 3) & 7))?;
            write_i32(out, offset, plus4?)
        }
        Kind::DescCallToLe => {
            // call *(%eax) -> nopl (%rax); call *(%rax) -> xchg %ax,%ax
            if byte_after(out, offset, 0) == Some(0x67) {
                copy_at(out, offset, &[0x0f, 0x1f, 0x00])
            } else {
                copy_at(out, offset, &[0x66, 0x90])
            }
        }
        _ => relax_tls(out, offset, kind, values),
    }
}

/// Copies `bytes` over `out` at `at`.
fn copy_at(out: &mut [u8], at: u64, bytes: &[u8]) -> Result<(), ApplyError> {
    let at = usize::try_from(at).map_err(|_| ApplyError::OutOfBounds)?;
    let end = at.checked_add(bytes.len()).ok_or(ApplyError::OutOfBounds)?;
    out.get_mut(at..end)
        .ok_or(ApplyError::BadInstruction)?
        .copy_from_slice(bytes);
    Ok(())
}

/// `mov %fs:0, %rax` followed by `lea x@tpoff(%rax), %rax`.
const GD_TO_LE: [u8; 16] = [
    0x64, 0x48, 0x8b, 0x04, 0x25, 0, 0, 0, 0, 0x48, 0x8d, 0x80, 0, 0, 0, 0,
];

/// Relaxes a TLS access to local-exec or initial-exec.
///
/// # Errors
///
/// [`ApplyError`] for unrecognized instruction sequences.
pub fn relax_tls(
    out: &mut [u8],
    offset: u64,
    kind: Kind,
    values: RelaxValues,
) -> Result<(), ApplyError> {
    if matches!(kind, Kind::GdToIe | Kind::DescToIe) {
        return relax_tls_ie(out, offset, kind, values.got_pc);
    }
    let tpoff = values.tpoff;
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

/// `mov %fs:0, %rax` followed by `add x@gottpoff(%rip), %rax`.
const GD_TO_IE: [u8; 16] = [
    0x64, 0x48, 0x8b, 0x04, 0x25, 0, 0, 0, 0, 0x48, 0x03, 0x05, 0, 0, 0, 0,
];

/// Relaxes a general-dynamic or descriptor access to initial-exec.
/// `got_pc` is `GOT entry + A - P`, with `A` the relocation's addend.
fn relax_tls_ie(out: &mut [u8], offset: u64, kind: Kind, got_pc: i64) -> Result<(), ApplyError> {
    match kind {
        Kind::GdToIe => {
            let start = offset.checked_sub(4).ok_or(ApplyError::BadInstruction)?;
            let at = usize::try_from(start).map_err(|_| ApplyError::OutOfBounds)?;
            let end = at
                .checked_add(GD_TO_IE.len())
                .ok_or(ApplyError::OutOfBounds)?;
            out.get_mut(at..end)
                .ok_or(ApplyError::BadInstruction)?
                .copy_from_slice(&GD_TO_IE);
            // The displacement is at P + 8 and relative to P + 12; the
            // addend of the original relocation accounts for 4 of it.
            write_i32(
                out,
                offset.checked_add(8).ok_or(ApplyError::OutOfBounds)?,
                got_pc.checked_sub(8).ok_or(ApplyError::Overflow)?,
            )
        }
        Kind::DescToIe => {
            // lea x@tlsdesc(%rip), %reg -> mov x@gottpoff(%rip), %reg
            if get(out, offset, 2)? != 0x8d {
                return Err(ApplyError::BadInstruction);
            }
            put(out, offset, -2, 0x8b)?;
            write_i32(out, offset, got_pc)
        }
        _ => Err(ApplyError::BadInstruction),
    }
}

/// Size of a PLT entry (all entry kinds with IBT, and `.plt` entries
/// without it).
pub const PLT_ENTRY_SIZE: u64 = 16;
/// Size of a `.plt.got` entry without IBT.
pub const PLT_GOT_ENTRY_SIZE: u64 = 8;

fn rel32(target: u64, next_instruction: u64) -> Result<[u8; 4], ApplyError> {
    let value = (target as i64).wrapping_sub(next_instruction as i64);
    Ok(i32::try_from(value)
        .map_err(|_| ApplyError::Overflow)?
        .to_le_bytes())
}

fn put_bytes(out: &mut [u8], at: usize, bytes: &[u8]) -> Result<(), ApplyError> {
    let end = at.checked_add(bytes.len()).ok_or(ApplyError::OutOfBounds)?;
    out.get_mut(at..end)
        .ok_or(ApplyError::OutOfBounds)?
        .copy_from_slice(bytes);
    Ok(())
}

/// Writes the lazy PLT header at address `plt`: push the link map word and
/// jump to the resolver through `.got.plt` (at `got_plt`).
///
/// # Errors
///
/// [`ApplyError`] when out of range.
pub fn write_plt_header(out: &mut [u8], plt: u64, got_plt: u64) -> Result<(), ApplyError> {
    put_bytes(out, 0, &[0xff, 0x35])?;
    put_bytes(
        out,
        2,
        &rel32(got_plt.wrapping_add(8), plt.wrapping_add(6))?,
    )?;
    put_bytes(out, 6, &[0xff, 0x25])?;
    put_bytes(
        out,
        8,
        &rel32(got_plt.wrapping_add(16), plt.wrapping_add(12))?,
    )?;
    put_bytes(out, 12, &[0x0f, 0x1f, 0x40, 0x00])
}

/// Writes lazy `.plt` entry `index` at `entry`. Without IBT it jumps
/// through its `.got.plt` slot at `slot` (which initially points back at
/// the `push`); with IBT it is only the lazy-binding half, and the jump is
/// in `.plt.sec`.
///
/// # Errors
///
/// [`ApplyError`] when out of range.
pub fn write_plt_entry(
    out: &mut [u8],
    entry: u64,
    slot: u64,
    index: u32,
    plt: u64,
    ibt: bool,
) -> Result<(), ApplyError> {
    if ibt {
        put_bytes(out, 0, &[0xf3, 0x0f, 0x1e, 0xfa, 0x68])?;
        put_bytes(out, 5, &index.to_le_bytes())?;
        put_bytes(out, 9, &[0xe9])?;
        put_bytes(out, 10, &rel32(plt, entry.wrapping_add(14))?)?;
        put_bytes(out, 14, &[0x66, 0x90])
    } else {
        put_bytes(out, 0, &[0xff, 0x25])?;
        put_bytes(out, 2, &rel32(slot, entry.wrapping_add(6))?)?;
        put_bytes(out, 6, &[0x68])?;
        put_bytes(out, 7, &index.to_le_bytes())?;
        put_bytes(out, 11, &[0xe9])?;
        put_bytes(out, 12, &rel32(plt, entry.wrapping_add(16))?)
    }
}

/// Writes a `.plt.sec` entry (IBT) or `.plt.got` entry at `entry` that
/// jumps through the GOT word at `slot`. `ibt` selects the 16-byte form
/// with `endbr64`; otherwise the 8-byte `.plt.got` form is written.
///
/// # Errors
///
/// [`ApplyError`] when out of range.
pub fn write_plt_jump(out: &mut [u8], entry: u64, slot: u64, ibt: bool) -> Result<(), ApplyError> {
    if ibt {
        put_bytes(out, 0, &[0xf3, 0x0f, 0x1e, 0xfa, 0xff, 0x25])?;
        put_bytes(out, 6, &rel32(slot, entry.wrapping_add(10))?)?;
        put_bytes(out, 10, &[0x66, 0x0f, 0x1f, 0x44, 0x00, 0x00])
    } else {
        put_bytes(out, 0, &[0xff, 0x25])?;
        put_bytes(out, 2, &rel32(slot, entry.wrapping_add(6))?)?;
        put_bytes(out, 6, &[0x66, 0x90])
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

    /// A relaxation that only needs the thread pointer offset.
    fn tpoff(tpoff: i64) -> RelaxValues {
        RelaxValues {
            tpoff,
            ..RelaxValues::default()
        }
    }

    /// A relaxation that only needs the GOT entry's PC-relative offset.
    fn got_pc(got_pc: i64) -> RelaxValues {
        RelaxValues {
            got_pc,
            ..RelaxValues::default()
        }
    }

    #[test]
    fn relaxes_mov_to_lea() {
        // mov foo@GOTPCREL(%rip), %rax
        let mut code = vec![0x48, 0x8b, 0x05, 0, 0, 0, 0];
        let class = classify(
            R_X86_64_REX_GOTPCRELX,
            -4,
            &code,
            3,
            ClassifyContext::static_exec(true),
        )
        .unwrap();
        assert_eq!(class.kind, Kind::RelaxGotPc);
        relax_got(&mut code, 3, class.kind, 0x100).unwrap();
        assert_eq!(code, [0x48, 0x8d, 0x05, 0, 1, 0, 0]);
        let class = classify(
            R_X86_64_REX_GOTPCRELX,
            -4,
            &code,
            3,
            ClassifyContext::static_exec(false),
        )
        .unwrap();
        assert_eq!(class.kind, Kind::Got);
    }

    #[test]
    fn plain_gotpcrel_relaxes_only_mov_in_code() {
        let code_context = ClassifyContext {
            code: true,
            ..ClassifyContext::static_exec(true)
        };
        let mov = [0x48, 0x8b, 0x05, 0, 0, 0, 0];
        let class = classify(R_X86_64_GOTPCREL, -4, &mov, 3, code_context).unwrap();
        assert_eq!(class.kind, Kind::RelaxGotPc);
        let data = classify(
            R_X86_64_GOTPCREL,
            -4,
            &mov,
            3,
            ClassifyContext::static_exec(true),
        )
        .unwrap();
        assert_eq!(data.kind, Kind::Got);
        let call = [0xff, 0x15, 0, 0, 0, 0];
        let class = classify(R_X86_64_GOTPCREL, -4, &call, 2, code_context).unwrap();
        assert_eq!(class.kind, Kind::Got);
    }

    #[test]
    fn relaxes_call_and_jmp() {
        let mut call = vec![0xff, 0x15, 0, 0, 0, 0];
        let class = classify(
            R_X86_64_GOTPCRELX,
            -4,
            &call,
            2,
            ClassifyContext::static_exec(true),
        )
        .unwrap();
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
        relax_tls(&mut code, 3, Kind::IeToLe, tpoff(-0x14)).unwrap();
        assert_eq!(code, [0x48, 0xc7, 0xc0, 0xf0, 0xff, 0xff, 0xff]);
        let mut bad = vec![0x0f, 0x0b, 0x05, 0, 0, 0, 0];
        assert_eq!(
            relax_tls(&mut bad, 3, Kind::IeToLe, tpoff(0)),
            Err(ApplyError::BadInstruction)
        );
    }

    #[test]
    fn relaxes_general_dynamic() {
        let mut code = vec![
            0x66, 0x48, 0x8d, 0x3d, 0, 0, 0, 0, 0x66, 0x66, 0x48, 0xe8, 0, 0, 0, 0,
        ];
        let class = classify(
            R_X86_64_TLSGD,
            -4,
            &code,
            4,
            ClassifyContext::static_exec(true),
        )
        .unwrap();
        let plt = classify(
            R_X86_64_TLSGD,
            -4,
            &[
                0x66, 0x48, 0x8d, 0x3d, 0, 0, 0, 0, 0x66, 0x48, 0xff, 0x15, 0, 0, 0, 0,
            ],
            4,
            ClassifyContext::static_exec(true),
        );
        assert_eq!(plt.map(|c| c.kind), Ok(Kind::GdToLe));
        let shared = ClassifyContext {
            relax_got: false,
            pic: true,
            tls: TlsMode::Dynamic,
            tls_ld: TlsMode::Dynamic,
            code: true,
        };
        let kept = classify(R_X86_64_TLSGD, -4, &code, 4, shared).unwrap();
        assert_eq!(kept.kind, Kind::Got);
        assert_eq!(kept.slot, GotKind::TlsGd);
        let ie = ClassifyContext {
            tls: TlsMode::InitialExec,
            tls_ld: TlsMode::LocalExec,
            ..shared
        };
        let mut gd = code.clone();
        let ie_class = classify(R_X86_64_TLSGD, -4, &gd, 4, ie).unwrap();
        assert_eq!(ie_class.kind, Kind::GdToIe);
        relax_tls(&mut gd, 4, Kind::GdToIe, got_pc(0x100 - 4)).unwrap();
        assert_eq!(&gd[..12], &GD_TO_IE[..12]);
        assert_eq!(&gd[12..], &(0x100i32 - 12).to_le_bytes());
        assert_eq!(class.kind, Kind::GdToLe);
        relax_tls(&mut code, 4, Kind::GdToLe, tpoff(-8 - 4)).unwrap();
        assert_eq!(&code[..12], &GD_TO_LE[..12]);
        assert_eq!(&code[12..], &(-8i32).to_le_bytes());
    }

    #[test]
    fn plt_entries_jump_through_their_slots() {
        let mut header = [0u8; 16];
        write_plt_header(&mut header, 0x1020, 0x3000).unwrap();
        assert_eq!(&header[..2], &[0xff, 0x35]);
        assert_eq!(&header[2..6], &(0x3008i32 - 0x1026).to_le_bytes());
        let mut entry = [0u8; 16];
        write_plt_entry(&mut entry, 0x1030, 0x3018, 3, 0x1020, false).unwrap();
        assert_eq!(&entry[2..6], &(0x3018i32 - 0x1036).to_le_bytes());
        assert_eq!(&entry[7..11], &3u32.to_le_bytes());
        assert_eq!(&entry[12..16], &(0x1020i32 - 0x1040).to_le_bytes());
        write_plt_entry(&mut entry, 0x1030, 0x3018, 3, 0x1020, true).unwrap();
        assert_eq!(&entry[..4], &[0xf3, 0x0f, 0x1e, 0xfa]);
        assert_eq!(&entry[10..14], &(0x1020i32 - 0x103e).to_le_bytes());
        let mut sec = [0u8; 16];
        write_plt_jump(&mut sec, 0x1050, 0x3018, true).unwrap();
        assert_eq!(&sec[6..10], &(0x3018i32 - 0x105a).to_le_bytes());
        let mut got = [0u8; 8];
        write_plt_jump(&mut got, 0x1050, 0x3018, false).unwrap();
        assert_eq!(got[..2], [0xff, 0x25]);
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
            classify(R_X86_64_COPY, 0, &[], 0, ClassifyContext::static_exec(true)),
            Err(ClassifyError::Unsupported)
        );
        assert_eq!(
            classify(
                R_X86_64_TLSGD,
                -4,
                &[0; 8],
                4,
                ClassifyContext::static_exec(true)
            ),
            Err(ClassifyError::BadTlsInstruction)
        );
    }

    /// x32 general-dynamic: the `lea` has no `0x66` prefix, and the
    /// relaxed sequences start 3 bytes before the relocation.
    #[test]
    fn x32_relaxes_general_dynamic() {
        let mut code = vec![
            0x48, 0x8d, 0x3d, 0, 0, 0, 0, 0x66, 0x66, 0x48, 0xe8, 0, 0, 0, 0,
        ];
        let class = classify_x32(
            R_X86_64_TLSGD,
            -4,
            &code,
            3,
            ClassifyContext::static_exec(true),
        )
        .unwrap();
        assert_eq!(class.kind, Kind::GdToLe);
        assert!(class.skip_next);
        relax_tls_x32(&mut code, 3, Kind::GdToLe, tpoff(-8 - 4)).unwrap();
        assert_eq!(&code[..11], &X32_GD_TO_LE[..11]);
        assert_eq!(&code[11..], &(-8i32).to_le_bytes());
        // Without the `lea` an x32 sequence is not recognized.
        assert_eq!(
            classify_x32(
                R_X86_64_TLSGD,
                -4,
                &[
                    0x90, 0x90, 0x90, 0, 0, 0, 0, 0x66, 0x66, 0x48, 0xe8, 0, 0, 0, 0
                ],
                3,
                ClassifyContext::static_exec(true)
            ),
            Err(ClassifyError::BadTlsInstruction)
        );
        // To initial-exec: `addq x@gottpoff(%rip), %rax` reads the entry
        // 12 bytes past the relocation.
        let mut gd = vec![
            0x48, 0x8d, 0x3d, 0, 0, 0, 0, 0x66, 0x48, 0xff, 0x15, 0, 0, 0, 0,
        ];
        relax_tls_x32(&mut gd, 3, Kind::GdToIe, got_pc(0x100 - 4)).unwrap();
        assert_eq!(&gd[..11], &X32_GD_TO_IE[..11]);
        assert_eq!(&gd[11..], &(0x100i32 - 12).to_le_bytes());
    }

    /// x32 local-dynamic: `nopl 0(%rax)` before `movl %fs:0, %eax`, one
    /// byte longer for the indirect call.
    #[test]
    fn x32_relaxes_local_dynamic() {
        let mut direct = vec![0x48, 0x8d, 0x3d, 0, 0, 0, 0, 0xe8, 0, 0, 0, 0];
        let class = classify_x32(
            R_X86_64_TLSLD,
            -4,
            &direct,
            3,
            ClassifyContext::static_exec(true),
        )
        .unwrap();
        assert_eq!(class.kind, Kind::LdToLe);
        relax_tls_x32(&mut direct, 3, Kind::LdToLe, tpoff(0)).unwrap();
        assert_eq!(direct, X32_LD_TO_LE);
        let mut indirect = vec![0x48, 0x8d, 0x3d, 0, 0, 0, 0, 0xff, 0x15, 0, 0, 0, 0];
        relax_tls_x32(&mut indirect, 3, Kind::LdToLe, tpoff(0)).unwrap();
        assert_eq!(indirect, X32_LD_TO_LE_INDIRECT);
    }

    /// x32 initial-exec: `addl` without a REX prefix, with an empty one,
    /// and with `0x44`, whose R bit moves to B.
    #[test]
    fn x32_relaxes_initial_exec() {
        // addl foo@gottpoff(%rip), %eax -> leal foo(%rax), %eax
        let mut plain = vec![0x03, 0x05, 0, 0, 0, 0];
        relax_tls_x32(&mut plain, 2, Kind::IeToLe, tpoff(-0x14)).unwrap();
        assert_eq!(plain, [0x8d, 0x80, 0xf0, 0xff, 0xff, 0xff]);
        // rex addl foo@gottpoff(%rip), %eax -> rex leal foo(%rax), %eax
        let mut rex = vec![0x40, 0x03, 0x05, 0, 0, 0, 0];
        relax_tls_x32(&mut rex, 3, Kind::IeToLe, tpoff(-0x14)).unwrap();
        assert_eq!(rex, [0x40, 0x8d, 0x80, 0xf0, 0xff, 0xff, 0xff]);
        // addl foo@gottpoff(%rip), %r9d -> leal foo(%r9), %r9d
        let mut high = vec![0x44, 0x03, 0x0d, 0, 0, 0, 0];
        relax_tls_x32(&mut high, 3, Kind::IeToLe, tpoff(-0x14)).unwrap();
        assert_eq!(high, [0x45, 0x8d, 0x89, 0xf0, 0xff, 0xff, 0xff]);
        // addl foo@gottpoff(%rip), %r12d -> addl $foo, %r12d
        let mut stack = vec![0x44, 0x03, 0x25, 0, 0, 0, 0];
        relax_tls_x32(&mut stack, 3, Kind::IeToLe, tpoff(-0x14)).unwrap();
        assert_eq!(stack, [0x41, 0x81, 0xc4, 0xf0, 0xff, 0xff, 0xff]);
        // movl foo@gottpoff(%rip), %ecx -> movl $foo, %ecx
        let mut load = vec![0x8b, 0x0d, 0, 0, 0, 0];
        relax_tls_x32(&mut load, 2, Kind::IeToLe, tpoff(-0x14)).unwrap();
        assert_eq!(load, [0xc7, 0xc1, 0xf0, 0xff, 0xff, 0xff]);
    }

    /// x32 TLS descriptors: `rex leal x@tlsdesc(%rip), %eax` and
    /// `call *(%eax)`, which becomes `nopl (%rax)`.
    #[test]
    fn x32_relaxes_descriptors() {
        let mut lea = vec![0x40, 0x8d, 0x05, 0, 0, 0, 0];
        let class = classify_x32(
            R_X86_64_GOTPC32_TLSDESC,
            -4,
            &lea,
            3,
            ClassifyContext::static_exec(true),
        )
        .unwrap();
        assert_eq!(class.kind, Kind::DescToLe);
        relax_tls_x32(&mut lea, 3, Kind::DescToLe, tpoff(-0x8 - 4)).unwrap();
        assert_eq!(lea, [0x40, 0xc7, 0xc0, 0xf8, 0xff, 0xff, 0xff]);
        // REX.R moves to REX.B: `rex.R leal ..., %r9d` -> `rex.B movl`.
        let mut high = vec![0x44, 0x8d, 0x0d, 0, 0, 0, 0];
        relax_tls_x32(&mut high, 3, Kind::DescToLe, tpoff(-4)).unwrap();
        assert_eq!(high, [0x41, 0xc7, 0xc1, 0, 0, 0, 0]);
        // The descriptor call, with and without the address-size prefix.
        let mut call = vec![0x67, 0xff, 0x10];
        relax_tls_x32(&mut call, 0, Kind::DescCallToLe, RelaxValues::default()).unwrap();
        assert_eq!(call, [0x0f, 0x1f, 0x00]);
        let mut lp64 = vec![0xff, 0x10];
        relax_tls_x32(&mut lp64, 0, Kind::DescCallToLe, RelaxValues::default()).unwrap();
        assert_eq!(lp64, [0x66, 0x90]);
        // To initial-exec, the `lea` becomes a `mov` of the GOT entry.
        let mut ie = vec![0x40, 0x8d, 0x05, 0, 0, 0, 0];
        relax_tls_x32(&mut ie, 3, Kind::DescToIe, got_pc(0x20)).unwrap();
        assert_eq!(ie, [0x40, 0x8b, 0x05, 0x20, 0, 0, 0]);
    }

    /// x32 `GOTPCRELX`: in position-dependent output a load takes the
    /// address as an immediate with REX.W cleared, and `test` and binary
    /// operators do so with or without a REX prefix. A form whose REX
    /// prefix cannot be told from the previous instruction's last byte is
    /// left to the `lea` relaxation.
    #[test]
    fn x32_relaxes_got_loads() {
        let context = ClassifyContext::static_exec(true);
        // movq foo@GOTPCREL(%rip), %rax -> movl $foo, %eax
        let mut wide = vec![0x48, 0x8b, 0x05, 0, 0, 0, 0];
        let class = classify_x32(R_X86_64_REX_GOTPCRELX, -4, &wide, 3, context).unwrap();
        assert_eq!(class.kind, Kind::RelaxGotPcNoPic);
        relax_got_x32(&mut wide, 3, class.kind, 0x4000f8).unwrap();
        assert_eq!(wide, [0x40, 0xc7, 0xc0, 0xfc, 0x00, 0x40, 0x00]);
        // movl foo@GOTPCREL(%rip), %eax, with no REX prefix at all.
        let mut plain = vec![0x8b, 0x05, 0, 0, 0, 0];
        let class = classify_x32(R_X86_64_GOTPCRELX, -4, &plain, 2, context).unwrap();
        assert_eq!(class.kind, Kind::RelaxGotPcNoPic);
        relax_got_x32(&mut plain, 2, class.kind, 0xfc).unwrap();
        assert_eq!(plain, [0xc7, 0xc0, 0x00, 0x01, 0x00, 0x00]);
        // addl foo@GOTPCREL(%rip), %eax -> addl $foo, %eax
        let mut binop = vec![0x03, 0x05, 0, 0, 0, 0];
        let class = classify_x32(R_X86_64_GOTPCRELX, -4, &binop, 2, context).unwrap();
        relax_got_x32(&mut binop, 2, class.kind, 0xfc).unwrap();
        assert_eq!(binop, [0x81, 0xc0, 0x00, 0x01, 0x00, 0x00]);
        // A byte that could be a REX prefix before a `GOTPCRELX` load: the
        // load relaxes to a `lea`, which rewrites no prefix.
        let ambiguous = vec![0x41, 0x8b, 0x05, 0, 0, 0, 0];
        let class = classify_x32(R_X86_64_GOTPCRELX, -4, &ambiguous, 3, context).unwrap();
        assert_eq!(class.kind, Kind::RelaxGotPc);
        // In position-independent output nothing takes an immediate: a
        // load becomes a `lea` and the rest keep their GOT entry.
        let pic = ClassifyContext {
            pic: true,
            ..context
        };
        let load = [0x48, 0x8b, 0x05, 0, 0, 0, 0];
        let class = classify_x32(R_X86_64_REX_GOTPCRELX, -4, &load, 3, pic).unwrap();
        assert_eq!(class.kind, Kind::RelaxGotPc);
        let add = [0x03, 0x05, 0, 0, 0, 0];
        let class = classify_x32(R_X86_64_GOTPCRELX, -4, &add, 2, pic).unwrap();
        assert_eq!(class.kind, Kind::Got);
    }
}
