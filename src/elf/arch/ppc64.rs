//! PowerPC64 little-endian (ELFv2) relocations: classification, the TOC,
//! call stubs, the TLS relaxations and the PLT.
//!
//! [`classify`] turns a relocation type into a [`Class`]: what the ABI
//! says to compute ([`Kind`]), which GOT entry it reads ([`GotKind`]) and
//! which instruction field holds the result ([`Field`]). The relocation
//! scan and the writer both use it, so they always agree.
//!
//! **The TOC.** Code addresses data relative to `r2`, the TOC pointer,
//! which is `.got + 0x8000` (the symbol `.TOC.`); the first `.got` word
//! holds that value. TOC-relative relocations (`TOC16*`) compute
//! `S + A - .TOC.` and GOT-indirect ones (`GOT16*`) `G + A - .TOC.`, which
//! is what [`Kind::GotRel`] and [`Kind::GotSlotRel`] compute on this
//! architecture, whose GOT base is the TOC pointer. As in GNU ld and lld,
//! an `addis rT, r2, x@ha` whose `#ha` is zero becomes a `nop`, and the
//! instruction that used `rT` uses `r2` instead ([`Field::HaToc`] and its
//! companions). An access through a compiler-generated `.toc` entry that
//! holds the address of a non-preemptible symbol reads the symbol's address
//! TOC-relative instead of loading it ([`toc_indirection`]).
//!
//! **Calls.** A function has a global entry point, which computes `r2`
//! from `r12`, and a local entry point after it (`st_other` bits 5-7). A
//! `bl` to a function of the same output enters it at the local entry
//! point, since `r2` is already right. A call to a preemptible function
//! goes through a PLT call stub in `.plt.sec` that saves `r2` in the
//! caller's frame and loads the target from its `.got.plt` slot; the `nop`
//! after the `bl` becomes `ld r2, 24(r1)` to restore it. `bl` reaches
//! ±32 MiB; farther branches go through the range-extension thunks of
//! [`crate::elf::arch::thunk`], which compute their target from the program
//! counter and so also serve Power10 callers that do not maintain `r2`
//! (`R_PPC64_REL24_NOTOC`): through one they call a function needing the
//! TOC at its global entry point, or a PLT entry by loading its word
//! PC-relatively. A caller that keeps `r2` calls a function that clobbers
//! it (`st_other` 1) through a thunk that saves `r2` first, restored by
//! the `nop` after the call as for PLT calls. Every called preemptible
//! function gets its own PLT slot (no `.plt.got`), and a dynamic output's
//! `IRELATIVE` relocations go to `.rela.dyn`, as with GNU ld.
//!
//! **The PLT.** `.plt` is the lazy-binding code the ABI calls `.glink`: a
//! 60-byte resolver, then one `b` back to it per PLT slot. `.got.plt` holds
//! the slots after two words the dynamic linker fills (the resolver and the
//! link map), and `DT_PPC64_GLINK` points 32 bytes before the first lazy
//! entry, as glibc expects. The dynamic linker initializes every slot
//! itself, so the slots are left zero.
//!
//! **TLS** follows the ELFv2 ABI and lld: in an executable,
//! general-dynamic and local-dynamic accesses to the executable's own
//! variables become local-exec, general-dynamic accesses to a shared
//! library's variables become initial-exec, and initial-exec becomes
//! local-exec. The `R_PPC64_TLSGD`/`R_PPC64_TLSLD` marker on the
//! `bl __tls_get_addr` rewrites the call, whose own relocation is then
//! skipped. The thread pointer (`r13`) is 0x7000 bytes past the start of
//! the TLS block, and `@dtprel` offsets are biased by 0x8000.

#![deny(clippy::arithmetic_side_effects)]

use crate::arch::ppc64::{
    self as insn, ADD_R3_R3_R13, ADDI_R3_R3, ADDI_R3_R3_4096, ADDIS_R3_R13, ADDIS_R13, Field,
    LD_R2_24_R1, NOP, PADDI_R3_R13, PADDI_R3_R13_4096, PADDI_R13, read_insn, read_prefixed,
    write_insn, write_prefixed,
};
use crate::elf::read::consts::ppc64::*;
use crate::elf::read::{Relocation, Relocations};

use super::{
    ApplyError, Branch, Class, ClassifyContext, ClassifyError, GotKind, Kind, RelaxValues, TlsMode,
    Width,
};

const fn class(kind: Kind, field: Field) -> Class {
    Class::new(kind, Width::Ppc(field))
}

const fn got(kind: Kind, field: Field, slot: GotKind) -> Class {
    Class::new(kind, Width::Ppc(field)).through(slot)
}

const fn relax(kind: Kind) -> Class {
    Class::new(kind, Width::None)
}

/// Whether the prefixed instruction at `offset` of `data` is a `pld`,
/// which a GOT-indirect access can turn into a `paddi`.
fn is_pld(data: &[u8], offset: u64) -> bool {
    usize::try_from(offset)
        .ok()
        .and_then(|at| read_prefixed(data, at))
        .is_some_and(|insn| insn & 0xfc00_0000 == 0xe400_0000)
}

