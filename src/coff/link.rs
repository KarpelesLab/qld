//! The PE/COFF link driver: the pipeline of `docs/architecture.md` for a
//! Windows target, from resolved options to a written image.
//!
//! 1. [`inputs::collect`]: search paths, objects and archives. MinGW import
//!    libraries are ordinary archives of ordinary objects, so they need no
//!    special case.
//! 2. a pre-pass over the command line's `.drectve` sections, which may add
//!    `-defaultlib:` inputs and `-include:` roots;
//! 3. [`resolve_symbols_with`] with [`CoffRules`] and [`ComdatHook`], which
//!    claims COMDAT groups before each round's symbols are inserted;
//! 4. `--alternatename` and weak-external aliases, then common symbols;
//! 5. [`layout`](super::layout): output sections, grouped-section ordering,
//!    RVAs and file offsets;
//! 6. [`defined`](super::defined): the symbols MinGW's C runtime expects;
//! 7. [`write`](super::write): relocations, base relocations and the image.
//!
//! Base relocations are found while relocating, so `.reloc` is sized in a
//! second layout pass. It is the last section, so no address moves and the
//! pass converges immediately.

#![deny(clippy::arithmetic_side_effects)]

use hashbrown::HashMap;

use crate::args::{LinkOptions, OutputKind, StripMode};
use crate::diag::{Diagnostic, DiagnosticSink};
use crate::error::{Error, Result};
use crate::ids::{FileId, SymbolId};
use crate::input::FileTable;
use crate::symbols::{DefinitionKind, SymbolFlags, SymbolName, SymbolTable, resolve_symbols_with};

use super::defined;
use super::directives::{Directives, ExportRequest};
use super::edata;
use super::implib;
use super::inputs::{self, CoffInput, InternalNames};
use super::layout::{self, CommonSymbol, LayoutInput};
use super::object::GlobalKind;
use super::options::PeOptions;
use super::read::consts::{
    IMAGE_SUBSYSTEM_WINDOWS_CUI, IMAGE_SUBSYSTEM_WINDOWS_GUI, IMAGE_WEAK_EXTERN_ANTI_DEPENDENCY,
};
use super::reloc::{self, Addresses, Value};
use super::resolve::{CoffRules, ComdatHook};
use super::write::{self, WriteInput};

/// Largest alignment a common symbol gets without `-aligncomm:`.
const MAX_COMMON_ALIGN: u32 = 16;

/// Links a PE/COFF output described by `options`.
///
/// Called by [`crate::link`] when the target's format is
/// [`BinaryFormat::Pe`](crate::BinaryFormat::Pe).
///
/// # Errors
///
/// Returns [`Error::Unimplemented`] for options a later milestone covers,
/// [`Error::Reported`] when errors were reported to `diagnostics`, and any
/// I/O or parse error.
pub fn link(options: &LinkOptions, diagnostics: &dyn DiagnosticSink) -> Result<()> {
    let pe = PeOptions::from_link_options(options);
    link_with(options, &pe, diagnostics)
}

