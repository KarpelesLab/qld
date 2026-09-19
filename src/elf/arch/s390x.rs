//! s390x relocations: classification, relaxation, and the PLT.
//!
//! s390x is big-endian, uses `SHT_RELA`, and counts PC-relative offsets of
//! instructions in halfwords (`R_390_PC32DBL`, `R_390_PLT32DBL`, …; see
//! [`crate::arch::s390x`]). Everything follows GNU ld's `elf64-s390.c`,
//! the reference (lld has no s390x port).
//!
//! **The GOT pointer** (`_GLOBAL_OFFSET_TABLE_`, which PIC code keeps in
//! `%r12` and `R_390_GOTPCDBL` computes) is at the very start of the GOT,
//! and every GOT-relative relocation (`R_390_GOT12`, `GOT20`, …) counts
//! from it, some of them unsigned. Its first three words are reserved for
//! the dynamic linker (`_DYNAMIC`, the link map, the resolver), and
//! `DT_PLTGOT` points at them. With `-z now`, `.got.plt` comes first, at
//! the start of `.got`, and holds them; otherwise `.got` comes first, and
//! the three words are the start of `.got`
//! ([`crate::elf::synth::Synth::got_header`]).
//!
//! **The PLT** has a 32-byte header and 32-byte entries that load their
//! `.got.plt` slot with `larl` + `lg`; a slot starts out pointing into its
//! own entry, which passes the offset of its `R_390_JMP_SLOT` relocation to
//! the header. IFUNC stubs are PLT entries too.
//!
//! **Relaxations** (in executables only, as GNU ld does):
//!
//! - `lgrl %rx,sym@GOTENT` and `lg %rx,sym@GOT(%r12)` become
//!   `larl %rx,sym` when the output is position-independent, `sym` is
//!   defined in it and cannot be preempted, and its address is even
//!   ([`Kind::GotRelax`]). The GOT entry stays, as in GNU ld.
//! - TLS: the literal pool entry of a general-dynamic access
//!   (`R_390_TLS_GD64`) becomes the offset of an initial-exec GOT entry,
//!   and the `brasl %r14,__tls_get_offset` marked by `R_390_TLS_GDCALL`
//!   becomes `lg %r2,0(%r2,%r12)`, for a variable of a shared library; for
//!   one of the executable the entry becomes the thread pointer offset and
//!   the call `brcl 0,.`. Local-dynamic becomes local-exec the same way
//!   (`R_390_TLS_LDM64`, `R_390_TLS_LDCALL`, `R_390_TLS_LDO64`), and so
//!   does initial-exec through the literal pool (`R_390_TLS_GOTIE64` and
//!   `R_390_TLS_IE64`, with the load marked by `R_390_TLS_LOAD` turned into
//!   `sllg`). Initial-exec through `R_390_TLS_GOTIE12`, `GOTIE20` and
//!   `IEENT` keeps its GOT entry, which then holds a constant.
//!
//! `R_390_GOTPLT*` relocations use the symbol's GOT entry, not its
//! `.got.plt` slot (GNU ld's choice when the symbol has a PLT entry), and
//! `R_390_PLTOFF*`, which gcc does not emit, are not linked.

#![deny(clippy::arithmetic_side_effects)]

use crate::arch::s390x::{self as insn, Field};

pub use super::{
    ApplyError, Class, ClassifyContext, ClassifyError, GotKind, Kind, RelaxValues, TlsMode, Width,
};