/// Classifies PowerPC64 relocation `r_type` at `offset` in section `data`.
///
/// # Errors
///
/// [`ClassifyError::Unsupported`] for types qld does not link: the
/// dynamic-only types, ELFv1 function descriptors (`R_PPC64_TOC`), the
/// inline PLT sequences (`PLTSEQ`, `PLTCALL`, `PLT16_*`, `PLT_PCREL34`),
/// the section-relative and 34-bit absolute forms, and
/// `R_PPC64_GOT_DTPREL*`; [`ClassifyError::BadTlsInstruction`] for a TLS
/// access in a form that cannot be relaxed (the `_HI` halves).
#[allow(clippy::too_many_lines)]
// Out of line: large, and not to be inlined into the other architectures'
// relocation loops.
#[inline(never)]
pub fn classify(
    r_type: u32,
    data: &[u8],
    offset: u64,
    context: ClassifyContext,
) -> Result<Class, ClassifyError> {
    use Field as F;
    use Kind as K;
    let tls = context.tls;
    let tls_ld = context.tls_ld;
    let r_type = base_type(r_type);
    Ok(match r_type {
        R_PPC64_NONE
        | R_PPC64_TOCSAVE
        | R_PPC64_ENTRY
        | R_PPC64_GNU_VTINHERIT
        | R_PPC64_GNU_VTENTRY => Class::new(K::None, Width::None),

        // Absolute data and instruction fields.
        R_PPC64_ADDR64 | R_PPC64_UADDR64 => Class::new(K::Abs, Width::W64),
        R_PPC64_ADDR32 | R_PPC64_UADDR32 => Class::new(K::Abs, Width::Any32),
        R_PPC64_ADDR16 | R_PPC64_UADDR16 => class(K::Abs, F::Half16),
        R_PPC64_ADDR16_LO => class(K::Abs, F::Lo),
        R_PPC64_ADDR16_HI => class(K::Abs, F::Hi),
        R_PPC64_ADDR16_HA => class(K::Abs, F::Ha),
        R_PPC64_ADDR16_HIGH => class(K::Abs, F::High),
        R_PPC64_ADDR16_HIGHA => class(K::Abs, F::Higha),
        R_PPC64_ADDR16_HIGHER => class(K::Abs, F::Higher),
        R_PPC64_ADDR16_HIGHERA => class(K::Abs, F::Highera),
        R_PPC64_ADDR16_HIGHEST => class(K::Abs, F::Highest),
        R_PPC64_ADDR16_HIGHESTA => class(K::Abs, F::Highesta),
        R_PPC64_ADDR16_DS => class(K::Abs, F::Ds),
        R_PPC64_ADDR16_LO_DS => class(K::Abs, F::LoDs),
        R_PPC64_ADDR24 => class(K::Abs, F::Addr24),
        R_PPC64_ADDR14 | R_PPC64_ADDR14_BRTAKEN | R_PPC64_ADDR14_BRNTAKEN => {
            class(K::Abs, F::Addr14)
        }

        // PC-relative data, halves and branches.
        R_PPC64_REL64 => Class::new(K::Pc, Width::W64),
        R_PPC64_REL32 => Class::new(K::Pc, Width::I32),
        R_PPC64_REL16 => class(K::Pc, F::Half16Signed),
        R_PPC64_REL16_LO => class(K::Pc, F::Lo),
        R_PPC64_REL16_HI => class(K::Pc, F::Hi),
        R_PPC64_REL16_HA => class(K::Pc, F::Ha),
        R_PPC64_REL16_HIGH => class(K::Pc, F::High),
        R_PPC64_REL16_HIGHA => class(K::Pc, F::Higha),
        R_PPC64_REL16_HIGHER => class(K::Pc, F::Higher),
        R_PPC64_REL16_HIGHERA => class(K::Pc, F::Highera),
        R_PPC64_REL16_HIGHEST => class(K::Pc, F::Highest),
        R_PPC64_REL16_HIGHESTA => class(K::Pc, F::Highesta),
        R_PPC64_REL24 | R_PPC64_REL24_NOTOC => class(K::Pc, F::Rel24),
        R_PPC64_REL14 | R_PPC64_REL14_BRTAKEN | R_PPC64_REL14_BRNTAKEN => class(K::Pc, F::Rel14),
        R_PPC64_PCREL34 => class(K::Pc, F::Prefixed34),

        // TOC-relative: `S + A - .TOC.`.
        R_PPC64_TOC16 => class(K::GotRel, F::Half16),
        R_PPC64_TOC16_LO => class(K::GotRel, F::LoToc),
        R_PPC64_TOC16_HI => class(K::GotRel, F::Hi),
        R_PPC64_TOC16_HA => class(K::GotRel, F::HaToc),
        R_PPC64_TOC16_DS => class(K::GotRel, F::Ds),
        R_PPC64_TOC16_LO_DS => class(K::GotRel, F::LoDsToc),

        // GOT-indirect: `G + A - .TOC.`, or PC-relative.
        R_PPC64_GOT16 => got(K::GotSlotRel, F::Half16, GotKind::Address),
        R_PPC64_GOT16_LO => got(K::GotSlotRel, F::Lo, GotKind::Address),
        R_PPC64_GOT16_HI => got(K::GotSlotRel, F::Hi, GotKind::Address),
        R_PPC64_GOT16_HA => got(K::GotSlotRel, F::HaToc, GotKind::Address),
        R_PPC64_GOT16_DS => got(K::GotSlotRel, F::Ds, GotKind::Address),
        R_PPC64_GOT16_LO_DS => got(K::GotSlotRel, F::LoDsToc, GotKind::Address),
        R_PPC64_GOT_PCREL34 => {
            if context.relax_got && is_pld(data, offset) {
                class(K::Pc, F::PldToPaddi)
            } else {
                got(K::Got, F::Prefixed34, GotKind::Address)
            }
        }
        // A hint on the `pld` of a relaxable GOT access: the load or store
        // that uses the address (`A` bytes on) becomes PC-relative.
        // It has no symbol: it applies when that relocation relaxed.
        R_PPC64_PCREL_OPT => class(K::Addend, F::PcrelOpt),

        // Local-exec and local-dynamic offsets.
        R_PPC64_TPREL64 => Class::new(K::TpOff, Width::W64),
        R_PPC64_TPREL16 => class(K::TpOff, F::Half16Signed),
        R_PPC64_TPREL16_LO => class(K::TpOff, F::Lo),
        R_PPC64_TPREL16_HI => class(K::TpOff, F::Hi),
        R_PPC64_TPREL16_HA => class(K::TpOff, F::Ha),
        R_PPC64_TPREL16_HIGH => class(K::TpOff, F::High),
        R_PPC64_TPREL16_HIGHA => class(K::TpOff, F::Higha),
        R_PPC64_TPREL16_HIGHER => class(K::TpOff, F::Higher),
        R_PPC64_TPREL16_HIGHERA => class(K::TpOff, F::Highera),
        R_PPC64_TPREL16_HIGHEST => class(K::TpOff, F::Highest),
        R_PPC64_TPREL16_HIGHESTA => class(K::TpOff, F::Highesta),
        R_PPC64_TPREL16_DS => class(K::TpOff, F::Ds),
        R_PPC64_TPREL16_LO_DS => class(K::TpOff, F::LoDs),
        R_PPC64_TPREL34 => class(K::TpOff, F::Prefixed34),
        R_PPC64_DTPREL64 => Class::new(K::DtpOff, Width::W64),
        R_PPC64_DTPREL16 => class(K::DtpOff, F::Half16Signed),
        R_PPC64_DTPREL16_LO => class(K::DtpOff, F::Lo),
        R_PPC64_DTPREL16_HI => class(K::DtpOff, F::Hi),
        R_PPC64_DTPREL16_HA => class(K::DtpOff, F::Ha),
        R_PPC64_DTPREL16_HIGH => class(K::DtpOff, F::High),
        R_PPC64_DTPREL16_HIGHA => class(K::DtpOff, F::Higha),
        R_PPC64_DTPREL16_HIGHER => class(K::DtpOff, F::Higher),
        R_PPC64_DTPREL16_HIGHERA => class(K::DtpOff, F::Highera),
        R_PPC64_DTPREL16_HIGHEST => class(K::DtpOff, F::Highest),
        R_PPC64_DTPREL16_HIGHESTA => class(K::DtpOff, F::Highesta),
        R_PPC64_DTPREL16_DS => class(K::DtpOff, F::Ds),
        R_PPC64_DTPREL16_LO_DS => class(K::DtpOff, F::LoDs),
        R_PPC64_DTPREL34 => class(K::DtpOff, F::Prefixed34),

        // General-dynamic.
        R_PPC64_GOT_TLSGD16 | R_PPC64_GOT_TLSGD16_LO => match tls {
            TlsMode::Dynamic => {
                let field = if r_type == R_PPC64_GOT_TLSGD16 {
                    F::Half16
                } else {
                    F::Lo
                };
                got(K::GotSlotRel, field, GotKind::TlsGd)
            }
            TlsMode::LocalExec => relax(K::GdToLe),
            TlsMode::InitialExec => got(K::GotSlotRel, F::LdR3LoDs, GotKind::TpOff),
        },
        R_PPC64_GOT_TLSGD16_HA => match tls {
            TlsMode::Dynamic => got(K::GotSlotRel, F::Ha, GotKind::TlsGd),
            TlsMode::LocalExec => relax(K::GdToLe),
            TlsMode::InitialExec => got(K::GotSlotRel, F::Ha, GotKind::TpOff),
        },
        R_PPC64_GOT_TLSGD16_HI => match tls {
            TlsMode::Dynamic => got(K::GotSlotRel, F::Hi, GotKind::TlsGd),
            _ => return Err(ClassifyError::BadTlsInstruction),
        },
        R_PPC64_GOT_TLSGD_PCREL34 => match tls {
            TlsMode::Dynamic => got(K::Got, F::Prefixed34, GotKind::TlsGd),
            TlsMode::LocalExec => relax(K::GdToLe),
            TlsMode::InitialExec => got(K::Got, F::PldR3, GotKind::TpOff),
        },
        R_PPC64_TLSGD => match tls {
            TlsMode::Dynamic => Class::new(K::None, Width::None),
            TlsMode::LocalExec => relax(K::GdToLe).skipping(),
            TlsMode::InitialExec => relax(K::GdToIe).skipping(),
        },

        // Local-dynamic.
        R_PPC64_GOT_TLSLD16 | R_PPC64_GOT_TLSLD16_LO | R_PPC64_GOT_TLSLD16_HA => match tls_ld {
            TlsMode::Dynamic => {
                let field = match r_type {
                    R_PPC64_GOT_TLSLD16 => F::Half16,
                    R_PPC64_GOT_TLSLD16_LO => F::Lo,
                    _ => F::Ha,
                };
                got(K::GotSlotRel, field, GotKind::TlsLd)
            }
            _ => relax(K::LdToLe),
        },
        R_PPC64_GOT_TLSLD16_HI => match tls_ld {
            TlsMode::Dynamic => got(K::GotSlotRel, F::Hi, GotKind::TlsLd),
            _ => return Err(ClassifyError::BadTlsInstruction),
        },
        R_PPC64_GOT_TLSLD_PCREL34 => match tls_ld {
            TlsMode::Dynamic => got(K::Got, F::Prefixed34, GotKind::TlsLd),
            _ => relax(K::LdToLe),
        },
        R_PPC64_TLSLD => match tls_ld {
            TlsMode::Dynamic => Class::new(K::None, Width::None),
            _ => relax(K::LdToLe).skipping(),
        },

        // Initial-exec.
        R_PPC64_GOT_TPREL16_HA => match tls {
            TlsMode::LocalExec => relax(K::IeToLe),
            _ => got(K::GotSlotRel, F::Ha, GotKind::TpOff),
        },
        R_PPC64_GOT_TPREL16_LO_DS => match tls {
            TlsMode::LocalExec => relax(K::IeToLe),
            _ => got(K::GotSlotRel, F::LoDs, GotKind::TpOff),
        },
        R_PPC64_GOT_TPREL16_DS => match tls {
            TlsMode::LocalExec => relax(K::IeToLe),
            _ => got(K::GotSlotRel, F::Ds, GotKind::TpOff),
        },
        R_PPC64_GOT_TPREL16_HI => match tls {
            TlsMode::LocalExec => return Err(ClassifyError::BadTlsInstruction),
            _ => got(K::GotSlotRel, F::Hi, GotKind::TpOff),
        },
        R_PPC64_GOT_TPREL_PCREL34 => match tls {
            TlsMode::LocalExec => relax(K::IeToLe),
            _ => got(K::Got, F::Prefixed34, GotKind::TpOff),
        },
        R_PPC64_TLS => match tls {
            TlsMode::LocalExec => relax(K::IeToLe),
            _ => Class::new(K::None, Width::None),
        },

        _ => return Err(ClassifyError::Unsupported),
    })
}