/// Links with explicit PE options.
///
/// [`LinkOptions`] does not carry the MinGW PE options yet (see
/// [`PeOptions`]), so library callers that need `--subsystem`,
/// `--out-implib` or the `DllCharacteristics` flags pass them here.
///
/// # Errors
///
/// As for [`link`].
pub fn link_with(
    options: &LinkOptions,
    pe: &PeOptions,
    diagnostics: &dyn DiagnosticSink,
) -> Result<()> {
    check_supported(options, pe)?;
    pe.validate()?;

    let entry_root = entry_symbol(options, pe);

    // A first pass reads the `.drectve` sections of the objects on the
    // command line: they may add `-defaultlib:` inputs and `-include:` roots,
    // both of which change what resolution must find.
    let prescan = {
        let empty = InternalNames::default();
        let table = FileTable::new();
        let scan = inputs::collect(options, &table, &empty)?;
        let mut directives = Directives::default();
        let mut files = scan.files;
        for file in &mut files {
            if file.live_at_start {
                crate::symbols::ResolveFile::load(file)?;
                directives.add_from(file)?;
            }
        }
        directives
    };
    let mut internal = InternalNames::new(options, Some(&entry_root));
    for name in &prescan.includes {
        internal.push(name);
    }
    let extra_libraries = prescan.wanted_libraries(pe.no_default_lib);
    let options = if extra_libraries.is_empty() {
        options.clone()
    } else {
        with_libraries(options, &extra_libraries)
    };
    let options = &options;

    let table = FileTable::new();
    let mut inputs = inputs::collect(options, &table, &internal)?;
    let files = &mut inputs.files;

    let rules = CoffRules {
        allow_multiple_definition: options.allow_multiple_definition,
    };
    let mut symbols = SymbolTable::new();
    let mut hook = ComdatHook::default();
    let resolution = resolve_symbols_with(&mut symbols, &rules, files, &mut hook)?;
    let files = &inputs.files;

    let mut errors = super::resolve::report_conflicts(&hook.table.conflicts, files, diagnostics);
    errors = errors.saturating_add(report_duplicates(&resolution, files, options, diagnostics));

    // `.drectve` directives from every live file: exports, aligncomm and
    // alternate names.
    let mut directives = Directives::default();
    for file in files {
        directives.add_from(file)?;
    }
    let aliases = alias_table(&symbols, files, &resolution, &directives);
    errors = errors.saturating_add(report_undefined(
        &symbols,
        &resolution,
        &aliases,
        files,
        options,
        diagnostics,
    ));
    if errors > 0 && !options.noinhibit_exec {
        return Err(Error::Reported { errors });
    }

    let commons = allocate_commons(&symbols, files, &resolution, &directives);
    let emit_relocs = pe.dynamicbase && !pe.disable_reloc_section;
    let output_path = options.output_path();

    // Exports: a `.def` file, `-export:` directives and the command line,
    // or everything a DLL defines.
    let mut requests = directives.exports.clone();
    requests.extend(command_line_exports(pe, diagnostics));
    if let Some(path) = &pe.def_file {
        requests.extend(def_exports(path)?);
    }
    let dll_name = pe
        .implib_dll_name
        .clone()
        .unwrap_or_else(|| edata::default_dll_name(&output_path));
    let exports = edata::plan(&requests, &symbols, files, &resolution, pe, &dll_name);
    let export_size = exports.size();
    if let Some(path) = &pe.out_implib {
        implib::write(path, &exports, pe.machine)?;
    }
    if let Some(path) = &pe.output_def {
        implib::write_def(path, &exports)?;
    }

    // Lay out, relocate, then lay out again with the real `.reloc` size.
    let mut reloc_size = 0u32;
    let mut attempt = 0u32;
    loop {
        let mut synthetic: Vec<(Vec<u8>, u32, u32)> = Vec::new();
        if export_size > 0 {
            synthetic.push((b".edata".to_vec(), export_size, 4));
        }
        if emit_relocs && reloc_size > 0 {
            synthetic.push((b".reloc".to_vec(), reloc_size, 4));
        }
        let plan = layout::layout(&LayoutInput {
            files,
            options: pe,
            commons: &commons,
            synthetic: &synthetic,
        })?;
        let linker = defined::values(&plan, &symbols, pe.section_alignment);
        let addresses = Addresses {
            files,
            symbols: &symbols,
            resolution: &resolution,
            layout: &plan,
            image_base: pe.effective_image_base(),
            linker,
            commons: common_values(&plan, &commons),
            aliases: aliases.clone(),
        };
        let mut generated: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        if export_size > 0
            && let Some(section) = plan.by_name(b".edata")
        {
            generated.push((
                b".edata".to_vec(),
                exports.render(&addresses, section.rva, diagnostics),
            ));
        }
        let (contents, applied) = write::render(&addresses, &generated);
        let encoded = if emit_relocs {
            reloc::encode_base_relocs(&applied.base_relocs)
        } else {
            Vec::new()
        };
        let wanted = u32::try_from(encoded.len()).unwrap_or(u32::MAX);
        attempt = attempt.saturating_add(1);
        if emit_relocs && wanted != reloc_size {
            if attempt > 4 {
                return Err(Error::Internal(".reloc size did not converge".into()));
            }
            reloc_size = wanted;
            continue;
        }

        let errors = write::report(&applied.errors, diagnostics);
        if errors > 0 && !options.noinhibit_exec {
            return Err(Error::Reported { errors });
        }
        let has_relocs = !encoded.is_empty();
        generated.push((b".reloc".to_vec(), encoded));
        let (contents, _) = if has_relocs {
            write::render(&addresses, &generated)
        } else {
            (contents, applied)
        };

        let subsystem = subsystem(&addresses, pe);
        let entry = entry_rva(&addresses, options, pe, subsystem, diagnostics);
        let mut directories = write::section_directories(&plan);
        write::symbol_directories(&addresses, &mut directories);
        write::write(
            &WriteInput {
                addresses: &addresses,
                options: pe,
                path: &output_path,
                entry,
                subsystem,
                directories,
                generated: &generated,
                emit_base_relocs: emit_relocs,
            },
            &contents,
        )?;
        return Ok(());
    }
}

