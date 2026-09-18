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
//! 5. [`super::layout`]: output sections, grouped-section ordering,
//!    RVAs and file offsets;
//! 6. [`super::defined`]: the symbols MinGW's C runtime expects;
//! 7. [`super::write`](mod@super::write): relocations, base relocations and the image.
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

use super::arm64::Thunks;
use super::defined;
use super::directives::{Directives, ExportRequest};
use super::edata;
use super::implib;
use super::inputs::{self, CoffInput, InternalNames};
use super::layout::{self, CommonSymbol, LayoutInput};
use super::object::GlobalKind;
use super::options::AutoImport;
use super::options::PeOptions;
use super::read::consts::{
    IMAGE_SUBSYSTEM_WINDOWS_CUI, IMAGE_SUBSYSTEM_WINDOWS_GUI, IMAGE_WEAK_EXTERN_ANTI_DEPENDENCY,
};
use super::reloc::{self, Addresses, Value};
use super::resolve::{CoffRules, ComdatHook};
use super::safeseh;
use super::write::{self, WriteInput};

/// Largest alignment a common symbol gets without `-aligncomm:`.
const MAX_COMMON_ALIGN: u32 = 16;

/// How many times the image is laid out before the driver gives up on the
/// generated sizes and thunks settling. lld allows ten thunk passes.
const MAX_LAYOUT_PASSES: u32 = 12;

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
        let table = FileTable::for_link(options);
        let scan = inputs::collect(options, &table, &empty, pe.machine)?;
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

    // MinGW auto-import may need `__imp_` symbols that nothing referred to,
    // so their import library members were never extracted. The first
    // attempt finds them; the second links with them as roots.
    for attempt in 0..2u32 {
        let table = FileTable::for_link(options);
        let extra = link_once(options, pe, diagnostics, &table, &internal)?;
        if extra.is_empty() {
            return Ok(());
        }
        if attempt == 1 {
            return Err(Error::Internal(
                "auto-import did not settle after two resolution passes".into(),
            ));
        }
        for name in &extra {
            internal.push(name);
        }
    }
    Ok(())
}