fn at(offset: u64) -> Result<usize, ApplyError> {
    usize::try_from(offset).map_err(|_| ApplyError::OutOfBounds)
}

fn get(out: &[u8], offset: u64) -> Result<u32, ApplyError> {
    read_insn(out, at(offset)?).ok_or(ApplyError::OutOfBounds)
}

fn put(out: &mut [u8], offset: u64, value: u32) -> Result<(), ApplyError> {
    write_insn(out, at(offset)?, value).ok_or(ApplyError::OutOfBounds)
}

fn get_prefixed(out: &[u8], offset: u64) -> Result<u64, ApplyError> {
    read_prefixed(out, at(offset)?).ok_or(ApplyError::OutOfBounds)
}

fn put_prefixed(out: &mut [u8], offset: u64, value: u64) -> Result<(), ApplyError> {
    write_prefixed(out, at(offset)?, value).ok_or(ApplyError::OutOfBounds)
}

fn encode_error(error: insn::EncodeError) -> ApplyError {
    match error {
        insn::EncodeError::Overflow => ApplyError::Overflow,
        insn::EncodeError::BadInstruction => ApplyError::BadInstruction,
    }
}

/// Writes `field` of the instruction at `offset` with `value`.
fn patch(
    out: &mut [u8],
    offset: u64,
    insn: u32,
    field: Field,
    value: i64,
) -> Result<(), ApplyError> {
    let encoded = if field.bytes() == 2 {
        let half = field.encode16(insn as u16, value).map_err(encode_error)?;
        (insn & 0xffff_0000) | u32::from(half)
    } else {
        field.encode32(insn, value).map_err(encode_error)?
    };
    put(out, offset, encoded)
}