/// No relocation.
pub const R_390_NONE: u32 = 0;
/// `S + A`, 8 bits.
pub const R_390_8: u32 = 1;
/// `S + A`, a 12-bit displacement.
pub const R_390_12: u32 = 2;
/// `S + A`, 16 bits.
pub const R_390_16: u32 = 3;
/// `S + A`, 32 bits.
pub const R_390_32: u32 = 4;
/// `S + A - P`, 32 bits.
pub const R_390_PC32: u32 = 5;
/// `G + A - GOT`, a 12-bit displacement.
pub const R_390_GOT12: u32 = 6;
/// `G + A - GOT`, 32 bits.
pub const R_390_GOT32: u32 = 7;
/// `L + A - P`, 32 bits.
pub const R_390_PLT32: u32 = 8;
/// Dynamic: copy the symbol's data.
pub const R_390_COPY: u32 = 9;
/// Dynamic: a GOT entry.
pub const R_390_GLOB_DAT: u32 = 10;
/// Dynamic: a lazily bound `.got.plt` slot.
pub const R_390_JMP_SLOT: u32 = 11;
/// Dynamic: `B + A`.
pub const R_390_RELATIVE: u32 = 12;
/// `S + A - GOT`, 32 bits.
pub const R_390_GOTOFF32: u32 = 13;
/// `GOT + A - P`, 64 bits.
pub const R_390_GOTPC: u32 = 14;
/// `G + A - GOT`, 16 bits.
pub const R_390_GOT16: u32 = 15;
/// `S + A - P`, 16 bits.
pub const R_390_PC16: u32 = 16;
/// `(S + A - P) >> 1`, 16 bits.
pub const R_390_PC16DBL: u32 = 17;
/// `(L + A - P) >> 1`, 16 bits.
pub const R_390_PLT16DBL: u32 = 18;
/// `(S + A - P) >> 1`, 32 bits.
pub const R_390_PC32DBL: u32 = 19;
/// `(L + A - P) >> 1`, 32 bits.
pub const R_390_PLT32DBL: u32 = 20;
/// `(GOT + A - P) >> 1`, 32 bits.
pub const R_390_GOTPCDBL: u32 = 21;
/// `S + A`, 64 bits.
pub const R_390_64: u32 = 22;
/// `S + A - P`, 64 bits.
pub const R_390_PC64: u32 = 23;
/// `G + A - GOT`, 64 bits.
pub const R_390_GOT64: u32 = 24;
/// `L + A - P`, 64 bits.
pub const R_390_PLT64: u32 = 25;
/// `(G + A - P) >> 1`, 32 bits.
pub const R_390_GOTENT: u32 = 26;
/// `S + A - GOT`, 16 bits.
pub const R_390_GOTOFF16: u32 = 27;
/// `S + A - GOT`, 64 bits.
pub const R_390_GOTOFF64: u32 = 28;
/// `G + A - GOT` of the `.got.plt` slot, a 12-bit displacement.
pub const R_390_GOTPLT12: u32 = 29;
/// `G + A - GOT` of the `.got.plt` slot, 16 bits.
pub const R_390_GOTPLT16: u32 = 30;
/// `G + A - GOT` of the `.got.plt` slot, 32 bits.
pub const R_390_GOTPLT32: u32 = 31;
/// `G + A - GOT` of the `.got.plt` slot, 64 bits.
pub const R_390_GOTPLT64: u32 = 32;
/// `(G + A - P) >> 1` of the `.got.plt` slot, 32 bits.
pub const R_390_GOTPLTENT: u32 = 33;
/// `L + A - GOT`, 16 bits.
pub const R_390_PLTOFF16: u32 = 34;
/// `L + A - GOT`, 32 bits.
pub const R_390_PLTOFF32: u32 = 35;
/// `L + A - GOT`, 64 bits.
pub const R_390_PLTOFF64: u32 = 36;
/// Marks the initial-exec load of a thread pointer offset.
pub const R_390_TLS_LOAD: u32 = 37;
/// Marks the `__tls_get_offset` call of a general-dynamic access.
pub const R_390_TLS_GDCALL: u32 = 38;
/// Marks the `__tls_get_offset` call of a local-dynamic access.
pub const R_390_TLS_LDCALL: u32 = 39;
/// General-dynamic GOT offset, 32 bits (31-bit code).
pub const R_390_TLS_GD32: u32 = 40;
/// General-dynamic GOT offset, 64 bits.
pub const R_390_TLS_GD64: u32 = 41;
/// Initial-exec GOT offset, a 12-bit displacement.
pub const R_390_TLS_GOTIE12: u32 = 42;
/// Initial-exec GOT offset, 32 bits (31-bit code).
pub const R_390_TLS_GOTIE32: u32 = 43;
/// Initial-exec GOT offset, 64 bits.
pub const R_390_TLS_GOTIE64: u32 = 44;
/// Local-dynamic GOT offset, 32 bits (31-bit code).
pub const R_390_TLS_LDM32: u32 = 45;
/// Local-dynamic GOT offset, 64 bits.
pub const R_390_TLS_LDM64: u32 = 46;
/// Initial-exec GOT entry address, 32 bits (31-bit code).
pub const R_390_TLS_IE32: u32 = 47;
/// Initial-exec GOT entry address, 64 bits.
pub const R_390_TLS_IE64: u32 = 48;
/// Initial-exec GOT entry, PC-relative in halfwords.
pub const R_390_TLS_IEENT: u32 = 49;
/// Local-exec offset, 32 bits (31-bit code).
pub const R_390_TLS_LE32: u32 = 50;
/// Local-exec offset, 64 bits.
pub const R_390_TLS_LE64: u32 = 51;
/// Offset in the module's TLS block, 32 bits (31-bit code).
pub const R_390_TLS_LDO32: u32 = 52;
/// Offset in the module's TLS block, 64 bits.
pub const R_390_TLS_LDO64: u32 = 53;
/// Dynamic: the TLS module ID.
pub const R_390_TLS_DTPMOD: u32 = 54;
/// Dynamic: the offset in the module's TLS block.
pub const R_390_TLS_DTPOFF: u32 = 55;
/// Dynamic: the offset from the thread pointer.
pub const R_390_TLS_TPOFF: u32 = 56;
/// `S + A`, a 20-bit displacement.
pub const R_390_20: u32 = 57;
/// `G + A - GOT`, a 20-bit displacement.
pub const R_390_GOT20: u32 = 58;
/// `G + A - GOT` of the `.got.plt` slot, a 20-bit displacement.
pub const R_390_GOTPLT20: u32 = 59;
/// Initial-exec GOT offset, a 20-bit displacement.
pub const R_390_TLS_GOTIE20: u32 = 60;
/// Dynamic: call the resolver at `B + A`.
pub const R_390_IRELATIVE: u32 = 61;
/// `(S + A - P) >> 1`, 12 bits.
pub const R_390_PC12DBL: u32 = 62;
/// `(L + A - P) >> 1`, 12 bits.
pub const R_390_PLT12DBL: u32 = 63;
/// `(S + A - P) >> 1`, 24 bits.
pub const R_390_PC24DBL: u32 = 64;
/// `(L + A - P) >> 1`, 24 bits.
pub const R_390_PLT24DBL: u32 = 65;
/// C++ vtable hierarchy (for `--gc-sections`; ignored).
pub const R_390_GNU_VTINHERIT: u32 = 250;
/// C++ vtable member use (for `--gc-sections`; ignored).
pub const R_390_GNU_VTENTRY: u32 = 251;

