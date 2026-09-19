//! Per-relocation linking decisions, shared by the relocation scan and the
//! writer so the two always agree.
//!
//! [`decide`] combines the instruction-level classification
//! (`arch::x86_64::classify` and its AArch64 counterpart) with what the dynamic linker needs, following
//! lld's model:
//!
//! - **GOT, TLS and PLT accesses** set per-symbol needs ([`SymbolFlags`]),
//!   or per-file needs for local symbols. A call (`PLT32`) to a preemptible
//!   symbol goes through a PLT entry.
//! - **Absolute addresses** (`R_X86_64_64`) in position-independent output
//!   become `R_X86_64_RELATIVE`, or a symbolic `R_X86_64_64` when the
//!   symbol is preemptible.
//! - **Direct references from executables to shared library symbols**
//!   (`PC32`, `32S`, read-only `64`) get a copy relocation for data and a
//!   canonical PLT entry for functions, so the executable owns the address.
//! - **What cannot work** (a 32-bit absolute address in a PIE, a PC-relative
//!   reference to a preemptible symbol from a shared object, local-exec TLS
//!   in a shared object) is a [`Problem`], reported by the scan.
//!
//! A dynamic relocation in a read-only section is a text relocation
//! ([`Decision::text`]).

#![deny(clippy::arithmetic_side_effects)]

use crate::elf::read::Relocation;
use crate::elf::read::consts::{SHF_ALLOC, SHF_WRITE, STT_FUNC, STT_GNU_IFUNC};
use crate::symbols::SymbolFlags;

use super::arch::{
    Arch, Class, ClassifyContext, ClassifyError, DynKind, GotKind, Kind, TlsMode, Width,
};
use super::export::{Mode, PREEMPTIBLE};
use super::refs::{Def, Target};
use super::scan::NEEDS_IPLT;

/// What the dynamic linker has to do for one relocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dynamic {
    /// Nothing: the value is final at link time.
    None,
    /// A relative relocation: the load base is added to `S + A`.
    Relative,
    /// A symbolic dynamic relocation against the symbol.
    Symbolic(DynKind),
}

/// Why a relocation cannot be linked in this output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Problem {
    /// An absolute or PC-relative reference that position-independent
    /// output cannot express: recompile with `-fPIC`.
    NeedsPic,
    /// A local-exec TLS reference in a shared object, or to a shared
    /// library's variable.
    LocalExecTls,
    /// A copy relocation is needed but `-z nocopyreloc` was given.
    NoCopyReloc,
}

/// What a local symbol needs from the GOT.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalNeed {
    /// Nothing.
    None,
    /// An address entry.
    Got,
    /// A module/offset pair.
    TlsGd,
    /// A thread pointer offset entry.
    GotTpOff,
    /// A TLS descriptor pair.
    TlsDesc,
}

/// The decision for one relocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Decision {
    /// The instruction-level classification.
    pub class: Class,
    /// The dynamic relocation it becomes, if any.
    pub dynamic: Dynamic,
    /// The dynamic relocation applies to a read-only section.
    pub text: bool,
    /// Needs of the relocation's global symbol.
    pub flags: SymbolFlags,
    /// Needs of the relocation's local symbol.
    pub local: LocalNeed,
    /// A module-local TLS pair is needed.
    pub tls_ld: bool,
    /// The relocation cannot be linked.
    pub problem: Option<Problem>,
}

/// What [`decide`] needs about the link.
#[derive(Clone, Copy, Debug)]
pub struct Context {
    /// The output mode.
    pub mode: Mode,
    /// `--relax` (the default).
    pub relax: bool,
    /// `-z copyreloc` (the default).
    pub copy_relocs: bool,
    /// The architecture, chosen once per link.
    pub arch: Arch,
    /// Undefined weak symbols are 0 in an x86 executable (GNU ld's
    /// backends): absolute references to them need no dynamic relocation,
    /// and in a static position-dependent executable GOT loads of them
    /// relax to the constant.
    pub weak_zero: bool,
}

impl Context {
    /// Whether an undefined weak symbol is 0 in a link of `arch` in
    /// `mode`, needing no dynamic relocation: every executable of the x86
    /// family, as in GNU ld's shared x86 backend
    /// (`UNDEFINED_WEAK_RESOLVED_TO_ZERO`, which covers PIEs too).
    #[must_use]
    pub fn weak_zero(arch: Arch, mode: Mode) -> bool {
        mode.executable() && matches!(arch, Arch::I386 | Arch::X32 | Arch::X86_64)
    }
}

