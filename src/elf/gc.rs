//! `--gc-sections` (pipeline stage 7): builds the section graph and runs
//! [`crate::passes::gc`].
//!
//! **Edges** come from the relocations of allocated sections (to the section
//! defining each target), from `SHF_LINK_ORDER` (the linked-to section keeps
//! its dependent), from COMDAT groups (members live together), and from
//! `.eh_frame`: a live function keeps its LSDA and personality routine, but
//! FDEs themselves do not keep functions alive ([`EhFrames::gc_edges`]).
//!
//! **Roots** are the entry point, `-u`/`--require-defined` symbols and
//! `--defsym` targets, `KEEP` sections of the layout rules (init/fini
//! arrays, `.init`, `.fini`, `.ctors`, `.dtors`, `.eh_frame`),
//! `SHF_GNU_RETAIN` and note sections, non-allocated sections, and every
//! section of an output section named by a referenced `__start_`/`__stop_`
//! symbol (GNU ld's `-z nostart-stop-gc`).

#![deny(clippy::arithmetic_side_effects)]

use rayon::prelude::*;

use crate::diag::{Diagnostic, DiagnosticSink};
use crate::elf::read::Relocations;
use crate::elf::read::consts::{SHF_ALLOC, SHF_LINK_ORDER};
use crate::error::{Error, Result};
use crate::ids::SectionId;
use crate::passes::{SectionGraph, collect_garbage};
use crate::symbols::SymbolName;

use super::defined::LinkerSymbols;
use super::ehframe::EhFrames;
use super::inputs::InternalNames;
use super::object::SectionKind;
use super::place::Placement;
use super::refs::{Def, Refs};

/// Runs garbage collection. Returns the sections it removes, in input
/// order; the caller clears their live bits.
///
/// # Errors
///
/// Returns [`Error::Internal`] if the graph is inconsistent.
pub fn collect(
    refs: &Refs<'_, '_>,
    placement: &Placement<'_>,
    eh_frames: &EhFrames<'_>,
    linker: &LinkerSymbols,
    internal: &InternalNames,
) -> Result<(Vec<SectionId>, SectionGraph)> {
    let total = refs.sections.len();

    // Extra edges: eh_frame, link order, groups.
    let mut extra: Vec<(SectionId, SectionId)> = eh_frames.gc_edges(refs);
    let per_file: Vec<Vec<(SectionId, SectionId)>> = refs
        .files
        .par_iter()
        .enumerate()
        .map(|(file_index, file)| {
            let mut edges = Vec::new();
            let Some(object) = &file.object else {
                return edges;
            };
            for (index, section) in object.sections.iter().enumerate() {
                if section.header.sh_flags & SHF_LINK_ORDER == 0 {
                    continue;
                }
                let (Some(from), Some(to)) = (
                    refs.sections.id(file_index, section.header.sh_link),
                    refs.sections
                        .id(file_index, u32::try_from(index).unwrap_or(u32::MAX)),
                ) else {
                    continue;
                };
                edges.push((from, to));
            }
            for group in &object.groups {
                let ids: Vec<SectionId> = group
                    .members
                    .iter()
                    .filter_map(|&m| refs.sections.id(file_index, m))
                    .filter(|&id| refs.sections.is_live(id))
                    .collect();
                if ids.len() < 2 {
                    continue;
                }
                for pair in ids.windows(2) {
                    if let [a, b] = pair {
                        edges.push((*a, *b));
                    }
                }
                if let (Some(&last), Some(&first)) = (ids.last(), ids.first()) {
                    edges.push((last, first));
                }
            }
            edges
        })
        .collect();
    for edges in per_file {
        extra.extend(edges);
    }
    extra.par_sort_unstable();
    extra.dedup();

    // Roots.
    let mut roots: Vec<SectionId> = (0..total)
        .into_par_iter()
        .filter(|&index| {
            refs.sections.live.get(index).copied().unwrap_or(false)
                && placement.keep.get(index).copied().unwrap_or(false)
        })
        .map(SectionId::new)
        .collect();
    for (name, _) in &internal.names {
        if let Some(id) = refs.symbols.lookup(&SymbolName::new(name)) {
            let target = refs.global_target(id, false);
            if let Some(section) = refs.target_section(&target) {
                roots.push(section);
            }
        }
    }
    if !linker.start_stop_outputs.is_empty() {
        roots.par_extend((0..total).into_par_iter().filter_map(|index| {
            let output = placement.out.get(index)?;
            linker
                .start_stop_outputs
                .binary_search(output)
                .ok()
                .map(|_| SectionId::new(index))
        }));
    }

    let edge_count = |section: SectionId| -> usize {
        let base = relocation_count(refs, section);
        let from = extra.partition_point(|(f, _)| *f < section);
        let to = extra.partition_point(|(f, _)| *f <= section);
        base.saturating_add(to.saturating_sub(from))
    };
    let fill = |section: SectionId, slot: &mut [SectionId]| -> usize {
        let mut written = 0usize;
        let mut push = |target: SectionId, slot: &mut [SectionId]| {
            if let Some(entry) = slot.get_mut(written) {
                *entry = target;
                written = written.saturating_add(1);
            }
        };
        if let Some((file_index, index)) = refs.sections.locate(section)
            && let Some(object) = refs.files.get(file_index).and_then(|f| f.object.as_ref())
            && let Some(input) = object.section(index)
            && input.relocs != 0
            && input.kind != SectionKind::EhFrame
            && input.header.sh_flags & SHF_ALLOC != 0
            && let Some(Ok(Some(relocations))) = object
                .section(input.relocs)
                .map(|r| object.elf.relocation_section(input.relocs, &r.header))
            && let Relocations::Rela(relas) = relocations.relocations
        {
            for rel in relas.iter() {
                let Some(target) = refs.target(file_index, rel.symbol as usize) else {
                    continue;
                };
                if let Def::Section { .. } = target.def
                    && let Some(to) = refs.target_section(&target)
                {
                    push(to, slot);
                }
            }
        }
        let from = extra.partition_point(|(f, _)| *f < section);
        let to = extra.partition_point(|(f, _)| *f <= section);
        for &(_, target) in extra.get(from..to).unwrap_or_default() {
            push(target, slot);
        }
        written
    };
    let graph = SectionGraph::build_parallel(total, edge_count, fill, roots)
        .map_err(|e| Error::Internal(format!("section graph: {e}")))?;
    let live = collect_garbage(&graph);

    let removed: Vec<SectionId> = refs
        .sections
        .live
        .par_iter()
        .enumerate()
        .filter_map(|(index, &was_live)| {
            let id = SectionId::new(index);
            (was_live && !live.is_live(id)).then_some(id)
        })
        .collect();
    Ok((removed, graph))
}

