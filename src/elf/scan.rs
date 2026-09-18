//! The relocation scan (pipeline stage 6).
//!
//! Scans the relocations of every live allocated section, in parallel per
//! file, and records what layout has to provide, using the per-relocation
//! decisions of [`reloc::decide`]:
//!
//! - per-symbol [`SymbolFlags`]: GOT, PLT, copy relocation, canonical PLT,
//!   TLS GOT needs, [`NEEDS_IPLT`] for IFUNC symbols, and `ADDRESS_TAKEN`
//!   for non-call references (for `--icf=safe`). Flags are atomic, so
//!   threads set them without locks. Local symbols have no flag word; their
//!   needs are returned per file instead.
//! - the number of dynamic relocations each section needs, so `.rela.dyn`
//!   is sized before layout, and whether any lands in a read-only section
//!   (`DT_TEXTREL`);
//! - whether anything uses the GOT base (`_GLOBAL_OFFSET_TABLE_`) or needs
//!   a module-local TLS GOT pair;
//! - undefined symbols, with the location of every reference, for lld-style
//!   diagnostics;
//! - unsupported relocations and relocations the output cannot express, as
//!   errors.
//!
//! Relocations that TLS relaxation consumes (the `__tls_get_addr` call after
//! a general- or local-dynamic access) are skipped, so they neither need a
//! GOT or PLT entry nor report `__tls_get_addr` as undefined.

#![deny(clippy::arithmetic_side_effects)]

use rayon::prelude::*;

use crate::diag::{Diagnostic, Location};
use crate::elf::read::Relocations;
use crate::elf::read::consts::SHF_ALLOC;
use crate::ids::SymbolId;
use crate::symbols::SymbolFlags;

use super::arch::{Arch, ClassifyError, Kind};
use super::object::SectionKind;
use super::refs::{Def, Refs};
use super::reloc::{self, Context, Dynamic, LocalNeed, Problem};

/// Backend flag: the symbol is an IFUNC that needs a PLT stub and an
/// `IRELATIVE` GOT slot.
pub const NEEDS_IPLT: SymbolFlags = SymbolFlags::backend(0);

/// Backend flag: a relocation of a live allocated section refers to the
/// symbol. After `--gc-sections`, imports without it are left out of the
/// symbol tables, as GNU ld hides symbols only dead code refers to.
pub const REF_LIVE: SymbolFlags = SymbolFlags::backend(2);

/// One reference to an undefined symbol.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct UndefinedRef {
    /// The symbol.
    pub symbol: SymbolId,
    /// The referring file.
    pub file: usize,
    /// The section holding the relocation.
    pub section: u32,
    /// Offset of the relocation in that section.
    pub offset: u64,
}

/// A section with dynamic relocations.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct DynSection {
    /// The section index in its file.
    pub section: u32,
    /// How many `R_X86_64_RELATIVE` relocations it needs.
    pub relative: u32,
    /// How many symbolic dynamic relocations it needs.
    pub symbolic: u32,
    /// How many of the relative relocations `-z pack-relative-relocs` can
    /// move to `.relr.dyn` ([`reloc::packable`]).
    pub packable: u32,
}

/// What the scan of one file found.
#[derive(Debug, Default)]
pub struct FileScan {
    /// Local symbols needing a GOT entry, sorted and deduplicated.
    pub got_locals: Vec<u32>,
    /// Local IFUNC symbols needing a PLT stub, sorted and deduplicated.
    pub iplt_locals: Vec<u32>,
    /// Local TLS symbols needing a module/offset GOT pair.
    pub tlsgd_locals: Vec<u32>,
    /// Local TLS symbols needing a thread pointer offset GOT entry.
    pub gottpoff_locals: Vec<u32>,
    /// Local TLS symbols needing a descriptor GOT pair.
    pub tlsdesc_locals: Vec<u32>,
    /// A module-local TLS GOT pair is needed.
    pub tls_ld: bool,
    /// Sections with dynamic relocations, by section index.
    pub dyn_sections: Vec<DynSection>,
    /// A dynamic relocation applies to a read-only section.
    pub text_relocs: bool,
    /// An initial-exec TLS access remains (`DF_STATIC_TLS` in shared
    /// objects).
    pub static_tls: bool,
    /// References to undefined symbols.
    pub undefined: Vec<UndefinedRef>,
    /// Problems found.
    pub errors: Vec<Diagnostic>,
    /// The file uses the GOT base.
    pub uses_got_base: bool,
}

/// The scan result, per file.
#[derive(Debug)]
pub struct ScanResult {
    /// One entry per input file.
    pub files: Vec<FileScan>,
}

impl ScanResult {
    /// Whether any file uses the GOT base.
    #[must_use]
    pub fn uses_got_base(&self) -> bool {
        self.files.iter().any(|f| f.uses_got_base)
    }