/// Whether a relative relocation at `offset` of a section aligned to
/// `align` can go to `.relr.dyn`: its final address is certainly even, as
/// the format needs, only when the section's alignment makes the offset's
/// parity carry over (lld's rule).
#[must_use]
pub fn packable(align: u64, offset: u64) -> bool {
    align >= 2 && offset & 1 == 0
}

/// Properties of a relocation target that decisions depend on.
#[derive(Clone, Copy, Debug)]
struct Props {
    global: bool,
    preemptible: bool,
    shared: bool,
    defined: bool,
    absolute: bool,
    function: bool,
    local_ifunc: bool,
    undefined_weak: bool,
}

fn props(target: &Target, flags: SymbolFlags) -> Props {
    let preemptible = target.global.is_some() && flags.contains(PREEMPTIBLE);
    let kind = target.raw.map_or(0, |r| r.kind());
    Props {
        global: target.global.is_some(),
        preemptible,
        shared: matches!(target.def, Def::Shared(_)),
        defined: matches!(
            target.def,
            Def::Section { .. } | Def::Absolute(_) | Def::Common(_) | Def::Linker(_)
        ),
        absolute: matches!(target.def, Def::Absolute(_))
            || flags.contains(super::defined::ABSOLUTE),
        function: kind == STT_FUNC || kind == STT_GNU_IFUNC,
        local_ifunc: target.is_ifunc() && !preemptible,
        undefined_weak: matches!(target.def, Def::Undefined { weak: true }),
    }
}

/// The classification context for a relocation against `target`.
#[must_use]
pub fn classify_context(context: &Context, target: &Target, flags: SymbolFlags) -> ClassifyContext {
    let p = props(target, flags);
    let mode = context.mode;
    let tls = if !mode.dynamic {
        TlsMode::LocalExec
    } else if mode.shared {
        TlsMode::Dynamic
    } else if p.preemptible {
        TlsMode::InitialExec
    } else {
        TlsMode::LocalExec
    };
    ClassifyContext {
        relax_got: context.relax
            && p.defined
            && !p.preemptible
            && !p.local_ifunc
            && !(p.absolute && mode.pic),
        pic: mode.pic,
        tls,
        tls_ld: if mode.shared {
            TlsMode::Dynamic
        } else {
            TlsMode::LocalExec
        },
        code: false,
    }
}

/// Lets GOT loads of an undefined weak symbol relax to the constant 0
/// ([`Context::weak_zero`]). Out of line and cold, so the relocation loops
/// of the other architectures do not carry it.
#[cold]
#[inline(never)]
fn relax_weak_zero(context: &Context, target: &Target, classify: &mut ClassifyContext) {
    // Only in a static position-dependent executable: a dynamic one keeps
    // the GOT entry, which it may still bind, and position-independent
    // code has no immediate to relax to (GNU ld does neither).
    if context.relax
        && !context.mode.dynamic
        && !context.mode.pic
        && matches!(target.def, Def::Undefined { weak: true })
    {
        classify.relax_got = true;
    }
}