/// `PT_S390_PGSTE`: `--s390-pgste` asks the kernel for page tables with
/// guest storage extension (for programs that run virtual machines).
pub const PT_S390_PGSTE: u32 = 0x7000_0000;

/// The name of relocation type `r_type`, as `readelf` prints it.
#[must_use]
pub fn reloc_name(r_type: u32) -> Option<&'static str> {
    const NAMES: [&str; 66] = [
        "R_390_NONE",
        "R_390_8",
        "R_390_12",
        "R_390_16",
        "R_390_32",
        "R_390_PC32",
        "R_390_GOT12",
        "R_390_GOT32",
        "R_390_PLT32",
        "R_390_COPY",
        "R_390_GLOB_DAT",
        "R_390_JMP_SLOT",
        "R_390_RELATIVE",
        "R_390_GOTOFF32",
        "R_390_GOTPC",
        "R_390_GOT16",
        "R_390_PC16",
        "R_390_PC16DBL",
        "R_390_PLT16DBL",
        "R_390_PC32DBL",
        "R_390_PLT32DBL",
        "R_390_GOTPCDBL",
        "R_390_64",
        "R_390_PC64",
        "R_390_GOT64",
        "R_390_PLT64",
        "R_390_GOTENT",
        "R_390_GOTOFF16",
        "R_390_GOTOFF64",
        "R_390_GOTPLT12",
        "R_390_GOTPLT16",
        "R_390_GOTPLT32",
        "R_390_GOTPLT64",
        "R_390_GOTPLTENT",
        "R_390_PLTOFF16",
        "R_390_PLTOFF32",
        "R_390_PLTOFF64",
        "R_390_TLS_LOAD",
        "R_390_TLS_GDCALL",
        "R_390_TLS_LDCALL",
        "R_390_TLS_GD32",
        "R_390_TLS_GD64",
        "R_390_TLS_GOTIE12",
        "R_390_TLS_GOTIE32",
        "R_390_TLS_GOTIE64",
        "R_390_TLS_LDM32",
        "R_390_TLS_LDM64",
        "R_390_TLS_IE32",
        "R_390_TLS_IE64",
        "R_390_TLS_IEENT",
        "R_390_TLS_LE32",
        "R_390_TLS_LE64",
        "R_390_TLS_LDO32",
        "R_390_TLS_LDO64",
        "R_390_TLS_DTPMOD",
        "R_390_TLS_DTPOFF",
        "R_390_TLS_TPOFF",
        "R_390_20",
        "R_390_GOT20",
        "R_390_GOTPLT20",
        "R_390_TLS_GOTIE20",
        "R_390_IRELATIVE",
        "R_390_PC12DBL",
        "R_390_PLT12DBL",
        "R_390_PC24DBL",
        "R_390_PLT24DBL",
    ];
    match r_type {
        R_390_GNU_VTINHERIT => Some("R_390_GNU_VTINHERIT"),
        R_390_GNU_VTENTRY => Some("R_390_GNU_VTENTRY"),
        _ => NAMES.get(usize::try_from(r_type).ok()?).copied(),
    }
}