    /// Whether any file needs a module-local TLS GOT pair.
    #[must_use]
    pub fn tls_ld(&self) -> bool {
        self.files.iter().any(|f| f.tls_ld)
    }

    /// Whether any dynamic relocation applies to a read-only section.
    #[must_use]
    pub fn text_relocs(&self) -> bool {
        self.files.iter().any(|f| f.text_relocs)
    }

    /// Whether an initial-exec TLS access remains.
    #[must_use]
    pub fn static_tls(&self) -> bool {
        self.files.iter().any(|f| f.static_tls)
    }

    /// Total `(relative, symbolic)` dynamic relocations of input sections.
    #[must_use]
    pub fn section_dyn_relocs(&self) -> (u64, u64) {
        self.files
            .iter()
            .flat_map(|f| &f.dyn_sections)
            .fold((0u64, 0u64), |(r, s), d| {
                (
                    r.saturating_add(u64::from(d.relative)),
                    s.saturating_add(u64::from(d.symbolic)),
                )
            })
    }

    /// Total relative relocations of input sections that `.relr.dyn` can
    /// hold.
    #[must_use]
    pub fn section_packable(&self) -> u64 {
        self.files
            .iter()
            .flat_map(|f| &f.dyn_sections)
            .fold(0u64, |n, d| n.saturating_add(u64::from(d.packable)))
    }
}

/// Scans every live allocated section.
#[must_use]
pub fn scan(refs: &Refs<'_, '_>, context: &Context) -> ScanResult {
    let files = refs
        .files
        .par_iter()
        .enumerate()
        .map(|(file_index, _)| scan_file(refs, file_index, context))
        .collect();
    ScanResult { files }
}

/// A diagnostic location for offset `offset` of section `section` of `file`.
#[must_use]
pub fn location(refs: &Refs<'_, '_>, file: usize, section: u32, offset: u64) -> Location {
    let input = refs.files.get(file);
    let name = input
        .and_then(|f| f.object.as_ref())
        .and_then(|o| o.section(section))
        .map(|s| String::from_utf8_lossy(s.name).into_owned());
    Location {
        file: input.map(|f| f.path()).unwrap_or_default(),
        member: input.and_then(|f| f.member()),
        section: name,
        offset: Some(offset),
        source: None,
    }
}

fn type_name(arch: Arch, r_type: u32) -> String {
    arch.reloc_label(r_type)
}

