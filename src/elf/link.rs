//! The ELF link driver: runs the pipeline in `docs/architecture.md` for an ELF
//! target, from resolved options to a written output file.
//!
//! Each stage lives in its own module; this file only sequences them and
//! turns problems into diagnostics:
//!
//! 1. [`inputs`](super::inputs): search paths, archives, input scripts;
//! 2. [`resolve_symbols`] with [`ElfRules`], then COMDAT deduplication;
//! 3. [`place`](super::place): output section assignment;
//! 4. linker-defined symbols ([`defined`](super::defined));
//! 5. `.eh_frame` splitting and `--gc-sections` ([`gc`](super::gc));
//! 6. the relocation scan ([`scan`](super::scan)) and undefined symbols;
//! 7. common symbols, merged sections, live `.eh_frame` records;
//! 8. synthetic sections and the symbol table plan;
//! 9. [`layout`](super::layout), symbol addresses, and
//!    [`write`](super::write).

use std::time::Instant;

use crate::args::{LinkOptions, OutputKind, StripMode};
use crate::diag::{Diagnostic, DiagnosticSink};
use crate::error::{Error, Result};
use crate::input::FileTable;
use crate::symbols::{SymbolName, SymbolTable, resolve_symbols};

use super::common;
use super::defined;
use super::ehframe;
use super::gc;
use super::inputs::{self, InternalNames, parse_number};
use super::layout::{self, LayoutInput, TrailerSizes};
use super::merge;
use super::object::{ParseConfig, WrapTable};
use super::place;
use super::refs::{Def, Refs};
use super::resolve::{self, ElfRules};
use super::rules::RuleSet;
use super::scan::{self, UndefinedRef};
use super::sections::Sections;
use super::symtab;
use super::synth::{self, Synth};
use super::values::Addresses;
use super::write::{self, WriteInput};

/// At most this many references are listed per undefined symbol.
const MAX_REFERENCES: usize = 3;