/// The ABI version the output's `e_flags` records: ELFv2.
pub const ABI_VERSION: u32 = 2;

/// Set in the type of an `R_PPC64_TLSGD`/`R_PPC64_TLSLD` marker (by
/// [`annotate`]) whose `__tls_get_addr` call is PC-relative
/// (`R_PPC64_REL24_NOTOC`): that call has no `nop` after it to rewrite.
pub const PCREL_CALL_HINT: u32 = 1 << 31;

/// The type without [`PCREL_CALL_HINT`].
#[must_use]
pub const fn base_type(r_type: u32) -> u32 {
    r_type & !PCREL_CALL_HINT
}

/// `rel` with [`PCREL_CALL_HINT`] set when it is a `__tls_get_addr` marker
/// on a PC-relative call, which the relocation that follows (`next`)
/// tells.
#[must_use]
pub fn annotate(rel: Relocation, next: Option<&Relocation>) -> Relocation {
    if matches!(rel.r_type, R_PPC64_TLSGD | R_PPC64_TLSLD)
        && next.is_some_and(|next| next.r_type == R_PPC64_REL24_NOTOC)
    {
        return Relocation {
            r_type: rel.r_type | PCREL_CALL_HINT,
            ..rel
        };
    }
    rel
}

/// Rewrites one instruction of a relaxed TLS sequence.
///
/// # Errors
///
/// [`ApplyError`] for sequences qld cannot rewrite, and for offsets that
/// do not fit the replacement instructions.
#[allow(clippy::too_many_lines)]
pub fn relax_tls(
    out: &mut [u8],
    offset: u64,
    kind: Kind,
    r_type: u32,
    values: RelaxValues,
) -> Result<(), ApplyError> {
    let tpoff = values.tpoff;
    let pcrel = r_type & PCREL_CALL_HINT != 0;
    match (kind, base_type(r_type)) {
        (Kind::GdToLe, R_PPC64_GOT_TLSGD16_HA)
        | (Kind::LdToLe, R_PPC64_GOT_TLSLD16_HA)
        | (Kind::IeToLe, R_PPC64_GOT_TPREL16_HA) => put(out, offset, NOP),
        // addi r3, r3, x@got@tlsgd@l -> addis r3, r13, x@tprel@ha
        (Kind::GdToLe, R_PPC64_GOT_TLSGD16 | R_PPC64_GOT_TLSGD16_LO) => {
            patch(out, offset, ADDIS_R3_R13, Field::Ha, tpoff)
        }
        // addi r3, r3, x@got@tlsld@l -> addis r3, r13, 0
        (Kind::LdToLe, R_PPC64_GOT_TLSLD16 | R_PPC64_GOT_TLSLD16_LO) => {
            put(out, offset, ADDIS_R3_R13)
        }
        // paddi r3, 0, x@got@tlsgd@pcrel, 1 -> paddi r3, r13, x@tprel, 0
        (Kind::GdToLe, R_PPC64_GOT_TLSGD_PCREL34) => {
            let insn = insn::prefixed34(PADDI_R3_R13, tpoff).map_err(encode_error)?;
            put_prefixed(out, offset, insn)
        }
        // paddi r3, 0, x@got@tlsld@pcrel, 1 -> paddi r3, r13, 0x1000, 0
        (Kind::LdToLe, R_PPC64_GOT_TLSLD_PCREL34) => put_prefixed(out, offset, PADDI_R3_R13_4096),
        // bl __tls_get_addr(x@tlsgd); nop -> nop; addi r3, r3, x@tprel@l
        (Kind::GdToLe, R_PPC64_TLSGD) => {
            put(out, offset, NOP)?;
            if pcrel {
                return Ok(());
            }
            patch(out, offset.wrapping_add(4), ADDI_R3_R3, Field::Lo, tpoff)
        }
        // bl __tls_get_addr(x@tlsld); nop -> nop; addi r3, r3, 4096
        (Kind::LdToLe, R_PPC64_TLSLD) => {
            put(out, offset, NOP)?;
            if pcrel {
                return Ok(());
            }
            put(out, offset.wrapping_add(4), ADDI_R3_R3_4096)
        }
        // bl __tls_get_addr(x@tlsgd); nop -> nop; add r3, r3, r13
        (Kind::GdToIe, R_PPC64_TLSGD) => {
            if pcrel {
                return put(out, offset, ADD_R3_R3_R13);
            }
            put(out, offset, NOP)?;
            put(out, offset.wrapping_add(4), ADD_R3_R3_R13)
        }
        // ld rT, x@got@tprel@l(rA) -> addis rT, r13, x@tprel@ha
        (Kind::IeToLe, R_PPC64_GOT_TPREL16_LO_DS | R_PPC64_GOT_TPREL16_DS) => {
            let rt = get(out, offset)? & 0x03e0_0000;
            patch(out, offset, ADDIS_R13 | rt, Field::Ha, tpoff)
        }
        // pld rT, x@got@tprel@pcrel -> paddi rT, r13, x@tprel, 0
        (Kind::IeToLe, R_PPC64_GOT_TPREL_PCREL34) => {
            let rt = get_prefixed(out, offset)? & 0x03e0_0000;
            let insn = insn::prefixed34(PADDI_R13 | rt, tpoff).map_err(encode_error)?;
            put_prefixed(out, offset, insn)
        }
        (Kind::IeToLe, R_PPC64_TLS) => relax_tls_marker(out, offset, tpoff),
        _ => Err(ApplyError::BadInstruction),
    }
}