/// Decides how relocation `rel` of a section with flags `section_flags`
/// (contents `data`) against `target` is linked. `flags` are the global
/// symbol's flags (empty for locals).
///
/// # Errors
///
/// [`ClassifyError`] for unsupported relocations and unrecognized TLS code.
#[inline(always)]
pub fn decide<F: crate::elf::read::ElfFormat>(
    context: &Context,
    rel: &Relocation,
    data: &[u8],
    target: &Target,
    flags: SymbolFlags,
    section_flags: u64,
) -> Result<Decision, ClassifyError> {
    let mut classify = classify_context(context, target, flags);
    classify.code = section_flags & crate::elf::read::consts::SHF_EXECINSTR != 0;
    // Only ELF32 links can be i386 ones: for the other formats this is
    // gone at compile time, and costs their relocation loops nothing.
    if F::WORD_SIZE == 4 && context.weak_zero {
        relax_weak_zero(context, target, &mut classify);
    }
    let class = context
        .arch
        .classify(rel.r_type, rel.addend, data, rel.offset, classify)?;
    let p = props(target, flags);
    let mode = context.mode;
    let mut decision = Decision {
        class,
        dynamic: Dynamic::None,
        text: false,
        flags: SymbolFlags::EMPTY,
        local: LocalNeed::None,
        tls_ld: false,
        problem: None,
    };
    if section_flags & SHF_ALLOC == 0 {
        return Ok(decision);
    }
    let need = |decision: &mut Decision, global: SymbolFlags, local: LocalNeed| {
        if p.global {
            decision.flags |= global;
        } else {
            decision.local = local;
        }
    };
    if p.local_ifunc && p.global {
        decision.flags |= NEEDS_IPLT;
    }
    if class.needs_got() {
        match class.slot {
            GotKind::Address => need(&mut decision, SymbolFlags::NEEDS_GOT, LocalNeed::Got),
            GotKind::TpOff => need(
                &mut decision,
                SymbolFlags::NEEDS_GOTTPOFF,
                LocalNeed::GotTpOff,
            ),
            GotKind::TlsGd => need(&mut decision, SymbolFlags::NEEDS_TLSGD, LocalNeed::TlsGd),
            GotKind::TlsDesc => need(
                &mut decision,
                SymbolFlags::NEEDS_TLSDESC,
                LocalNeed::TlsDesc,
            ),
            GotKind::TlsLd => decision.tls_ld = true,
        }
    }
    match class.kind {
        Kind::GdToIe | Kind::DescToIe => {
            need(
                &mut decision,
                SymbolFlags::NEEDS_GOTTPOFF,
                LocalNeed::GotTpOff,
            );
        }
        Kind::TpOff => {
            if mode.dynamic && (mode.shared || p.preemptible) {
                decision.problem = Some(Problem::LocalExecTls);
            }
        }
        Kind::Pc | Kind::Page => {
            // Whether the relocation is a call is asked only for the
            // symbols it matters for: most branch to a local definition.
            if p.preemptible {
                if context.arch.is_branch(rel.r_type) {
                    decision.flags |= SymbolFlags::NEEDS_PLT;
                } else {
                    direct_reference(&mut decision, context, p);
                }
            } else if p.undefined_weak && mode.dynamic && context.arch.is_branch(rel.r_type) {
                // Resolved to zero here, but GNU ld still gives a call
                // that is also checked through the GOT a `.plt.got` entry.
                decision.flags |= SymbolFlags::NEEDS_PLT;
            }
        }
        Kind::Abs => {
            // An absolute value packed into an instruction is the low half
            // of a PC-relative pair (`adrp` plus `add`/`ldr`): it never
            // becomes a dynamic relocation, and the `adrp` half reports the
            // problem if there is one.
            // RISC-V label differences (`SET`, `SUB6`, `ULEB128`) are final
            // at link time too.
            match class.width {
                Width::Field(field) if !field.is_data() => return Ok(decision),
                Width::RiscV(field) if field.is_label_math() => return Ok(decision),
                _ => {}
            }
            if !mode.dynamic {
                return Ok(decision);
            }
            let writable = section_flags & SHF_WRITE != 0;
            // An ELF64 output's pointer is always `Width::W64`, so the
            // architecture is asked only in ELF32 links, where the answer
            // differs (`R_386_32`, `R_X86_64_32`, `R_ARM_ABS32`, …).
            let word = if F::WORD_SIZE == 8 {
                class.width == Width::W64
            } else {
                context.arch.is_word(class.width)
            };
            if word {
                if !p.preemptible {
                    if mode.pic && (p.defined || !p.global) && !p.absolute {
                        decision.dynamic = Dynamic::Relative;
                    }
                } else if context.weak_zero && p.undefined_weak {
                    // GNU ld's x86 backends resolve an undefined weak
                    // symbol to 0 in an executable, with no dynamic
                    // relocation (crtbegin.o's `_ITM_*`).
                } else if mode.shared || writable || !p.shared {
                    decision.dynamic = Dynamic::Symbolic(DynKind::Abs64);
                    decision.flags |= SymbolFlags::NEEDS_DYNSYM;
                } else {
                    direct_reference(&mut decision, context, p);
                }
            } else if p.preemptible {
                if mode.shared {
                    decision.problem = Some(Problem::NeedsPic);
                } else {
                    direct_reference(&mut decision, context, p);
                }
            } else if mode.pic && !p.absolute && (p.defined || !p.global) {
                decision.problem = Some(Problem::NeedsPic);
            }
            decision.text = decision.dynamic != Dynamic::None && !writable;
        }
        _ => {}
    }
    Ok(decision)
}

/// A non-GOT, non-PLT reference to a preemptible symbol from an executable
/// (or an error from a shared object).
fn direct_reference(decision: &mut Decision, context: &Context, p: Props) {
    if context.mode.shared {
        decision.problem = Some(Problem::NeedsPic);
    } else if p.shared {
        if p.function {
            decision.flags |= SymbolFlags::NEEDS_PLT | SymbolFlags::NEEDS_CANONICAL_PLT;
        } else if context.copy_relocs {
            decision.flags |= SymbolFlags::NEEDS_COPY_RELOC;
        } else {
            decision.problem = Some(Problem::NoCopyReloc);
        }
    } else if !p.undefined_weak {
        decision.problem = Some(Problem::NeedsPic);
    }
}