/// Links an ELF output described by `options`.
///
/// Called by [`crate::link`] inside a rayon pool sized by `--threads`.
///
/// # Errors
///
/// Returns [`Error::Unimplemented`] for output kinds of later milestones,
/// [`Error::Reported`] when errors were reported to `diagnostics`, and any
/// I/O or parse error.
pub fn link(options: &LinkOptions, diagnostics: &dyn DiagnosticSink) -> Result<()> {
    match options.kind {
        OutputKind::StaticExecutable | OutputKind::Executable => {}
        OutputKind::Pie | OutputKind::StaticPie => {
            return Err(Error::Unimplemented(
                "position-independent executables (roadmap M2: dynamic ELF)".into(),
            ));
        }
        OutputKind::Shared => {
            return Err(Error::Unimplemented(
                "shared objects (roadmap M2: dynamic ELF)".into(),
            ));
        }
        OutputKind::Relocatable => {
            return Err(Error::Unimplemented(
                "relocatable output (roadmap M2: dynamic ELF)".into(),
            ));
        }
    }
    if !options.plugins.is_empty()
        && options
            .plugins
            .iter()
            .any(|(_, opts)| opts.iter().any(|o| o == "-fresolution="))
    {
        return Err(Error::Unimplemented("LTO plugins (roadmap M6)".into()));
    }
    let timing = std::env::var_os("QLD_TIMING").is_some();
    let start = Instant::now();
    let lap = |what: &str| {
        if timing {
            eprintln!(
                "qld: {what}: {:.1} ms",
                start.elapsed().as_secs_f64() * 1000.0
            );
        }
    };

    let wrap = WrapTable::new(&options.wrap);
    let internal = InternalNames::new(options);
    let table = FileTable::new();
    let config = ParseConfig {
        strip_debug: options.strip >= StripMode::Debug,
        wrap: &wrap,
    };
    let mut inputs = inputs::collect(options, &table, &internal, config)?;
    lap("inputs");

    let rules = ElfRules {
        allow_multiple_definition: options.allow_multiple_definition,
    };
    let mut symbols = SymbolTable::new();
    let resolution = resolve_symbols(&mut symbols, &rules, &mut inputs.files)?;
    let files = &inputs.files;
    lap("resolution");

    let mut sections = Sections::new(files, &resolution)?;
    resolve::deduplicate_comdat(files, &mut sections);
    resolve::redirect_discarded(&symbols, &rules, files, &resolution, &sections);
    let mut errors = resolve::report_duplicates(files, &resolution, &sections, diagnostics);
    report_gnu_warnings(files, &symbols, diagnostics);

    let rule_set = RuleSet::default_rules();
    let placement = place::place(&rule_set, files, &sections);
    let linker = defined::register(&symbols, &placement, options);
    lap("placement");

    let mut eh_frames = ehframe::split(files, &sections)?;
    if options.gc_sections {
        let refs = Refs {
            files,
            symbols: &symbols,
            resolution: &resolution,
            sections: &sections,
        };
        let removed = gc::collect(&refs, &placement, &eh_frames, &linker, &internal)?;
        if options.print_gc_sections {
            gc::print_removed(&refs, &removed, diagnostics);
        }
        for id in &removed {
            if let Some(slot) = sections.live.get_mut(id.index()) {
                *slot = false;
            }
        }
        eh_frames
            .sections
            .retain(|s| sections.live.get(s.id.index()).copied().unwrap_or(false));
        lap("gc");
    }

    let refs = Refs {
        files,
        symbols: &symbols,
        resolution: &resolution,
        sections: &sections,
    };
    let scan = scan::scan(&refs, options.relax);
    for file in &scan.files {
        for error in &file.errors {
            diagnostics.emit(error.clone());
            errors = errors.saturating_add(1);
        }
    }
    errors = errors.saturating_add(report_undefined(
        &refs,
        &scan,
        options,
        &internal,
        diagnostics,
    ));
    if errors > 0 && !options.noinhibit_exec {
        return Err(Error::Reported { errors });
    }
    lap("scan");

    let commons = common::allocate(&refs);
    let merged = merge::merge(files, &sections, &placement, options.optimize >= 2)?;
    eh_frames.finalize(&refs);
    lap("merge");

    let mut synth = Synth::default();
    synth.plan_entries(&symbols, &scan);
    synth.build_id = synth::plan_build_id(options);
    synth.property_note = synth::plan_property_note(files, options);
    synth.fde_count = u64::try_from(eh_frames.live_fdes()).unwrap_or(0);
    synth.eh_frame_hdr = options.eh_frame_hdr && synth.fde_count > 0;
    synth.eh_frame_end = eh_frames.sections.iter().any(|s| s.size > 0);
    synth.common = (commons.size, commons.align);

    let plan = symtab::plan(&refs, &linker, options);
    let trailers = TrailerSizes {
        symtab: plan.symtab_size(),
        strtab: if plan.is_empty() {
            0
        } else {
            u64::try_from(plan.strtab_size).unwrap_or(u64::MAX)
        },
        first_global: u32::try_from(plan.first_global).unwrap_or(0),
    };
    let exec_stack = files
        .iter()
        .filter_map(|f| f.object.as_ref())
        .any(|o| o.exec_stack);
    let layout = layout::layout(&LayoutInput {
        options,
        rules: &rule_set,
        files,
        sections: &sections,
        placement: &placement,
        merged: &merged,
        eh_frames: &eh_frames,
        synth: &synth,
        trailers,
        exec_stack,
    })?;
    lap("layout");

    let addresses = Addresses::new(
        refs, &layout, &merged, &eh_frames, &synth, &commons, &placement, &linker, options,
    );
    let entry = entry_address(&addresses, options, diagnostics);
    write::write(&WriteInput {
        options,
        addresses: &addresses,
        symtab: &plan,
        linker: &linker,
        entry,
        diagnostics,
    })?;
    lap("write");
    Ok(())
}