/// The `-export:` specifications given on the command line.
fn command_line_exports(pe: &PeOptions, diagnostics: &dyn DiagnosticSink) -> Vec<ExportRequest> {
    let mut requests = Vec::new();
    for text in &pe.exports {
        match super::read::directives::parse_export(text) {
            Ok(spec) => requests.push(ExportRequest::from_spec(&spec)),
            Err(problem) => diagnostics.emit(Diagnostic::error(format!(
                "invalid export `{}`: {problem}",
                String::from_utf8_lossy(text)
            ))),
        }
    }
    requests
}

/// The `EXPORTS` entries of a module-definition file.
fn def_exports(path: &std::path::Path) -> Result<Vec<ExportRequest>> {
    let text = std::fs::read(path).map_err(|error| Error::Io {
        path: Some(path.to_path_buf()),
        source: error,
    })?;
    let definition = super::read::parse_module_definition(&text, super::read::Source::new(path))?;
    Ok(definition
        .exports
        .iter()
        .map(ExportRequest::from_spec)
        .collect())
}

/// Rejects options whose effect is not implemented for PE yet, rather than
/// silently producing a different binary.
fn check_supported(options: &LinkOptions, pe: &PeOptions) -> Result<()> {
    let unimplemented = |what: &str| Err(Error::Unimplemented(format!("{what} for PE/COFF")));
    if options.kind == OutputKind::Relocatable {
        return unimplemented("-r");
    }
    if pe.machine != super::read::consts::IMAGE_FILE_MACHINE_AMD64 {
        return Err(Error::Unimplemented(format!(
            "PE output for machine {:#x} (roadmap M7: x86-64 first)",
            pe.machine
        )));
    }
    if options.gc_sections {
        return unimplemented("--gc-sections");
    }
    if options.icf.is_some() {
        return unimplemented("--icf");
    }
    if !options.wrap.is_empty() {
        return unimplemented("--wrap");
    }
    if !options.defsym.is_empty() {
        return unimplemented("--defsym");
    }
    if options.strip < StripMode::All {
        // The COFF symbol table is not written yet; the image is complete
        // without it, so this is a warning-free silent difference recorded in
        // docs/compatibility.md rather than an error.
    }
    Ok(())
}

/// A copy of `options` with `-l` entries appended for `-defaultlib:` names.
fn with_libraries(options: &LinkOptions, libraries: &[Vec<u8>]) -> LinkOptions {
    use crate::args::{InputKind, InputSpec};
    let mut options = options.clone();
    for name in libraries {
        let Ok(name) = std::str::from_utf8(name) else {
            continue;
        };
        let position = options.inputs.len();
        options.inputs.push(InputSpec {
            kind: InputKind::Library(name.to_string()),
            attrs: crate::args::InputAttrs::default(),
            position,
        });
    }
    options
}

/// The symbol the link starts from, used as a resolution root.
fn entry_symbol(options: &LinkOptions, pe: &PeOptions) -> Vec<u8> {
    if let Some(entry) = &options.entry {
        return entry.as_bytes().to_vec();
    }
    if pe.dll {
        return b"DllMainCRTStartup".to_vec();
    }
    match pe.subsystem {
        Some(IMAGE_SUBSYSTEM_WINDOWS_GUI) => b"WinMainCRTStartup".to_vec(),
        _ => b"mainCRTStartup".to_vec(),
    }
}