const fn class(kind: Kind, width: Width) -> Class {
    Class::new(kind, width)
}

const fn got(kind: Kind, width: Width, slot: GotKind) -> Class {
    Class::new(kind, width).through(slot)
}

const fn field(field: Field) -> Width {
    Width::S390(field)
}

/// Whether relocation `r_type` is a call or jump through the PLT.
#[must_use]
pub fn is_branch(r_type: u32) -> bool {
    matches!(
        r_type,
        R_390_PLT12DBL
            | R_390_PLT16DBL
            | R_390_PLT24DBL
            | R_390_PLT32DBL
            | R_390_PLT32
            | R_390_PLT64
    )
}

/// The bytes of `data` from `offset - back`, `len` of them.
fn bytes_at(data: &[u8], offset: u64, back: u64, len: usize) -> Option<&[u8]> {
    let start = usize::try_from(offset.checked_sub(back)?).ok()?;
    data.get(start..start.checked_add(len)?)
}

/// Whether a GOT load may become `larl`, as far as the instruction and the
/// link tell: GNU ld relaxes only in position-independent output, and
/// [`relax_got`] still checks the symbol's address.
fn got_load_relaxes(r_type: u32, data: &[u8], offset: u64, context: ClassifyContext) -> bool {
    if !(context.relax_got && context.pic) {
        return false;
    }
    match r_type {
        R_390_GOTENT => bytes_at(data, offset, 2, 2)
            .and_then(<[u8]>::first_chunk::<2>)
            .is_some_and(|op| insn::is_lgrl(*op)),
        R_390_GOT20 => bytes_at(data, offset, 2, 6).is_some_and(insn::is_lg_got),
        _ => false,
    }
}