/// Reports `--why-live`: for every defined global symbol matching one of
/// `patterns` (with `*` and `?` wildcards), the reference chain from a GC
/// root to its section, or that it was removed.
pub fn report_why_live(
    refs: &Refs<'_, '_>,
    graph: &SectionGraph,
    patterns: &[String],
    diagnostics: &dyn DiagnosticSink,
) {
    let patterns: Vec<crate::script::Pattern> = patterns
        .iter()
        .map(|p| crate::script::Pattern::section(p.as_bytes()))
        .collect();
    let mut matches: Vec<(crate::ids::SymbolId, SectionId)> = refs
        .symbols
        .ids()
        .filter_map(|id| {
            let name = refs.symbols.name(id);
            if !patterns.iter().any(|p| p.matches(name.bytes())) {
                return None;
            }
            let target = refs.global_target(id, true);
            Some((id, refs.target_section(&target)?))
        })
        .collect();
    matches.sort_unstable();
    for (id, section) in matches {
        let name = refs.symbols.name(id);
        let message = match crate::passes::why_live(graph, section) {
            Some(chain) => {
                let mut diagnostic = Diagnostic::new(
                    crate::diag::Severity::Note,
                    format!("live symbol: {}", name.display()),
                );
                for &link in chain.iter().rev().skip(1) {
                    diagnostic = diagnostic.note(format!("kept alive by {}", describe(refs, link)));
                }
                if chain.len() == 1 {
                    diagnostic = diagnostic.note("is a GC root".to_string());
                }
                diagnostic
            }
            None => Diagnostic::new(
                crate::diag::Severity::Note,
                format!("symbol {} is removed by --gc-sections", name.display()),
            ),
        };
        diagnostics.emit(message.order(u64::from(id.as_u32())));
    }
}

fn describe(refs: &Refs<'_, '_>, id: SectionId) -> String {
    let Some((file, index)) = refs.sections.locate(id) else {
        return String::new();
    };
    let Some(input) = refs.files.get(file) else {
        return String::new();
    };
    let name = input
        .object
        .as_ref()
        .and_then(|o| o.section(index))
        .map_or_else(String::new, |s| {
            String::from_utf8_lossy(s.name).into_owned()
        });
    format!("{}:({name})", input.display())
}

fn relocation_count(refs: &Refs<'_, '_>, section: SectionId) -> usize {
    let Some((file_index, index)) = refs.sections.locate(section) else {
        return 0;
    };
    let Some(object) = refs.files.get(file_index).and_then(|f| f.object.as_ref()) else {
        return 0;
    };
    let Some(input) = object.section(index) else {
        return 0;
    };
    if input.relocs == 0
        || input.kind == SectionKind::EhFrame
        || input.header.sh_flags & SHF_ALLOC == 0
    {
        return 0;
    }
    // Through the reader, which checks the table lies inside the file: the
    // count sizes an allocation.
    match object
        .section(input.relocs)
        .map(|r| object.elf.relocation_section(input.relocs, &r.header))
    {
        Some(Ok(Some(relocations))) => relocations.relocations.len(),
        _ => 0,
    }
}

/// Prints `--print-gc-sections` lines for removed allocated sections.
pub fn print_removed(refs: &Refs<'_, '_>, removed: &[SectionId], diagnostics: &dyn DiagnosticSink) {
    for &id in removed {
        let Some((file_index, index)) = refs.sections.locate(id) else {
            continue;
        };
        let Some(file) = refs.files.get(file_index) else {
            continue;
        };
        let Some(section) = file.object.as_ref().and_then(|o| o.section(index)) else {
            continue;
        };
        if section.header.sh_flags & SHF_ALLOC == 0 || section.header.sh_size == 0 {
            continue;
        }
        diagnostics.emit(
            Diagnostic::new(
                crate::diag::Severity::Note,
                format!(
                    "removing unused section '{}' in file '{}'",
                    String::from_utf8_lossy(section.name),
                    file.display()
                ),
            )
            .order(file.position.raw()),
        );
    }
}
