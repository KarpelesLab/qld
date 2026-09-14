//! The relocation scan (pipeline stage 6).
//!
//! Scans the relocations of every live allocated section, in parallel per
//! file, and records what layout has to provide:
//!
//! - per-symbol [`SymbolFlags`]: `NEEDS_GOT` for GOT-indirect accesses that
//!   cannot be relaxed, [`NEEDS_IPLT`] for references to IFUNC symbols, and
//!   `ADDRESS_TAKEN` for non-call references (for `--icf=safe`). Flags are
//!   atomic, so threads set them without locks. Local symbols have no flag
//!   word; their needs are returned per file instead.
//! - whether anything uses the GOT base (`_GLOBAL_OFFSET_TABLE_`);
//! - undefined symbols, with the location of every reference, for lld-style
//!   diagnostics;
//! - unsupported relocations, as errors.
//!
//! Relocations that TLS relaxation consumes (the `__tls_get_addr` call after
//! a general- or local-dynamic access) are skipped, so they neither need a
//! GOT entry nor report `__tls_get_addr` as undefined.

#![deny(clippy::arithmetic_side_effects)]

use rayon::prelude::*;

use crate::diag::{Diagnostic, Location};
use crate::elf::read::Relocations;
use crate::elf::read::consts::{EM_X86_64, SHF_ALLOC, reloc_name};
use crate::ids::SymbolId;
use crate::symbols::SymbolFlags;

use super::arch::x86_64::{self, ClassifyError, Kind};
use super::object::SectionKind;
use super::refs::{Def, Refs};

/// Backend flag: the symbol is an IFUNC that needs a PLT stub and an
/// `IRELATIVE` GOT slot.
pub const NEEDS_IPLT: SymbolFlags = SymbolFlags::backend(0);

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

/// What the scan of one file found.
#[derive(Debug, Default)]
pub struct FileScan {
    /// Local symbols needing a GOT entry, sorted and deduplicated.
    pub got_locals: Vec<u32>,
    /// Local IFUNC symbols needing a PLT stub, sorted and deduplicated.
    pub iplt_locals: Vec<u32>,
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
}

/// Scans every live allocated section.
#[must_use]
pub fn scan(refs: &Refs<'_, '_>, relax: bool) -> ScanResult {
    let files = refs
        .files
        .par_iter()
        .enumerate()
        .map(|(file_index, _)| scan_file(refs, file_index, relax))
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

fn scan_file(refs: &Refs<'_, '_>, file_index: usize, relax: bool) -> FileScan {
    let mut result = FileScan::default();
    let Some(file) = refs.files.get(file_index) else {
        return result;
    };
    let Some(object) = &file.object else {
        return result;
    };
    let order = file.position.raw();
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
                    "{}: SHT_REL relocations are not supported for x86-64",
                    file.display()
                ))
                .order(order),
            );
            continue;
        };
        let eh_frame = section.kind == SectionKind::EhFrame;
        let mut skip = false;
        for rel in relas.iter() {
            if skip {
                skip = false;
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
            let is_ifunc = target.is_ifunc();
            let class = match x86_64::classify(
                rel.r_type,
                rel.addend,
                data,
                rel.offset,
                relax && !is_ifunc,
            ) {
                Ok(class) => class,
                Err(error) => {
                    let what = match error {
                        ClassifyError::Unsupported => format!(
                            "unsupported relocation type {}",
                            reloc_name(EM_X86_64, rel.r_type)
                                .map_or_else(|| rel.r_type.to_string(), str::to_owned)
                        ),
                        ClassifyError::BadTlsInstruction => format!(
                            "{} must be followed by a call to __tls_get_addr",
                            reloc_name(EM_X86_64, rel.r_type).unwrap_or("TLS relocation")
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
            skip = class.kind.skips_next();
            if class.kind == Kind::None {
                continue;
            }
            result.uses_got_base |= class.kind.uses_got_base();
            match target.def {
                Def::Undefined { weak: false } => {
                    if let Some(symbol) = target.global
                        && !eh_frame
                    {
                        result.undefined.push(UndefinedRef {
                            symbol,
                            file: file_index,
                            section: section_index,
                            offset: rel.offset,
                        });
                    }
                }
                Def::Section { .. } | Def::Absolute(_) | Def::Common(_) | Def::Linker(_) => {}
                Def::Undefined { weak: true } => {}
            }
            let got = class.kind.needs_got();
            match target.global {
                Some(id) => {
                    let mut flags = SymbolFlags::EMPTY;
                    if got {
                        flags |= SymbolFlags::NEEDS_GOT;
                    }
                    if is_ifunc {
                        flags |= NEEDS_IPLT;
                    }
                    if !matches!(rel.r_type, crate::elf::read::consts::x86_64::R_X86_64_PLT32) {
                        flags |= SymbolFlags::ADDRESS_TAKEN;
                    }
                    if !flags.is_empty() {
                        refs.symbols.set_flags(id, flags);
                    }
                }
                None => {
                    if got {
                        result.got_locals.push(rel.symbol);
                    }
                    if is_ifunc {
                        result.iplt_locals.push(rel.symbol);
                    }
                }
            }
        }
    }
    result.got_locals.sort_unstable();
    result.got_locals.dedup();
    result.iplt_locals.sort_unstable();
    result.iplt_locals.dedup();
    result
}