/// The initial-exec → local-exec rewrite of the instruction an
/// `R_PPC64_TLS` marks: the X-form access that adds `r13` becomes the
/// D-form one with `x@tprel@l` as its displacement. The PC-relative form
/// marks the instruction one byte before it, and needs no displacement
/// because the `paddi` before it computed the whole address.
fn relax_tls_marker(out: &mut [u8], offset: u64, tpoff: i64) -> Result<(), ApplyError> {
    match offset & 3 {
        0 => {
            let (d_form, ds) =
                insn::x_to_d_form(get(out, offset)?).ok_or(ApplyError::BadInstruction)?;
            let field = if ds { Field::LoDs } else { Field::Lo };
            patch(out, offset, d_form, field, tpoff)
        }
        1 => {
            let offset = offset.wrapping_sub(1);
            let old = get(out, offset)?;
            if insn::primary_opcode(old) == 31 && (old >> 1) & 0x3ff == 266 {
                // add rT, rA, r13: the address is already in rA.
                let rt = (old >> 21) & 0x1f;
                let ra = (old >> 16) & 0x1f;
                let replacement = if rt == ra {
                    NOP
                } else {
                    // mr rT, rA
                    0x7c00_0378 | (rt << 16) | (ra << 21) | (ra << 11)
                };
                return put(out, offset, replacement);
            }
            let (d_form, _) = insn::x_to_d_form(old).ok_or(ApplyError::BadInstruction)?;
            put(out, offset, d_form)
        }
        _ => Err(ApplyError::BadInstruction),
    }
}

/// Relaxes an `R_PPC64_PCREL_OPT` pair at `offset`, after the
/// `R_PPC64_GOT_PCREL34` there turned its `pld` into `paddi rX, sym`: the
/// load or store `addend` bytes on that used `rX` becomes the prefixed
/// PC-relative access of `sym` in its place, and itself a `nop`. When the
/// GOT access was not relaxed, or the displacement does not fit, both stay
/// as they are.
///
/// # Errors
///
/// [`ApplyError::BadInstruction`] when the second instruction has no
/// PC-relative form.
pub fn relax_pcrel_opt(out: &mut [u8], offset: u64, addend: i64) -> Result<(), ApplyError> {
    let paddi = get_prefixed(out, offset)?;
    // paddi rX, 0, sym@pcrel, 1
    if paddi & 0xff10_0000_fc1f_0000 != 0x0610_0000_3800_0000 {
        return Ok(());
    }
    let access_at = offset.wrapping_add_signed(addend);
    let access = get(out, access_at)?;
    let form = insn::pcrel_form(access).ok_or(ApplyError::BadInstruction)?;
    let total = insn::total_displacement(paddi, access);
    let Ok(relaxed) = insn::prefixed34(form, total) else {
        return Ok(());
    };
    put_prefixed(out, offset, relaxed)?;
    put(out, access_at, NOP)
}

/// The address a direct branch jumps to: a `bl` from code that keeps the
/// TOC pointer enters a function of this output at its local entry point.
#[must_use]
pub fn branch_destination(branch: Branch) -> u64 {
    if branch.r_type == R_PPC64_REL24 && !branch.via_stub {
        return branch
            .target
            .wrapping_add(insn::local_entry_offset(branch.st_other));
    }
    branch.target
}

/// Whether relocation `r_type` is a branch that range-extension thunks
/// serve.
#[must_use]
pub fn is_thunk_branch(r_type: u32) -> bool {
    matches!(r_type, R_PPC64_REL24 | R_PPC64_REL24_NOTOC)
}

/// The key of the thunk a branch needs, if it needs one
/// ([`crate::arch::ppc64::thunk`]): a branch out of range; a call from
/// code without a TOC pointer to a function that needs one, which the
/// thunk enters at its global entry point with `r12` set, or through the
/// PLT, which it reaches by loading the PLT word PC-relatively; or a call
/// from code with a TOC pointer to a function that clobbers it, which the
/// thunk saves first.
#[must_use]
pub fn branch_thunk(branch: Branch) -> Option<u64> {
    if !is_thunk_branch(branch.r_type) {
        return None;
    }
    let notoc = branch.r_type == R_PPC64_REL24_NOTOC;
    if notoc && branch.via_stub {
        return branch.slot.map(|slot| slot | insn::THUNK_VIA_SLOT);
    }
    let destination = branch_destination(branch);
    if !notoc && !branch.via_stub && insn::clobbers_toc(branch.st_other) {
        return Some(destination | insn::THUNK_SAVE_TOC);
    }
    let needs_toc = notoc && !branch.via_stub && insn::local_entry_offset(branch.st_other) != 0;
    (needs_toc || !insn::branch24_in_range(branch.place, destination)).then_some(destination)
}