/// Reports undefined symbols, lld-style. Returns the number of errors.
fn report_undefined(
    refs: &Refs<'_, '_>,
    scan: &scan::ScanResult,
    options: &LinkOptions,
    internal: &InternalNames,
    diagnostics: &dyn DiagnosticSink,
) -> usize {
    let mut all: Vec<UndefinedRef> = scan
        .files
        .iter()
        .flat_map(|f| f.undefined.iter().copied())
        .collect();
    all.sort_unstable_by_key(|r| (r.symbol, r.file, r.section, r.offset));
    let mut errors = 0usize;
    let mut groups: Vec<&[UndefinedRef]> = all.chunk_by(|a, b| a.symbol == b.symbol).collect();
    groups.sort_by_key(|group| group.first().map(|r| (r.file, r.section, r.offset)));
    let ignore = matches!(
        options.unresolved_symbols,
        Some(
            crate::args::UnresolvedSymbols::IgnoreAll
                | crate::args::UnresolvedSymbols::IgnoreInObjectFiles
        )
    );
    for group in groups {
        let Some(first) = group.first() else {
            continue;
        };
        let name = refs.symbols.name(first.symbol);
        if ignore
            || options
                .ignore_unresolved_symbols
                .iter()
                .any(|s| s.as_bytes() == name.bytes())
        {
            continue;
        }
        let order = refs.files.get(first.file).map_or(0, |f| f.position.raw());
        let mut diagnostic = if options.warn_unresolved_symbols {
            Diagnostic::warning(format!("undefined symbol: {}", name.display()))
        } else {
            Diagnostic::error(format!("undefined symbol: {}", name.display()))
        };
        diagnostic = diagnostic.order(order);
        for reference in group.iter().take(MAX_REFERENCES) {
            diagnostic = diagnostic.at(scan::location(
                refs,
                reference.file,
                reference.section,
                reference.offset,
            ));
        }
        if group.len() > MAX_REFERENCES {
            diagnostic = diagnostic.note(format!(
                "referenced {} more times",
                group.len().saturating_sub(MAX_REFERENCES)
            ));
        }
        diagnostics.emit(diagnostic);
        if !options.warn_unresolved_symbols {
            errors = errors.saturating_add(1);
        }
    }
    for name in &options.require_defined {
        let defined = refs
            .symbols
            .lookup(&SymbolName::new(name.as_bytes()))
            .is_some_and(|id| !matches!(refs.global_target(id, false).def, Def::Undefined { .. }));
        if !defined {
            diagnostics.emit(Diagnostic::error(format!(
                "required symbol '{name}' is not defined"
            )));
            errors = errors.saturating_add(1);
        }
    }
    let _ = internal;
    errors
}

/// Emits the messages of `.gnu.warning.SYM` sections whose symbol is
/// referenced (and of plain `.gnu.warning` sections), as GNU ld does.
fn report_gnu_warnings(
    files: &[super::inputs::ElfInput<'_>],
    symbols: &SymbolTable<'_>,
    diagnostics: &dyn DiagnosticSink,
) {
    for file in files {
        let Some(object) = &file.object else {
            continue;
        };
        for &index in &object.warnings {
            let Some(section) = object.section(index) else {
                continue;
            };
            let symbol = section.name.strip_prefix(b".gnu.warning.");
            let used = match symbol {
                Some(name) => symbols.lookup(&SymbolName::new(name)).is_some_and(|id| {
                    symbols
                        .flags(id)
                        .contains(crate::symbols::SymbolFlags::REFERENCED)
                }),
                None => section.name == b".gnu.warning",
            };
            if !used {
                continue;
            }
            let text = object.elf.section_data(&section.header).unwrap_or_default();
            let text = text.split(|&b| b == 0).next().unwrap_or_default();
            diagnostics.emit(
                Diagnostic::warning(String::from_utf8_lossy(text).into_owned())
                    .order(file.position.raw()),
            );
        }
    }
}

fn entry_address(
    addresses: &Addresses<'_, '_>,
    options: &LinkOptions,
    diagnostics: &dyn DiagnosticSink,
) -> u64 {
    let name = options.entry.as_deref().unwrap_or("_start");
    if let Some(value) = parse_number(name) {
        return value;
    }
    let found = addresses
        .refs
        .symbols
        .lookup(&SymbolName::new(name.as_bytes()))
        .filter(|&id| {
            !matches!(
                addresses.refs.global_target(id, false).def,
                Def::Undefined { .. }
            )
        })
        .and_then(|id| addresses.globals.get(id.index()).copied());
    match found {
        Some(value) => value,
        None => {
            let text = addresses.layout.by_name(b".text").map_or(0, |s| s.addr);
            diagnostics.emit(Diagnostic::warning(format!(
                "cannot find entry symbol {name}; defaulting to {text:#x}"
            )));
            text
        }
    }
}