/// The subsystem of the image: `--subsystem` if given, otherwise inferred
/// from the entry points the link defines, and console by default.
fn subsystem(addresses: &Addresses<'_, '_>, pe: &PeOptions) -> u16 {
    if let Some(subsystem) = pe.subsystem {
        return subsystem;
    }
    let defined = |name: &[u8]| addresses.by_name(name).is_some();
    let gui = defined(b"WinMain") || defined(b"wWinMain");
    let console = defined(b"main") || defined(b"wmain");
    if gui && !console {
        IMAGE_SUBSYSTEM_WINDOWS_GUI
    } else {
        IMAGE_SUBSYSTEM_WINDOWS_CUI
    }
}

/// The entry point's RVA.
fn entry_rva(
    addresses: &Addresses<'_, '_>,
    options: &LinkOptions,
    pe: &PeOptions,
    subsystem: u16,
    diagnostics: &dyn DiagnosticSink,
) -> u32 {
    let mut candidates: Vec<Vec<u8>> = Vec::new();
    if let Some(entry) = &options.entry {
        candidates.push(entry.as_bytes().to_vec());
    } else if pe.dll {
        candidates.push(b"DllMainCRTStartup".to_vec());
    } else if subsystem == IMAGE_SUBSYSTEM_WINDOWS_GUI {
        candidates.push(b"WinMainCRTStartup".to_vec());
        candidates.push(b"mainCRTStartup".to_vec());
    } else {
        candidates.push(b"mainCRTStartup".to_vec());
    }
    for name in &candidates {
        if let Some(Value::Address { rva, .. }) = addresses.by_name(name) {
            return rva;
        }
    }
    let shown = candidates
        .first()
        .map(|name| String::from_utf8_lossy(name).into_owned())
        .unwrap_or_default();
    let text = addresses
        .layout
        .by_name(b".text")
        .map_or(0, |section| section.rva);
    diagnostics.emit(Diagnostic::warning(format!(
        "cannot find entry symbol {shown}; defaulting to {text:#x}"
    )));
    text
}

/// The alias table: `--alternatename` and the weak externals that stayed
/// undefined.
fn alias_table<'a>(
    symbols: &SymbolTable<'a>,
    files: &[CoffInput<'a>],
    resolution: &crate::symbols::Resolution<'a>,
    directives: &Directives,
) -> HashMap<SymbolId, SymbolId> {
    let mut aliases = HashMap::default();
    for (alias, target) in &directives.alternate_names {
        let (Some(alias), Some(target)) = (
            symbols.lookup(&SymbolName::new(alias)),
            symbols.lookup(&SymbolName::new(target)),
        ) else {
            continue;
        };
        if symbols.definition_kind(alias) == DefinitionKind::Undefined {
            aliases.insert(alias, target);
        }
    }
    for (index, file) in files.iter().enumerate() {
        let Some(parsed) = file.object() else {
            continue;
        };
        if !resolution.is_live(FileId::new(index)) {
            continue;
        }
        let ids = resolution.symbol_ids(FileId::new(index));
        for (slot, global) in parsed.globals.iter().enumerate() {
            let GlobalKind::Weak { tag, .. } = global.kind else {
                continue;
            };
            let Some(&id) = ids.get(slot) else {
                continue;
            };
            if symbols.definition_kind(id) != DefinitionKind::Undefined {
                continue;
            }
            // The tag is a symbol record index; map it to a global.
            let Some(super::object::RecordTarget::Global(target)) =
                parsed.targets.get(tag as usize).copied()
            else {
                continue;
            };
            if let Some(&target) = ids.get(target as usize) {
                aliases.entry(id).or_insert(target);
            }
        }
    }
    aliases
}