/// Classifies relocation `r_type` at `offset` in section `data`.
///
/// # Errors
///
/// [`ClassifyError`] for unsupported types and unrecognized TLS code.
pub fn classify(
    r_type: u32,
    data: &[u8],
    offset: u64,
    context: ClassifyContext,
) -> Result<Class, ClassifyError> {
    use Kind as K;
    use Width as W;
    // The call a TLS marker relaxes must be `brasl %r14,…`.
    let brasl = || {
        if bytes_at(data, offset, 0, 6).is_some_and(insn::is_brasl_r14) {
            Ok(())
        } else {
            Err(ClassifyError::BadTlsInstruction)
        }
    };
    Ok(match r_type {
        R_390_NONE | R_390_GNU_VTINHERIT | R_390_GNU_VTENTRY => class(K::None, W::None),
        R_390_8 => class(K::Abs, W::Any8),
        R_390_12 => class(K::Abs, field(Field::Imm12)),
        R_390_16 => class(K::Abs, W::Any16),
        R_390_20 => class(K::Abs, field(Field::Disp20)),
        R_390_32 => class(K::Abs, W::Any32),
        R_390_64 => class(K::Abs, W::W64),
        R_390_PC16 => class(K::Pc, W::Any16),
        R_390_PC32 | R_390_PLT32 => class(K::Pc, W::Any32),
        R_390_PC64 | R_390_PLT64 => class(K::Pc, W::W64),
        R_390_PC12DBL | R_390_PLT12DBL => class(K::Pc, field(Field::Pc12Dbl)),
        R_390_PC16DBL | R_390_PLT16DBL => class(K::Pc, field(Field::Pc16Dbl)),
        R_390_PC24DBL | R_390_PLT24DBL => class(K::Pc, field(Field::Pc24Dbl)),
        R_390_PC32DBL | R_390_PLT32DBL => class(K::Pc, field(Field::Pc32Dbl)),
        R_390_GOT12 | R_390_GOTPLT12 => class(K::GotSlotRel, field(Field::Imm12)),
        R_390_GOT16 | R_390_GOTPLT16 => class(K::GotSlotRel, W::Any16),
        R_390_GOT20 if got_load_relaxes(r_type, data, offset, context) => {
            class(K::GotRelax, field(Field::Disp20))
        }
        R_390_GOT20 | R_390_GOTPLT20 => class(K::GotSlotRel, field(Field::Disp20)),
        R_390_GOT32 | R_390_GOTPLT32 => class(K::GotSlotRel, W::Any32),
        R_390_GOT64 | R_390_GOTPLT64 => class(K::GotSlotRel, W::W64),
        R_390_GOTENT if got_load_relaxes(r_type, data, offset, context) => {
            class(K::GotRelax, field(Field::Pc32Dbl))
        }
        R_390_GOTENT | R_390_GOTPLTENT => class(K::Got, field(Field::Pc32Dbl)),
        R_390_GOTOFF16 => class(K::GotRel, W::Any16),
        R_390_GOTOFF32 => class(K::GotRel, W::Any32),
        R_390_GOTOFF64 => class(K::GotRel, W::W64),
        R_390_GOTPC => class(K::GotBasePc, W::W64),
        R_390_GOTPCDBL => class(K::GotBasePc, field(Field::Pc32Dbl)),

        // Literal pool entries of TLS sequences.
        R_390_TLS_GD64 => match context.tls {
            TlsMode::Dynamic => got(K::GotSlotRel, W::W64, GotKind::TlsGd),
            TlsMode::InitialExec => class(K::GdToIe, W::W64),
            TlsMode::LocalExec => class(K::GdToLe, W::W64),
        },
        R_390_TLS_LDM64 => match context.tls_ld {
            TlsMode::LocalExec => class(K::LdToLe, W::W64),
            _ => got(K::GotSlotRel, W::W64, GotKind::TlsLd),
        },
        R_390_TLS_LDO64 => class(K::DtpOff, W::W64),
        R_390_TLS_GOTIE64 => match context.tls {
            TlsMode::LocalExec => class(K::IeToLe, W::W64),
            _ => got(K::GotSlotRel, W::W64, GotKind::TpOff),
        },
        R_390_TLS_IE64 => match context.tls {
            TlsMode::LocalExec => class(K::IeToLe, W::W64),
            _ => got(K::GotAbs, W::W64, GotKind::TpOff),
        },
        R_390_TLS_LE64 => class(K::TpOff, W::W64),
        // Initial-exec accesses that load the GOT entry themselves keep it.
        R_390_TLS_GOTIE12 => got(K::GotSlotRel, field(Field::Imm12), GotKind::TpOff),
        R_390_TLS_GOTIE20 => got(K::GotSlotRel, field(Field::Disp20), GotKind::TpOff),
        R_390_TLS_IEENT => got(K::Got, field(Field::Pc32Dbl), GotKind::TpOff),

        // Instruction markers.
        R_390_TLS_LOAD => match context.tls {
            TlsMode::LocalExec => class(K::IeToLe, W::None),
            _ => class(K::None, W::None),
        },
        R_390_TLS_GDCALL => match context.tls {
            TlsMode::Dynamic => class(K::None, W::None),
            TlsMode::InitialExec => {
                brasl()?;
                class(K::GdToIe, W::None)
            }
            TlsMode::LocalExec => {
                brasl()?;
                class(K::GdToLe, W::None)
            }
        },
        R_390_TLS_LDCALL => match context.tls_ld {
            TlsMode::LocalExec => {
                brasl()?;
                class(K::LdToLe, W::None)
            }
            _ => class(K::None, W::None),
        },
        _ => return Err(ClassifyError::Unsupported),
    })
}