/// Finishes a direct call written at `offset`: a call through a stub that
/// saves the TOC pointer (a PLT or IFUNC stub, or the thunk of a call to a
/// function that clobbers `r2`) gets the `nop` after it turned into the
/// `ld r2, 24(r1)` that restores it.
///
/// A recursive call without the `nop` is accepted, as GCC once emitted
/// those and the function is not really preempted in practice (lld does
/// the same).
///
/// # Errors
///
/// [`ApplyError::BadInstruction`] for a PLT call from code without a TOC
/// pointer whose PLT word is unknown.
pub fn finish_call(out: &mut [u8], offset: u64, branch: Branch) -> Result<(), ApplyError> {
    match branch.r_type {
        R_PPC64_REL24 if branch.via_stub || insn::clobbers_toc(branch.st_other) => {
            let next = offset.wrapping_add(4);
            if get(out, next).ok() == Some(NOP) {
                put(out, next, LD_R2_24_R1)?;
            }
            Ok(())
        }
        R_PPC64_REL24_NOTOC if branch.via_stub && branch.slot.is_none() => {
            Err(ApplyError::BadInstruction)
        }
        _ => Ok(()),
    }
}

/// Replaces a `bl` to an undefined weak symbol with a `nop`, as GNU ld
/// does: the symbol has no address, so the call is skipped.
///
/// # Errors
///
/// [`ApplyError::OutOfBounds`] when the instruction is outside the section.
pub fn nop_undefined_branch(out: &mut [u8], offset: u64, r_type: u32) -> Result<bool, ApplyError> {
    if !is_thunk_branch(r_type) {
        return Ok(false);
    }
    put(out, offset, NOP)?;
    Ok(true)
}

/// Fills `out` with `nop` instructions; a partial word is zeroed.
pub fn write_nops(out: &mut [u8]) {
    let (words, rest) = out.as_chunks_mut::<4>();
    for word in words {
        *word = NOP.to_le_bytes();
    }
    rest.fill(0);
}

/// Writes the lazy-binding resolver at address `plt` (the start of
/// `.plt`, the ABI's `.glink`), which reaches `.got.plt` at `got_plt`.
///
/// # Errors
///
/// [`ApplyError::OutOfBounds`] when `out` is too short.
pub fn write_plt_header(out: &mut [u8], plt: u64, got_plt: u64) -> Result<(), ApplyError> {
    let delta = got_plt.wrapping_sub(plt.wrapping_add(8)) as i64;
    let (words, tail) = insn::glink_header(delta);
    insn::write_words(out, 0, &words).map_err(encode_error)?;
    let slot = out.get_mut(52..60).ok_or(ApplyError::OutOfBounds)?;
    slot.copy_from_slice(&tail.to_le_bytes());
    Ok(())
}

/// Writes the lazy `.plt` entry at address `entry`: a branch back to the
/// resolver at `plt`.
///
/// # Errors
///
/// [`ApplyError`] when the resolver is out of reach.
pub fn write_plt_entry(out: &mut [u8], entry: u64, plt: u64) -> Result<(), ApplyError> {
    let word = insn::glink_entry(entry.wrapping_sub(plt)).map_err(encode_error)?;
    put(out, 0, word)
}

/// Writes a PLT call stub that jumps through the GOT word at `slot`,
/// addressed from the TOC pointer `toc`.
///
/// # Errors
///
/// [`ApplyError`] when the slot is more than 2 GiB from the TOC pointer.
pub fn write_call_stub(out: &mut [u8], slot: u64, toc: u64) -> Result<(), ApplyError> {
    let words = insn::plt_call_stub(slot.wrapping_sub(toc) as i64).map_err(encode_error)?;
    insn::write_words(out, 0, &words).map_err(encode_error)
}

/// The TOC entries a section addresses with `R_PPC64_TOC16_LO` (an `addi`
/// taking the entry's address rather than loading it): the `addis` of such
/// a pair must keep addressing the entry, so accesses to these entries are
/// not relaxed ([`toc_indirection`]). Each is `(.toc section index, offset
/// in it)`, sorted.
#[must_use]
pub fn pinned_toc_entries(
    refs: &crate::elf::refs::Refs<'_, '_>,
    file: usize,
    relocations: Relocations<'_, crate::elf::read::Elf64Le>,
) -> Vec<(u32, u64)> {
    let Relocations::Rela(relas) = relocations else {
        return Vec::new();
    };
    let mut pinned: Vec<(u32, u64)> = relas
        .iter()
        .filter(|rel| rel.r_type == R_PPC64_TOC16_LO)
        .filter_map(|rel| toc_entry(refs, file, &rel))
        .collect();
    pinned.sort_unstable();
    pinned.dedup();
    pinned
}

/// The `.toc` entry a relocation against a `.toc` section symbol names:
/// `(section index, offset)`.
fn toc_entry(
    refs: &crate::elf::refs::Refs<'_, '_>,
    file: usize,
    rel: &Relocation,
) -> Option<(u32, u64)> {
    let target = refs.target(file, rel.symbol as usize)?;
    let crate::elf::refs::Def::Section {
        file: owner,
        section,
        value,
    } = target.def
    else {
        return None;
    };
    if owner != file || !target.is_section_symbol() || rel.addend < 0 {
        return None;
    }
    let object = refs.files.get(file)?.object.as_ref()?;
    (object.section(section)?.name == b".toc")
        .then(|| Some((section, value.checked_add_signed(rel.addend)?)))
        .flatten()
}