fn scan_file(refs: &Refs<'_, '_>, file_index: usize, context: &Context) -> FileScan {
    let mut result = FileScan::default();
    let Some(file) = refs.files.get(file_index) else {
        return result;
    };
    let Some(object) = &file.object else {
        return result;
    };
    let order = file.position.raw();
    if let Some(error) = context.arch.incompatible(refs.files, file_index) {
        result.errors.push(Diagnostic::error(error).order(order));
    }
    for (section_index, section) in object.sections.iter().enumerate() {
        let section_index = u32::try_from(section_index).unwrap_or(u32::MAX);
        if section.relocs == 0
            || section.header.sh_flags & SHF_ALLOC == 0
            || section.kind == SectionKind::Ignored
            || !refs.sections.is_live_in(file_index, section_index)
        {
            continue;
        }
        let data = if section.kind == SectionKind::Merge || section.is_nobits() {
            &[][..]
        } else {
            match object.elf.section_data(&section.header) {
                Ok(data) => data,
                Err(error) => {
                    result
                        .errors
                        .push(Diagnostic::error(error.to_string()).order(order));
                    continue;
                }
            }
        };
        let relocations = match object
            .section(section.relocs)
            .map(|r| object.elf.relocation_section(section.relocs, &r.header))
        {
            Some(Ok(Some(relocations))) => relocations.relocations,
            Some(Err(error)) => {
                result
                    .errors
                    .push(Diagnostic::error(error.to_string()).order(order));
                continue;
            }
            _ => continue,
        };
        let Relocations::Rela(relas) = relocations else {
            result.errors.push(
                Diagnostic::error(format!(
                    "{}: SHT_REL relocations are not supported for {}",
                    file.display(),
                    context.arch.emulation()
                ))
                .order(order),
            );
            continue;
        };
        let eh_frame = section.kind == SectionKind::EhFrame;
        let mut dyn_section = DynSection {
            section: section_index,
            relative: 0,
            symbolic: 0,
            packable: 0,
        };
        let mut skip = false;
        for rel in relas.iter() {
            if skip {
                // The call a TLS relaxation removed still counts as a use
                // (GNU ld keeps `__tls_get_addr` in the dynamic symbols).
                skip = false;
                if let Some(id) = refs.global_id(file_index, rel.symbol as usize) {
                    refs.symbols.set_flags(id, REF_LIVE);
                }
                continue;
            }
            let Some(target) = refs.target(file_index, rel.symbol as usize) else {
                result.errors.push(
                    Diagnostic::error(format!(
                        "{}: relocation refers to invalid symbol index {}",
                        file.display(),
                        rel.symbol
                    ))
                    .order(order),
                );
                continue;
            };
            let flags = target
                .global
                .map_or(SymbolFlags::EMPTY, |id| refs.symbols.flags(id));
            if let Some(id) = target.global
                && !flags.contains(REF_LIVE)
            {
                refs.symbols.set_flags(id, REF_LIVE);
            }
            let decision =
                match reloc::decide(context, &rel, data, &target, flags, section.header.sh_flags) {
                    Ok(decision) => decision,
                    Err(error) => {
                        let what = match error {
                            ClassifyError::Unsupported => format!(
                                "unsupported relocation type {}",
                                type_name(context.arch, rel.r_type)
                            ),
                            ClassifyError::BadTlsInstruction => format!(
                                "{} is not part of a TLS sequence qld can link",
                                context
                                    .arch
                                    .reloc_name(rel.r_type)
                                    .unwrap_or("a TLS relocation")
                            ),
                        };
                        result.errors.push(
                            Diagnostic::error(what)
                                .at(location(refs, file_index, section_index, rel.offset))
                                .order(order),
                        );
                        continue;
                    }
                };
            let class = decision.class;
            skip = class.skip_next;
            if class.kind == Kind::None {
                continue;
            }
            result.uses_got_base |= class.uses_got_base();
            result.tls_ld |= decision.tls_ld;
            result.static_tls |= class.needs_gottpoff();
            if let Def::Undefined { weak: false } = target.def
                && let Some(symbol) = target.global
                && !eh_frame
            {
                result.undefined.push(UndefinedRef {
                    symbol,
                    file: file_index,
                    section: section_index,
                    offset: rel.offset,
                });
            }
            if let Some(problem) = decision.problem {
                let name = refs
                    .symbol_name(file_index, rel.symbol)
                    .unwrap_or_else(|| "local symbol".to_string());
                let what = match problem {
                    Problem::NeedsPic => format!(
                        "relocation {} cannot be used against symbol '{name}'; recompile with -fPIC",
                        type_name(context.arch, rel.r_type)
                    ),
                    Problem::LocalExecTls => format!(
                        "relocation {} against '{name}' cannot be used with this output; \
                         recompile with -fPIC",
                        type_name(context.arch, rel.r_type)
                    ),
                    Problem::NoCopyReloc => format!(
                        "unresolvable relocation {} against symbol '{name}'; recompile with -fPIC \
                         or remove '-z nocopyreloc'",
                        type_name(context.arch, rel.r_type)
                    ),
                };
                result.errors.push(
                    Diagnostic::error(what)
                        .at(location(refs, file_index, section_index, rel.offset))
                        .order(order),
                );
                continue;
            }
            match decision.dynamic {
                Dynamic::None => {}
                Dynamic::Relative => {
                    dyn_section.relative = dyn_section.relative.saturating_add(1);
                    if reloc::packable(section.header.sh_addralign, rel.offset) {
                        dyn_section.packable = dyn_section.packable.saturating_add(1);
                    }
                }
                Dynamic::Symbolic(_) => {
                    dyn_section.symbolic = dyn_section.symbolic.saturating_add(1);
                }
            }
            result.text_relocs |= decision.text;
            match target.global {
                Some(id) => {
                    let mut flags = decision.flags;
                    if !context.arch.is_branch(rel.r_type) {
                        flags |= SymbolFlags::ADDRESS_TAKEN;
                    }
                    if !flags.is_empty() {
                        refs.symbols.set_flags(id, flags);
                    }
                }
                None => {
                    let list = match decision.local {
                        LocalNeed::None => None,
                        LocalNeed::Got => Some(&mut result.got_locals),
                        LocalNeed::TlsGd => Some(&mut result.tlsgd_locals),
                        LocalNeed::GotTpOff => Some(&mut result.gottpoff_locals),
                        LocalNeed::TlsDesc => Some(&mut result.tlsdesc_locals),
                    };
                    if let Some(list) = list {
                        list.push(rel.symbol);
                    }
                    if target.is_ifunc() {
                        result.iplt_locals.push(rel.symbol);
                    }
                }
            }
        }
        if dyn_section.relative != 0 || dyn_section.symbolic != 0 {
            result.dyn_sections.push(dyn_section);
        }
    }
    for list in [
        &mut result.got_locals,
        &mut result.iplt_locals,
        &mut result.tlsgd_locals,
        &mut result.gottpoff_locals,
        &mut result.tlsdesc_locals,
    ] {
        list.sort_unstable();
        list.dedup();
    }
    result
}