fn slot<const N: usize>(out: &mut [u8], at: u64) -> Result<&mut [u8; N], ApplyError> {
    let start = usize::try_from(at).map_err(|_| ApplyError::OutOfBounds)?;
    out.get_mut(start..)
        .and_then(|rest| rest.first_chunk_mut::<N>())
        .ok_or(ApplyError::OutOfBounds)
}

/// Writes `value` into field `field` at `offset`.
///
/// # Errors
///
/// [`ApplyError::Overflow`] when the value does not fit (or is odd where
/// halfwords are counted), [`ApplyError::OutOfBounds`] past the section.
pub fn write_field(
    out: &mut [u8],
    offset: u64,
    field: Field,
    value: u64,
) -> Result<(), ApplyError> {
    let start = usize::try_from(offset).map_err(|_| ApplyError::OutOfBounds)?;
    let end = start
        .checked_add(field.bytes())
        .ok_or(ApplyError::OutOfBounds)?;
    let bytes = out.get_mut(start..end).ok_or(ApplyError::OutOfBounds)?;
    insn::encode(field, bytes, value as i64).map_err(|_| ApplyError::Overflow)
}

/// Writes a GOT load classified [`Kind::GotRelax`]: `larl` of `symbol`
/// when its address is even, as `larl` needs, else the GOT access through
/// `values.got` (the GOT entry's address plus the addend): `G + A - P`
/// for `R_390_GOTENT`, `G + A - GOT` for `R_390_GOT20`.
///
/// # Errors
///
/// [`ApplyError`] when a field does not fit.
pub fn relax_got(
    out: &mut [u8],
    offset: u64,
    r_type: u32,
    symbol: u64,
    values: RelaxValues,
) -> Result<(), ApplyError> {
    let entry = values.got;
    if symbol & 1 != 0 {
        return if r_type == R_390_GOT20 {
            write_field(
                out,
                offset,
                Field::Disp20,
                entry.wrapping_sub(values.got_base),
            )
        } else {
            write_field(
                out,
                offset,
                Field::Pc32Dbl,
                entry.wrapping_sub(values.place),
            )
        };
    }
    // The instruction starts two bytes before the field, and `larl`
    // counts from there.
    let start = offset.checked_sub(2).ok_or(ApplyError::OutOfBounds)?;
    let opcode = slot::<2>(out, start)?;
    *opcode = insn::larl(opcode[1]);
    write_field(
        out,
        offset,
        Field::Pc32Dbl,
        symbol.wrapping_add(2).wrapping_sub(values.place),
    )
}