/// The TOC-relative address to use instead of a TOC-indirect load: when
/// `rel` (an `R_PPC64_TOC16_HA` or `R_PPC64_TOC16_LO_DS`) addresses a
/// `.toc` entry that holds the address of a symbol defined in this output
/// and not preemptible, within 2 GiB of the TOC pointer, returns that
/// address and the field that packs it (the `ld` becomes an `addi`).
/// `pinned` comes from [`pinned_toc_entries`].
#[must_use]
pub fn toc_indirection(
    addresses: &crate::elf::values::Addresses<'_, '_>,
    file: usize,
    rel: &Relocation,
    pinned: &[(u32, u64)],
    pic: bool,
) -> Option<(u64, Field)> {
    use crate::elf::refs::Def;
    let field = match rel.r_type {
        R_PPC64_TOC16_HA => Field::HaToc,
        R_PPC64_TOC16_LO_DS => Field::LoDsToAddi,
        _ => return None,
    };
    let refs = &addresses.refs;
    let entry = toc_entry(refs, file, rel)?;
    if pinned.binary_search(&entry).is_ok() {
        return None;
    }
    let object = refs.files.get(file)?.object.as_ref()?;
    let toc = object.section(entry.0)?;
    let Relocations::Rela(relas) = object
        .elf
        .relocation_section(toc.relocs, &object.section(toc.relocs)?.header)
        .ok()??
        .relocations
    else {
        return None;
    };
    // `.rela.toc` holds one `R_PPC64_ADDR64` per 8-byte entry, sorted, so
    // the entry is usually at index offset / 8; entries holding constants
    // have none, so search down from there.
    let mut index = usize::try_from(entry.1 / 8)
        .ok()?
        .min(relas.len().checked_sub(1)?);
    let slot = loop {
        let candidate = relas.get(index)?;
        if candidate.offset == entry.1 {
            break candidate;
        }
        if candidate.offset < entry.1 {
            return None;
        }
        index = index.checked_sub(1)?;
    };
    if slot.r_type != R_PPC64_ADDR64 {
        return None;
    }
    let target = refs.target(file, slot.symbol as usize)?;
    let flags = target
        .global
        .map_or(crate::symbols::SymbolFlags::EMPTY, |id| {
            refs.symbols.flags(id)
        });
    if flags.contains(crate::elf::export::PREEMPTIBLE) || target.is_ifunc() {
        return None;
    }
    match target.def {
        Def::Section { .. } | Def::Common(_) | Def::Linker(_) => {}
        Def::Absolute(_) if !pic => {}
        _ => return None,
    }
    if flags.contains(crate::elf::defined::ABSOLUTE) && pic {
        return None;
    }
    let (s, a) = addresses.symbol_address(&target, slot.addend)?;
    let address = s.wrapping_add_signed(a);
    let relative = address.wrapping_sub(addresses.got_base()) as i64;
    insn::fits_signed(relative, 32).then_some((address, field))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exec() -> ClassifyContext {
        ClassifyContext::static_exec(true)
    }

    fn shared() -> ClassifyContext {
        ClassifyContext {
            relax_got: false,
            pic: true,
            tls: TlsMode::Dynamic,
            tls_ld: TlsMode::Dynamic,
            code: true,
        }
    }

    fn words(code: &[u8]) -> Vec<u32> {
        code.as_chunks::<4>()
            .0
            .iter()
            .map(|w| u32::from_le_bytes(*w))
            .collect()
    }

    fn bytes(words: &[u32]) -> Vec<u8> {
        words.iter().flat_map(|w| w.to_le_bytes()).collect()
    }

    #[test]
    fn toc_and_got_types_are_classified() {
        let toc = classify(R_PPC64_TOC16_HA, &[], 0, exec()).unwrap();
        assert_eq!(toc.kind, Kind::GotRel);
        assert!(toc.uses_got_base());
        let got_lo = classify(R_PPC64_GOT16_LO_DS, &[], 0, exec()).unwrap();
        assert_eq!(got_lo.kind, Kind::GotSlotRel);
        assert!(got_lo.needs_got());
        assert_eq!(
            classify(R_PPC64_REL24, &[], 0, exec()).unwrap().width,
            Width::Ppc(Field::Rel24)
        );
        for r_type in [R_PPC64_TOC, R_PPC64_JMP_SLOT, R_PPC64_PLTCALL, 0xdead] {
            assert_eq!(
                classify(r_type, &[], 0, exec()),
                Err(ClassifyError::Unsupported),
                "type {r_type}"
            );
        }
    }

    #[test]
    fn got_pcrel_relaxes_only_a_pld() {
        let pld = bytes(&[0x0410_0000, 0xe460_0000]);
        let paddi = bytes(&[0x0610_0000, 0x3860_0000]);
        let relaxed = classify(R_PPC64_GOT_PCREL34, &pld, 0, exec()).unwrap();
        assert_eq!(relaxed.kind, Kind::Pc);
        let kept = classify(R_PPC64_GOT_PCREL34, &paddi, 0, exec()).unwrap();
        assert_eq!(kept.kind, Kind::Got);
        let shared = classify(R_PPC64_GOT_PCREL34, &pld, 0, shared()).unwrap();
        assert!(shared.needs_got());
    }

    #[test]
    fn tls_models_follow_the_output() {
        let gd = classify(R_PPC64_GOT_TLSGD16_HA, &[], 0, shared()).unwrap();
        assert_eq!(gd.slot, GotKind::TlsGd);
        assert_eq!(
            classify(R_PPC64_TLSGD, &[], 0, shared()).unwrap().kind,
            Kind::None
        );
        let marker = classify(R_PPC64_TLSGD, &[], 0, exec()).unwrap();
        assert_eq!(marker.kind, Kind::GdToLe);
        assert!(marker.skip_next);
        let ie = ClassifyContext {
            tls: TlsMode::InitialExec,
            ..exec()
        };
        let lo = classify(R_PPC64_GOT_TLSGD16_LO, &[], 0, ie).unwrap();
        assert_eq!(lo.slot, GotKind::TpOff);
        assert!(lo.needs_gottpoff());
        assert_eq!(
            classify(R_PPC64_GOT_TPREL16_HI, &[], 0, exec()),
            Err(ClassifyError::BadTlsInstruction)
        );
    }

    /// The sequences lld 23 writes for the same inputs.
    #[test]
    fn tls_relaxation_matches_lld() {
        let values = RelaxValues {
            tpoff: -0x6ff8,
            ..RelaxValues::default()
        };
        // addis r3, r2, x@got@tlsgd@ha; addi r3, r3, x@got@tlsgd@l;
        // bl __tls_get_addr(x@tlsgd); nop
        let mut code = bytes(&[0x3c62_0000, 0x3863_0000, 0x4800_0001, NOP]);
        relax_tls(&mut code, 0, Kind::GdToLe, R_PPC64_GOT_TLSGD16_HA, values).unwrap();
        relax_tls(&mut code, 4, Kind::GdToLe, R_PPC64_GOT_TLSGD16_LO, values).unwrap();
        relax_tls(&mut code, 8, Kind::GdToLe, R_PPC64_TLSGD, values).unwrap();
        assert_eq!(
            words(&code),
            [NOP, 0x3c6d_0000, NOP, 0x3863_9008],
            "nop; addis r3, r13, 0; nop; addi r3, r3, -0x6ff8"
        );

        // To initial-exec: the pair loads the offset, the call adds r13.
        let mut code = bytes(&[0x4800_0001, NOP]);
        relax_tls(&mut code, 0, Kind::GdToIe, R_PPC64_TLSGD, values).unwrap();
        assert_eq!(words(&code), [NOP, ADD_R3_R3_R13]);

        // addis r9, r2, x@got@tprel@ha; ld r9, x@got@tprel@l(r9);
        // lwzx r3, r9, x@tls
        let mut code = bytes(&[0x3d22_0000, 0xe929_0000, 0x7c69_682e]);
        relax_tls(&mut code, 0, Kind::IeToLe, R_PPC64_GOT_TPREL16_HA, values).unwrap();
        relax_tls(
            &mut code,
            4,
            Kind::IeToLe,
            R_PPC64_GOT_TPREL16_LO_DS,
            values,
        )
        .unwrap();
        relax_tls(&mut code, 8, Kind::IeToLe, R_PPC64_TLS, values).unwrap();
        assert_eq!(
            words(&code),
            [NOP, 0x3d2d_0000, 0x8069_9008],
            "nop; addis r9, r13, 0; lwz r3, -0x6ff8(r9)"
        );

        // The local-dynamic call: addi r3, r3, 4096 after it.
        let mut code = bytes(&[0x4800_0001, NOP]);
        relax_tls(&mut code, 0, Kind::LdToLe, R_PPC64_TLSLD, values).unwrap();
        assert_eq!(words(&code), [NOP, ADDI_R3_R3_4096]);

        // The PC-relative call has no nop after it to rewrite.
        let marker = Relocation {
            offset: 0,
            symbol: 1,
            r_type: R_PPC64_TLSGD,
            addend: 0,
        };
        let call = Relocation {
            r_type: R_PPC64_REL24_NOTOC,
            ..marker
        };
        let pcrel = annotate(marker, Some(&call)).r_type;
        assert_eq!(base_type(pcrel), R_PPC64_TLSGD);
        let mut code = bytes(&[0x4800_0001, 0x7c63_1a14]);
        relax_tls(&mut code, 0, Kind::GdToLe, pcrel, values).unwrap();
        assert_eq!(words(&code), [NOP, 0x7c63_1a14]);
    }

    #[test]
    fn calls_enter_at_the_local_entry_point() {
        let call = Branch {
            r_type: R_PPC64_REL24,
            place: 0x1000_0000,
            target: 0x1000_0100,
            st_other: 3 << 5,
            via_stub: false,
            slot: None,
        };
        assert_eq!(branch_destination(call), 0x1000_0108);
        assert_eq!(branch_thunk(call), None);
        let far = Branch {
            target: 0x1400_0000,
            ..call
        };
        assert_eq!(branch_thunk(far), Some(0x1400_0008));
        let notoc = Branch {
            r_type: R_PPC64_REL24_NOTOC,
            ..call
        };
        assert_eq!(branch_destination(notoc), 0x1000_0100);
        assert_eq!(branch_thunk(notoc), Some(0x1000_0100));
        let stub = Branch {
            via_stub: true,
            ..call
        };
        assert_eq!(branch_destination(stub), 0x1000_0100);
        let mut code = bytes(&[0x4800_0001, NOP]);
        finish_call(&mut code, 0, stub).unwrap();
        assert_eq!(words(&code), [0x4800_0001, LD_R2_24_R1]);

        // PC-relative code calls through the PLT with a stub of its own
        // that loads the PLT word.
        let pcrel_plt = Branch {
            r_type: R_PPC64_REL24_NOTOC,
            via_stub: true,
            slot: Some(0x1002_0010),
            ..call
        };
        assert_eq!(
            branch_thunk(pcrel_plt),
            Some(0x1002_0010 | insn::THUNK_VIA_SLOT)
        );
        // A callee that clobbers r2 is called through a thunk saving it,
        // and the caller's nop restores it.
        let clobbers = Branch {
            st_other: 1 << 5,
            ..call
        };
        assert_eq!(
            branch_thunk(clobbers),
            Some(0x1000_0100 | insn::THUNK_SAVE_TOC)
        );
        let mut code = bytes(&[0x4800_0001, NOP]);
        finish_call(&mut code, 0, clobbers).unwrap();
        assert_eq!(words(&code), [0x4800_0001, LD_R2_24_R1]);
        let thunk = insn::thunk(0x1000_0200, 0x1000_0100 | insn::THUNK_SAVE_TOC).unwrap();
        assert_eq!(thunk[0], insn::STD_R2_24_R1);
        let thunk = insn::thunk(0x1000_0200, 0x1002_0010 | insn::THUNK_VIA_SLOT).unwrap();
        assert_eq!(thunk[5] >> 16, 0xe98c, "ld r12, lo(r12)");
    }

    /// The `.glink` lld 23 writes for a PIE whose `.glink` is at 0x10310
    /// and `.plt` at 0x20450.
    #[test]
    fn glink_matches_lld() {
        let mut header = [0u8; 60];
        write_plt_header(&mut header, 0x10310, 0x20450).unwrap();
        assert_eq!(&words(&header)[..2], [0x7c08_02a6, 0x429f_0005]);
        assert_eq!(
            u64::from_le_bytes(header[52..60].try_into().unwrap()),
            0x20450 - 0x10318
        );
        let mut entry = [0u8; 4];
        write_plt_entry(&mut entry, 0x10310 + 64, 0x10310).unwrap();
        assert_eq!(words(&entry), [0x4bff_ffc0]);
    }
}