/// Common symbols, in symbol ID order so the `.bss` layout is deterministic.
fn allocate_commons<'a>(
    symbols: &SymbolTable<'a>,
    files: &[CoffInput<'a>],
    resolution: &crate::symbols::Resolution<'a>,
    directives: &Directives,
) -> Vec<CommonSymbol> {
    let mut requested: HashMap<&[u8], u32> = HashMap::default();
    for (name, log2) in &directives.align_comm {
        let entry = requested.entry(name.as_slice()).or_insert(0);
        *entry = (*entry).max(*log2);
    }
    let mut commons = Vec::new();
    for id in symbols.ids() {
        let definition = symbols.definition(id);
        if definition.kind != DefinitionKind::Common {
            continue;
        }
        if !resolution.is_live(definition.file) {
            continue;
        }
        let size = u32::try_from(definition.aux).unwrap_or(u32::MAX);
        let name = symbols.name(id);
        let declared = files
            .get(definition.file.index())
            .and_then(CoffInput::object)
            .and_then(|parsed| parsed.globals.get(definition.index as usize))
            .map_or(0, |global| global.align_log2);
        let from_directive = requested.get(name.bytes()).copied().unwrap_or(0);
        let log2 = declared.max(from_directive);
        let align = if log2 == 0 {
            size.next_power_of_two().clamp(1, MAX_COMMON_ALIGN)
        } else {
            1u32.checked_shl(log2.min(13)).unwrap_or(1)
        };
        commons.push(CommonSymbol {
            symbol: id,
            size,
            align,
        });
    }
    commons
}

/// The address of each common symbol, from the `.bss` chunks the layout
/// appended for them, in the same order.
fn common_values(plan: &layout::Layout, commons: &[CommonSymbol]) -> HashMap<SymbolId, Value> {
    let mut values = HashMap::default();
    let Some(index) = plan.index_of(b".bss") else {
        return values;
    };
    let Some(section) = plan.sections.get(index as usize) else {
        return values;
    };
    let zeros = section
        .chunks
        .iter()
        .filter(|chunk| matches!(chunk.piece, layout::Piece::Zero));
    for (common, chunk) in commons.iter().zip(zeros) {
        values.insert(
            common.symbol,
            Value::Address {
                rva: section.rva.wrapping_add(chunk.offset),
                section: index,
            },
        );
    }
    values
}

/// Reports duplicate definitions. Returns the number of errors.
fn report_duplicates(
    resolution: &crate::symbols::Resolution<'_>,
    files: &[CoffInput<'_>],
    options: &LinkOptions,
    diagnostics: &dyn DiagnosticSink,
) -> usize {
    let mut errors = 0usize;
    for duplicate in resolution.duplicates() {
        let name = crate::hints::display_symbol(duplicate.name.bytes(), options.demangle);
        let where_ = |file: FileId| {
            files
                .get(file.index())
                .map_or_else(|| "<unknown>".to_string(), CoffInput::display)
        };
        let mut diagnostic = Diagnostic::error(format!("duplicate symbol: {name}"))
            .detail(format!("defined at {}", where_(duplicate.winner.file)));
        for other in &duplicate.others {
            diagnostic = diagnostic.note(format!("defined at {}", where_(other.file)));
        }
        diagnostics.emit(diagnostic);
        errors = errors.saturating_add(1);
    }
    errors
}

/// Reports symbols that stayed undefined after aliases are applied. Returns
/// the number of errors.
fn report_undefined(
    symbols: &SymbolTable<'_>,
    resolution: &crate::symbols::Resolution<'_>,
    aliases: &HashMap<SymbolId, SymbolId>,
    files: &[CoffInput<'_>],
    options: &LinkOptions,
    diagnostics: &dyn DiagnosticSink,
) -> usize {
    let linker_defined: Vec<&[u8]> = defined::names().collect();
    let mut errors = 0usize;
    for undefined in resolution.undefined() {
        let name = undefined.name.bytes();
        if aliases.contains_key(&undefined.symbol) || linker_defined.contains(&name) {
            continue;
        }
        // An anti-dependency weak external is a hint, not a requirement.
        if symbols
            .flags(undefined.symbol)
            .contains(SymbolFlags::WEAK_REFERENCED)
            && !symbols
                .flags(undefined.symbol)
                .contains(SymbolFlags::REFERENCED)
        {
            continue;
        }
        let shown = crate::hints::display_symbol(name, options.demangle);
        let mut diagnostic = Diagnostic::error(format!("undefined symbol: {shown}"));
        for reference in undefined.references.iter().take(3) {
            let file = files
                .get(reference.file.index())
                .map_or_else(|| "<unknown>".to_string(), CoffInput::display);
            diagnostic = diagnostic.note(format!("referenced by {file}"));
        }
        diagnostics.emit(diagnostic);
        errors = errors.saturating_add(1);
    }
    let _ = IMAGE_WEAK_EXTERN_ANTI_DEPENDENCY;
    errors
}