/// Relaxes a TLS access to local-exec or initial-exec: rewrites the
/// instruction a marker (`R_390_TLS_GDCALL`, `LDCALL`, `LOAD`) is on, or
/// stores the relaxed value of a literal pool entry.
///
/// # Errors
///
/// [`ApplyError`] for unrecognized instructions.
pub fn relax_tls(
    out: &mut [u8],
    offset: u64,
    kind: Kind,
    r_type: u32,
    values: RelaxValues,
) -> Result<(), ApplyError> {
    let word = |out: &mut [u8], value: u64| -> Result<(), ApplyError> {
        *slot::<8>(out, offset)? = value.to_be_bytes();
        Ok(())
    };
    let call = |out: &mut [u8], code: [u8; 6]| -> Result<(), ApplyError> {
        let insn = slot::<6>(out, offset)?;
        if !insn::is_brasl_r14(insn) {
            return Err(ApplyError::BadInstruction);
        }
        *insn = code;
        Ok(())
    };
    match (kind, r_type) {
        // The literal pool entries.
        (Kind::GdToIe, R_390_TLS_GD64) => word(out, values.got.wrapping_sub(values.got_base)),
        (Kind::GdToLe | Kind::IeToLe, R_390_TLS_GD64 | R_390_TLS_GOTIE64 | R_390_TLS_IE64) => {
            word(out, values.tpoff as u64)
        }
        // The module's block starts at the thread pointer's offset 0 plus
        // what the `@dtpoff` entries hold, which are relaxed to thread
        // pointer offsets.
        (Kind::LdToLe, R_390_TLS_LDM64) => word(out, 0),
        // The calls to `__tls_get_offset`.
        (Kind::GdToIe, R_390_TLS_GDCALL) => call(out, insn::LG_R2_GOT),
        (Kind::GdToLe, R_390_TLS_GDCALL) | (Kind::LdToLe, R_390_TLS_LDCALL) => {
            call(out, insn::BRCL_NOP)
        }
        // The load of the offset.
        (Kind::IeToLe, R_390_TLS_LOAD) => {
            let load = slot::<6>(out, offset)?;
            *load = insn::ie_load_to_le(*load).ok_or(ApplyError::BadInstruction)?;
            Ok(())
        }
        _ => Err(ApplyError::BadInstruction),
    }
}

/// Size of the PLT header.
pub const PLT_HEADER_SIZE: u64 = insn::PLT_HEADER_SIZE;
/// Size of a PLT entry and of an IFUNC stub.
pub const PLT_ENTRY_SIZE: u64 = insn::PLT_ENTRY_SIZE;

/// Writes the PLT header at address `plt`, which reaches the dynamic
/// linker's words at the GOT pointer `got`.
///
/// # Errors
///
/// [`ApplyError::Overflow`] when the GOT is out of reach.
pub fn write_plt_header(out: &mut [u8], plt: u64, got: u64) -> Result<(), ApplyError> {
    let code = insn::plt_header(plt, got).map_err(|_| ApplyError::Overflow)?;
    *slot::<32>(out, 0)? = code;
    Ok(())
}

/// Writes PLT entry `index` at `entry`, which jumps through `.got.plt` slot
/// `slot` and falls back to the header at `plt`.
///
/// # Errors
///
/// [`ApplyError::Overflow`] when out of reach.
pub fn write_plt_entry(
    out: &mut [u8],
    entry: u64,
    slot_address: u64,
    index: u32,
    plt: u64,
) -> Result<(), ApplyError> {
    // The byte offset of the entry's relocation in `.rela.plt`.
    let rela = index.checked_mul(24).ok_or(ApplyError::Overflow)?;
    let code = insn::plt_entry(entry, slot_address, plt, rela).map_err(|_| ApplyError::Overflow)?;
    *slot::<32>(out, 0)? = code;
    Ok(())
}

/// Writes the IFUNC stub at `stub` of a static executable, which jumps
/// through the slot at `slot_address` (filled by `R_390_IRELATIVE`). GNU
/// ld writes a whole PLT entry; the lazy half is never reached, and here
/// branches to itself.
///
/// # Errors
///
/// [`ApplyError::Overflow`] when out of reach.
pub fn write_iplt(out: &mut [u8], stub: u64, slot_address: u64) -> Result<(), ApplyError> {
    let code = insn::plt_entry(stub, slot_address, stub.wrapping_add(22), 0)
        .map_err(|_| ApplyError::Overflow)?;
    *slot::<32>(out, 0)? = code;
    Ok(())
}