/// One resolution and link attempt.
///
/// Returns the `__imp_` symbols auto-import needs as extra roots, which is
/// empty when the image was written.
#[allow(clippy::too_many_lines)]
fn link_once<'a>(
    options: &LinkOptions,
    pe: &PeOptions,
    diagnostics: &dyn DiagnosticSink,
    table: &'a FileTable,
    internal: &'a InternalNames,
) -> Result<Vec<Vec<u8>>> {
    let mut inputs = inputs::collect(options, table, internal, pe.machine)?;
    let files = &mut inputs.files;

    let rules = CoffRules {
        allow_multiple_definition: options.allow_multiple_definition,
    };
    let mut symbols = SymbolTable::new();
    let mut hook = ComdatHook::default();
    let resolution = resolve_symbols_with(&mut symbols, &rules, files, &mut hook)?;
    let files = &inputs.files;
    options.check_cancelled()?;

    let mut errors = super::resolve::report_conflicts(&hook.table.conflicts, files, diagnostics);
    errors = errors.saturating_add(report_duplicates(&resolution, files, options, diagnostics));

    // `.drectve` directives from every live file: exports, aligncomm and
    // alternate names.
    let mut directives = Directives::default();
    for file in files {
        directives.add_from(file)?;
    }
    let mut aliases = alias_table(&symbols, files, &resolution, &directives);
    // GNU ld's stdcall fixup binds `_foo@8` to `_foo` and back on i386,
    // before auto-import looks at what is left.
    let fixups = stdcall_fixups(&symbols, pe, &mut aliases);
    // Auto-import must be decided before undefined symbols are reported: it
    // is what binds a reference to a DLL's data that was compiled without
    // `__declspec(dllimport)`.
    let auto_imported = if pe.auto_import == AutoImport::Enabled {
        let pending = pending_auto_imports(&symbols);
        if !pending.is_empty() {
            return Ok(pending);
        }
        auto_import_table(&symbols, &mut aliases)
    } else {
        HashMap::default()
    };
    if pe.enable_stdcall_fixup.is_none() {
        let mut warned = false;
        for (from, to) in &fixups {
            edata::warn_fixup(from, to, &mut warned, diagnostics);
        }
    }
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
    let seh = safeseh::plan(pe, &symbols, files, &resolution)?;
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
    let exports = edata::plan(
        &requests,
        &symbols,
        files,
        &resolution,
        pe,
        &dll_name,
        diagnostics,
    );
    let export_size = exports.size();
    if let Some(path) = &pe.out_implib {
        implib::write(path, &exports, pe.machine)?;
    }
    if let Some(path) = &pe.output_def {
        implib::write_def(path, &exports)?;
    }

    // Lay out, relocate, then lay out again with the real `.reloc` and
    // pseudo-relocation sizes, and with the ARM64 range-extension thunks the
    // relocation pass asked for. The two sizes only grow the end of a
    // section, so they settle in two rounds; thunks move code, and may push
    // another branch out of range, so they take as many rounds as it takes
    // for no branch to ask for a new one.
    let mut reloc_size = 0u32;
    let mut pseudo_size = 0u32;
    let mut thunks = Thunks::default();
    let mut attempt = 0u32;
    loop {
        options.check_cancelled()?;
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
            pseudo_reloc_size: pseudo_size,
            thunks: &thunks,
            safe_seh_size: seh.reserved_size(),
        })?;
        let linker = defined::values(&plan, &symbols, pe.section_alignment);
        let mut addresses = Addresses {
            files,
            symbols: &symbols,
            resolution: &resolution,
            layout: &plan,
            image_base: pe.effective_image_base(),
            linker,
            commons: common_values(&plan, &commons),
            aliases: aliases.clone(),
            auto_imported: auto_imported.clone(),
        };
        // The SafeSEH table's symbols must be known before relocating the
        // load configuration that refers to them.
        let seh_table = match &seh {
            safeseh::Plan::Nothing => None,
            safeseh::Plan::Zero => {
                addresses
                    .linker
                    .extend(safeseh::symbol_values(&symbols, None, 0));
                None
            }
            safeseh::Plan::Table(handlers) => {
                let rvas = safeseh::handler_rvas(&addresses, handlers);
                let at = plan.markers.iter().find_map(|&(marker, section, offset)| {
                    (marker == layout::Marker::SafeSehTable).then_some((section, offset))
                });
                let table = at.and_then(|(section, offset)| {
                    Some(Value::Address {
                        rva: plan
                            .sections
                            .get(section as usize)?
                            .rva
                            .wrapping_add(offset),
                        section,
                    })
                });
                addresses
                    .linker
                    .extend(safeseh::symbol_values(&symbols, table, rvas.len()));
                at.map(|(section, offset)| (section, offset, safeseh::encode(&rvas)))
            }
        };
        let mut generated: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        // Export problems are reported once, from the final pass.
        let export_diagnostics = crate::diag::Collect::new();
        if export_size > 0
            && let Some(section) = plan.by_name(b".edata")
        {
            generated.push((
                b".edata".to_vec(),
                exports.render(&addresses, section.rva, &export_diagnostics),
            ));
        }
        let (contents, applied) = write::render(&addresses, &generated);
        let encoded = if emit_relocs {
            // The loader never maps the debugging sections, so their
            // addresses are not rebased (GNU ld writes none for them).
            let loaded: Vec<reloc::BaseReloc> = applied
                .base_relocs
                .iter()
                .copied()
                .filter(|site| {
                    !plan.sections.iter().any(|section| {
                        layout::is_debug_section(&section.name)
                            && site.rva >= section.rva
                            && site.rva.wrapping_sub(section.rva) < section.virtual_size
                    })
                })
                .collect();
            reloc::encode_base_relocs(&loaded)
        } else {
            Vec::new()
        };
        let pseudo = reloc::encode_pseudo_relocs(&applied.pseudo_relocs);
        let wanted = u32::try_from(encoded.len()).unwrap_or(u32::MAX);
        let wanted_pseudo = u32::try_from(pseudo.len()).unwrap_or(u32::MAX);
        attempt = attempt.saturating_add(1);
        let more_thunks = thunks.add(&applied.thunk_requests);
        if (emit_relocs && wanted != reloc_size) || wanted_pseudo != pseudo_size || more_thunks {
            if attempt > MAX_LAYOUT_PASSES {
                return Err(Error::Internal(
                    "the generated section sizes did not converge".into(),
                ));
            }
            reloc_size = wanted;
            pseudo_size = wanted_pseudo;
            continue;
        }

        for diagnostic in export_diagnostics.take_sorted() {
            diagnostics.emit(diagnostic);
        }
        let errors = write::report(&applied.errors, diagnostics);
        if errors > 0 && !options.noinhibit_exec {
            return Err(Error::Reported { errors });
        }
        let has_relocs = !encoded.is_empty();
        generated.push((b".reloc".to_vec(), encoded));
        let (mut contents, _) = if has_relocs {
            write::render(&addresses, &generated)
        } else {
            (contents, applied)
        };
        // The pseudo-relocation list sits inside `.rdata`, between the
        // bounds `__RUNTIME_PSEUDO_RELOC_LIST__` names.
        if !pseudo.is_empty()
            && let Some(&(_, section, offset)) = plan
                .markers
                .iter()
                .find(|&&(marker, _, _)| marker == layout::Marker::PseudoStart)
            && let Some(bytes) = contents.get_mut(section as usize)
        {
            let start = offset as usize;
            let end = start.saturating_add(pseudo.len());
            if let Some(slot) = bytes.get_mut(start..end) {
                slot.copy_from_slice(&pseudo);
            }
        }

        if let Some((section, offset, bytes)) = &seh_table
            && let Some(data) = contents.get_mut(*section as usize)
        {
            let start = *offset as usize;
            if let Some(slot) = data.get_mut(start..start.saturating_add(bytes.len())) {
                slot.copy_from_slice(bytes);
            }
        }

        let symbols = if options.strip >= StripMode::All {
            super::symtab::SymbolTable::default()
        } else {
            super::symtab::build(&addresses, &plan)
        };
        let subsystem = subsystem(&addresses, pe);
        let entry = entry_rva(&addresses, options, pe, subsystem, diagnostics);
        let mut directories = write::section_directories(&plan);
        write::symbol_directories(&addresses, pe.target(), &mut directories);
        write::load_config_size(&plan, pe, subsystem, &contents, &mut directories);
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
                symbols: &symbols,
                output: crate::output::OutputOptions::for_link(options),
            },
            &contents,
        )?;
        return Ok(Vec::new());
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
    let machine = super::machine::Machine::from_coff(pe.machine)?;
    // `--oformat` names a BFD target, which must be this machine's.
    if let Some(format) = &options.output_format
        && !machine.bfd_names().contains(&format.name())
    {
        return Err(Error::Option(format!(
            "--oformat {} does not match the {} emulation (expected {})",
            format.name(),
            match machine {
                super::machine::Machine::Amd64 => "i386pep",
                super::machine::Machine::I386 => "i386pe",
                super::machine::Machine::Arm64 => "arm64pe",
            },
            machine.bfd_names().join(" or ")
        )));
    }
    if options.gc_sections {
        return unimplemented("--gc-sections");
    }
    if options.icf != crate::args::IcfMode::None {
        return unimplemented("--icf");
    }
    if !options.wrap.is_empty() {
        return unimplemented("--wrap");
    }
    if !options.defsym.is_empty() {
        return unimplemented("--defsym");
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
///
/// `-e` names a COFF symbol as written (`-e _start` on i386); the default
/// is a C name, decorated for the machine.
fn entry_symbol(options: &LinkOptions, pe: &PeOptions) -> Vec<u8> {
    if let Some(entry) = &options.entry {
        return entry.as_bytes().to_vec();
    }
    let gui = pe.subsystem == Some(IMAGE_SUBSYSTEM_WINDOWS_GUI);
    pe.target().default_entry(pe.dll, gui)
}

/// The subsystem of the image: `--subsystem` if given, otherwise inferred
/// from the entry points the link defines, and console by default.
fn subsystem(addresses: &Addresses<'_, '_>, pe: &PeOptions) -> u16 {
    if let Some(subsystem) = pe.subsystem {
        return subsystem;
    }
    let machine = pe.target();
    let defined = |name: &[u8]| addresses.by_name(&machine.decorate(name)).is_some();
    // `WinMain` is `__stdcall`: `_WinMain@16` on i386.
    let gui = defined(b"WinMain")
        || defined(b"wWinMain")
        || defined(b"WinMain@16")
        || defined(b"wWinMain@16");
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
    let machine = pe.target();
    let mut candidates: Vec<Vec<u8>> = Vec::new();
    if let Some(entry) = &options.entry {
        candidates.push(entry.as_bytes().to_vec());
    } else if pe.dll {
        candidates.push(machine.default_entry(true, false));
    } else if subsystem == IMAGE_SUBSYSTEM_WINDOWS_GUI {
        candidates.push(machine.default_entry(false, true));
        candidates.push(machine.default_entry(false, false));
    } else {
        candidates.push(machine.default_entry(false, false));
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
        if is_unresolved(symbols, alias) {
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
            if !is_unresolved(symbols, id) {
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

/// GNU `ld`'s stdcall fixup: an undefined `_foo@N` (or `@foo@N`) binds to
/// a defined `_foo`, and an undefined `_foo` to a defined `_foo@N`.
///
/// Only i386 decorates names this way. `--disable-stdcall-fixup` turns
/// the fixup off; without `--enable-stdcall-fixup` the caller warns about
/// each one. Returns the `(from, to)` pairs, in symbol order.
fn stdcall_fixups(
    symbols: &SymbolTable<'_>,
    pe: &PeOptions,
    aliases: &mut HashMap<SymbolId, SymbolId>,
) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut fixed = Vec::new();
    if !pe.target().underscores() || pe.enable_stdcall_fixup == Some(false) {
        return fixed;
    }
    let defined = |name: &[u8]| {
        symbols
            .lookup(&SymbolName::new(name))
            .is_some_and(|id| symbols.definition_kind(id) == DefinitionKind::Regular)
    };
    for id in symbols.ids() {
        if !is_unresolved(symbols, id)
            || aliases.contains_key(&id)
            || !symbols
                .flags(id)
                .intersects(SymbolFlags::REFERENCED | SymbolFlags::WEAK_REFERENCED)
        {
            continue;
        }
        let name = symbols.name(id).bytes();
        let Some(twin) = edata::stdcall_twin(symbols, name, &defined) else {
            continue;
        };
        if let Some(target) = symbols.lookup(&SymbolName::new(&twin)) {
            aliases.insert(id, target);
            fixed.push((name.to_vec(), twin));
        }
    }
    fixed
}

/// The `__imp_` symbols auto-import needs but that resolution left lazy,
/// because nothing referred to them: an import library member defines them,
/// and only an extra root pulls it in.
///
/// Returning a non-empty list makes the driver resolve again with these
/// names as roots.
fn pending_auto_imports(symbols: &SymbolTable<'_>) -> Vec<Vec<u8>> {
    let mut pending = Vec::new();
    for id in symbols.ids() {
        if symbols.definition_kind(id) != DefinitionKind::Undefined
            || !symbols
                .flags(id)
                .intersects(SymbolFlags::REFERENCED | SymbolFlags::WEAK_REFERENCED)
        {
            continue;
        }
        let name = symbols.name(id).bytes();
        if name.starts_with(super::read::IMP_PREFIX) {
            continue;
        }
        let imp = [super::read::IMP_PREFIX, name].concat();
        if symbols
            .lookup(&SymbolName::new(&imp))
            .is_some_and(|slot| symbols.definition_kind(slot) == DefinitionKind::Lazy)
        {
            pending.push(imp);
        }
    }
    pending.sort_unstable();
    pending.dedup();
    pending
}

/// The symbols MinGW auto-import binds to an import address table slot.
///
/// A reference to a DLL's *data* that was compiled without
/// `__declspec(dllimport)` leaves `<name>` undefined while `__imp_<name>` is
/// defined by the import library. `--enable-auto-import` binds `<name>` to
/// the slot's address and records a runtime pseudo-relocation, which
/// `_pei386_runtime_relocator` turns into the slot's contents at startup.
fn auto_import_table(
    symbols: &SymbolTable<'_>,
    aliases: &mut HashMap<SymbolId, SymbolId>,
) -> HashMap<SymbolId, SymbolId> {
    let mut table = HashMap::default();
    for id in symbols.ids() {
        if !is_unresolved(symbols, id) || aliases.contains_key(&id) {
            continue;
        }
        if !symbols
            .flags(id)
            .intersects(SymbolFlags::REFERENCED | SymbolFlags::WEAK_REFERENCED)
        {
            continue;
        }
        let name = symbols.name(id).bytes();
        if name.starts_with(super::read::IMP_PREFIX) {
            continue;
        }
        let imp = [super::read::IMP_PREFIX, name].concat();
        let Some(slot) = symbols.lookup(&SymbolName::new(&imp)) else {
            continue;
        };
        if is_unresolved(symbols, slot) {
            continue;
        }
        aliases.insert(id, slot);
        table.insert(id, slot);
    }
    table
}

/// Whether a symbol ends the link without a definition in the image.
///
/// A [`DefinitionKind::Lazy`] best definition means the only candidate is an
/// archive member nothing extracted, which is what a COFF weak external with
/// `IMAGE_WEAK_EXTERN_SEARCH_NOLIBRARY` leaves behind: the fallback applies,
/// and the member stays out of the link, as GNU `ld` does.
fn is_unresolved(symbols: &SymbolTable<'_>, id: SymbolId) -> bool {
    matches!(
        symbols.definition_kind(id),
        DefinitionKind::Undefined | DefinitionKind::Lazy
    )
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
    let linker_defined: Vec<&[u8]> = defined::names()
        .chain([safeseh::TABLE_SYMBOL, safeseh::COUNT_SYMBOL])
        .collect();
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