/// Fills `out` with no-op instructions.
pub fn write_nops(out: &mut [u8]) {
    insn::write_nops(out);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(tls: TlsMode, pic: bool) -> ClassifyContext {
        ClassifyContext {
            relax_got: true,
            pic,
            tls,
            tls_ld: tls,
            code: true,
        }
    }

    #[test]
    fn names() {
        assert_eq!(reloc_name(R_390_PC32DBL), Some("R_390_PC32DBL"));
        assert_eq!(reloc_name(R_390_PLT24DBL), Some("R_390_PLT24DBL"));
        assert_eq!(reloc_name(R_390_GNU_VTENTRY), Some("R_390_GNU_VTENTRY"));
        assert_eq!(reloc_name(66), None);
    }

    fn values(got: u64, place: u64, got_base: u64) -> RelaxValues {
        RelaxValues {
            got,
            place,
            got_base,
            ..RelaxValues::default()
        }
    }

    #[test]
    fn got_loads_relax_only_in_pic() {
        // lgrl %r2,sym@GOTENT
        let data = [0xc4, 0x28, 0, 0, 0, 0];
        let pie = context(TlsMode::LocalExec, true);
        let exe = context(TlsMode::LocalExec, false);
        assert_eq!(
            classify(R_390_GOTENT, &data, 2, pie).unwrap().kind,
            Kind::GotRelax
        );
        assert_eq!(
            classify(R_390_GOTENT, &data, 2, exe).unwrap().kind,
            Kind::Got
        );
        // lgrl -> larl, and an odd symbol keeps the GOT load.
        let mut out = data;
        relax_got(&mut out, 2, R_390_GOTENT, 0x1000, values(0x842, 0x802, 0)).unwrap();
        assert_eq!(out, [0xc0, 0x20, 0, 0, 0x04, 0x00]);
        let mut out = data;
        relax_got(&mut out, 2, R_390_GOTENT, 0x1001, values(0x842, 0x802, 0)).unwrap();
        assert_eq!(out, [0xc4, 0x28, 0, 0, 0, 0x20]);
        // lg %r1,sym@GOT(%r12) -> larl %r1,sym
        let lg = [0xe3, 0x10, 0xc0, 0x00, 0x00, 0x04];
        assert_eq!(
            classify(R_390_GOT20, &lg, 2, pie).unwrap().kind,
            Kind::GotRelax
        );
        let mut out = lg;
        relax_got(
            &mut out,
            2,
            R_390_GOT20,
            0x1000,
            values(0x2040, 0x802, 0x2000),
        )
        .unwrap();
        assert_eq!(out, [0xc0, 0x10, 0, 0, 0x04, 0x00]);
    }

    #[test]
    fn tls_markers() {
        let brasl = [0xc0, 0xe5, 0, 0, 0, 0];
        let le = context(TlsMode::LocalExec, false);
        let ie = context(TlsMode::InitialExec, true);
        let dynamic = context(TlsMode::Dynamic, true);
        assert_eq!(
            classify(R_390_TLS_GDCALL, &brasl, 0, le).unwrap().kind,
            Kind::GdToLe
        );
        assert_eq!(
            classify(R_390_TLS_GDCALL, &brasl, 0, ie).unwrap().kind,
            Kind::GdToIe
        );
        assert_eq!(
            classify(R_390_TLS_GDCALL, &brasl, 0, dynamic).unwrap().kind,
            Kind::None
        );
        assert_eq!(
            classify(R_390_TLS_GDCALL, &[0; 6], 0, le),
            Err(ClassifyError::BadTlsInstruction)
        );
        let mut out = brasl;
        relax_tls(
            &mut out,
            0,
            Kind::GdToIe,
            R_390_TLS_GDCALL,
            RelaxValues::default(),
        )
        .unwrap();
        assert_eq!(out, insn::LG_R2_GOT);
        let mut pool = [0u8; 8];
        let values = RelaxValues {
            tpoff: -16,
            ..RelaxValues::default()
        };
        relax_tls(&mut pool, 0, Kind::GdToLe, R_390_TLS_GD64, values).unwrap();
        assert_eq!(pool, (-16i64).to_be_bytes());
        let values = RelaxValues {
            got: 0x2018,
            got_base: 0x2000,
            ..RelaxValues::default()
        };
        relax_tls(&mut pool, 0, Kind::GdToIe, R_390_TLS_GD64, values).unwrap();
        assert_eq!(pool, 0x18u64.to_be_bytes());
        assert_eq!(
            classify(R_390_TLS_GD32, &pool, 0, le),
            Err(ClassifyError::Unsupported)
        );
    }
}
