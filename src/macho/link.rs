//! The Mach-O link driver: the pipeline of `docs/architecture.md` for Apple
//! targets, from resolved options to a written (and signed) image.
//!
//! For each architecture (in parallel when several `-arch` values ask for a
//! universal binary):
//!
//! 1. [`inputs::collect`]: search paths, objects, archives, dylibs and text
//!    stubs, with universal slices selected;
//! 2. [`resolve_symbols`] with [`MachRules`];
//! 3. [`Link::new`]: what every symbol resolved to, undefined symbol
//!    reports, `-undefined` handling; relocations are loaded in parallel;
//! 4. [`Link::mark_live`]: weak definition coalescing and `-dead_strip`
//!    (after which undefined symbols only dead code refers to are dropped);
//! 5. [`scan::scan`]: stubs, `__got`, `__thread_ptrs`, imports and dylib
//!    ordinals;
//! 6. [`layout::plan`] and [`Layout::assign_addresses`], with
//!    [`unwind`](super::unwind) sized before and built after;
//! 7. [`sections::write`]: atoms, relocations and synthetic sections,
//!    collecting pointer fixups;
//! 8. `__LINKEDIT`: fixups ([`fixups`]), the export trie, symbol tables;
//!    then the header, `LC_UUID` and the code signature.
//!
//! With `-r`, steps 1–4 run (without dead stripping) and
//! [`relocatable::write`](super::relocatable::write) writes an `MH_OBJECT`
//! instead of steps 5–8.

#![deny(clippy::arithmetic_side_effects)]

use rayon::prelude::*;

use crate::args::LinkOptions;
use crate::args::darwin::{DarwinInputKind, PlatformVersion};
use crate::diag::{Diagnostic, DiagnosticSink};
use crate::error::{Error, Result};
use crate::input::FileTable;
use crate::macho::read::consts::{
    EXPORT_SYMBOL_FLAGS_WEAK_DEFINITION, MH_BINDS_TO_WEAK, MH_WEAK_DEFINES,
};
use crate::macho::read::{Arch, FatFile, MachOFile, ObjectFile, Source};
use crate::output::hash::Md5;
use crate::output::{FileMode, OutputFile, OutputOptions};
use crate::symbols::{SymbolTable, resolve_symbols};

use super::addr::Addresses;
use super::buf::{align_up, to_u64, to_usize};
use super::codesign::{self, SignatureInput};
use super::config::Config;
use super::fat::{self, Slice};
use super::fixups;
use super::inputs::{self, InternalNames};
use super::layout::{self, Layout, SyntheticSizes};
use super::resolve::{MachRules, report_duplicates};
use super::scan;
use super::sections;
use super::state::{Link, SymbolDef};
use super::symtab::{self, ExportFilter};
use super::trie;
use super::write::{self, Commands, DylibLoad, HeaderInput, Linkedit};

/// Links a Mach-O output described by `options`.
///
/// Called by [`crate::link`] when the target's format is
/// [`BinaryFormat::MachO`](crate::BinaryFormat::MachO).
///
/// # Errors
///
/// Returns [`Error::Reported`] when errors were reported to `diagnostics`,
/// [`Error::Unimplemented`] for features a later step covers, and any I/O
/// or parse error.
pub fn link(options: &LinkOptions, diagnostics: &dyn DiagnosticSink) -> Result<()> {
    let bytes = link_to_bytes(options, diagnostics)?;
    let path = options.output_path();
    let mut output = OutputFile::create(
        &path,
        to_u64(bytes.len()),
        &OutputOptions {
            mode: FileMode::Executable,
            ..OutputOptions::default()
        },
    )?;
    output.write_at(0, &bytes)?;
    output.finish()?;
    Ok(())
}

/// Links and returns the output bytes instead of writing a file: a thin
/// Mach-O image for one architecture, or a universal binary for several.
///
/// # Errors
///
/// As for [`link`].
pub fn link_to_bytes(options: &LinkOptions, diagnostics: &dyn DiagnosticSink) -> Result<Vec<u8>> {
    if options.darwin.print_version {
        diagnostics.emit(Diagnostic::new(
            crate::diag::Severity::Note,
            crate::version_line(),
        ));
    }
    let archs = match options.darwin.archs.as_slice() {
        [] => vec![infer_arch(options)?],
        archs => archs.to_vec(),
    };
    if archs.len() == 1 {
        let arch = archs.first().copied().unwrap_or(Arch::ARM64);
        return link_arch(options, arch, diagnostics);
    }
    let slices: Vec<Result<Slice>> = archs
        .par_iter()
        .map(|&arch| {
            let data = link_arch(options, arch, diagnostics)?;
            let cpu_subtype = MachOFile::parse(&data, Source::new(&options.output_path()))
                .map_or(0, |f| f.header().cpu_subtype);
            Ok(Slice {
                arch,
                cpu_subtype,
                data,
            })
        })
        .collect();
    fat::assemble(slices.into_iter().collect::<Result<Vec<_>>>()?)
}

/// The architecture of the first object on the command line, for links
/// without `-arch`.
fn infer_arch(options: &LinkOptions) -> Result<Arch> {
    for input in &options.darwin.inputs {
        let DarwinInputKind::File(path) = &input.kind else {
            continue;
        };
        let Ok(data) = std::fs::read(path) else {
            continue;
        };
        if let Ok(file) = MachOFile::parse(&data, Source::new(path)) {
            return Ok(file.header().arch());
        }
    }
    Err(Error::Option(
        "no -arch given and no Mach-O object to infer it from".into(),
    ))
}

/// The build version of the first object that has one.
fn infer_platform(options: &LinkOptions, arch: Arch) -> Option<PlatformVersion> {
    if options.darwin.platform.is_some() {
        return None;
    }
    for input in &options.darwin.inputs {
        let DarwinInputKind::File(path) = &input.kind else {
            continue;
        };
        let Ok(data) = std::fs::read(path) else {
            continue;
        };
        let source = Source::new(path);
        let slice = if FatFile::is_fat(&data) {
            match FatFile::parse(&data, source)
                .ok()
                .and_then(|f| f.select(arch).ok().map(|s| s.data.to_vec()))
            {
                Some(slice) => slice,
                None => continue,
            }
        } else {
            data
        };
        if let Ok(object) = ObjectFile::parse(&slice, source)
            && let Some(version) = object.build_version()
        {
            return Some(PlatformVersion {
                platform: version.platform,
                min: version.minos,
                sdk: version.sdk,
            });
        }
    }
    None
}

/// Links one architecture into an in-memory image.
#[allow(clippy::too_many_lines)]
fn link_arch(
    options: &LinkOptions,
    arch: Arch,
    diagnostics: &dyn DiagnosticSink,
) -> Result<Vec<u8>> {
    // Archive members extracted during resolution may ask for more
    // libraries with LC_LINKER_OPTION; link again with them.
    // Selector stubs (`_objc_msgSend$sel`) left undefined are generated in
    // an extra object; link again with it.
    let mut options = std::borrow::Cow::Borrowed(options);
    let mut selectors: Vec<Vec<u8>> = Vec::new();
    for _ in 0..8 {
        let generated: Vec<(std::path::PathBuf, std::sync::Arc<[u8]>)> = if selectors.is_empty() {
            Vec::new()
        } else {
            vec![(
                std::path::PathBuf::from("<objc selector stubs>"),
                std::sync::Arc::from(super::objc_stubs::object(arch, &selectors)),
            )]
        };
        match link_arch_once(&options, arch, diagnostics, &generated)? {
            Attempt::Done(bytes) => return Ok(bytes),
            Attempt::MoreInputs(more) => options.to_mut().darwin.inputs.extend(more),
            Attempt::Selectors(more) => {
                selectors.extend(more);
                selectors.sort();
                selectors.dedup();
            }
        }
    }
    Err(Error::Internal(
        "LC_LINKER_OPTION requests and selector stubs did not settle".into(),
    ))
}

enum Attempt {
    Done(Vec<u8>),
    MoreInputs(Vec<crate::args::darwin::DarwinInput>),
    Selectors(Vec<Vec<u8>>),
}

/// One link attempt for `arch`.
#[allow(clippy::too_many_lines)]
fn link_arch_once(
    options: &LinkOptions,
    arch: Arch,
    diagnostics: &dyn DiagnosticSink,
    generated: &[(std::path::PathBuf, std::sync::Arc<[u8]>)],
) -> Result<Attempt> {
    let config = Config::new(options, arch, infer_platform(options, arch))?;
    let table = FileTable::new();
    let collected = inputs::collect(options, &config, &table, diagnostics, generated)?;
    let internal = InternalNames::new(options, &config);
    let mut files = collected.files(&internal)?;

    let mut symbols = SymbolTable::new();
    let resolution = resolve_symbols(&mut symbols, &MachRules, &mut files)?;
    // `-r` leaves both to the final link.
    if !config.is_relocatable() {
        let more = inputs::missing_linker_options(options, &collected, &files, |index| {
            resolution.is_live(crate::ids::FileId::new(index))
        });
        if !more.is_empty() {
            return Ok(Attempt::MoreInputs(more));
        }
        let stubs: Vec<Vec<u8>> = resolution
            .undefined()
            .iter()
            .filter_map(|u| u.name.bytes().strip_prefix(super::objc_stubs::PREFIX))
            .filter(|selector| !selector.is_empty())
            .map(<[u8]>::to_vec)
            .collect();
        if !stubs.is_empty() {
            return Ok(Attempt::Selectors(stubs));
        }
    }
    let duplicates = report_duplicates(
        resolution.duplicates(),
        &files,
        options.demangle,
        diagnostics,
    );
    if duplicates > 0 && !options.noinhibit_exec {
        return Err(Error::Reported { errors: duplicates });
    }
    files
        .par_iter_mut()
        .enumerate()
        .try_for_each(|(index, file)| {
            if resolution.is_live(crate::ids::FileId::new(index))
                && let Some(object) = file.object.as_deref_mut()
            {
                object.load_relocations()?;
            }
            Ok::<(), Error>(())
        })?;

    let mut link = Link::new(
        &config,
        options,
        files,
        symbols,
        resolution,
        &collected.dylibs,
        &internal,
        diagnostics,
    )?;
    link.mark_live(options)?;
    link.report_live_undefined(options, diagnostics)?;
    if config.is_relocatable() {
        return super::relocatable::write(&link, options, diagnostics).map(Attempt::Done);
    }
    let filter = ExportFilter::new(options)?;
    let synthetic = scan::scan(&link, options, &filter)?;

    let entry_id = config
        .entry
        .as_ref()
        .and_then(|entry| link.symbols.lookup(&crate::symbols::SymbolName::new(entry)));
    if config.is_exec() {
        let defined = entry_id
            .is_some_and(|id| matches!(link.defs.get(id.index()), Some(SymbolDef::Object { .. })));
        if !defined {
            return Err(Error::Option(format!(
                "entry point {} is not defined",
                String::from_utf8_lossy(config.entry.as_deref().unwrap_or_default())
            )));
        }
    }

    let mut unwind_entries = super::unwind::collect(&link)?;
    let eh_frame_plan = super::eh_frame::plan(&link, &mut unwind_entries)?;
    let unwind_plan = super::unwind::plan(&link, unwind_entries);
    let sizes = SyntheticSizes {
        stubs: to_u64(synthetic.stubs.len()),
        got: to_u64(synthetic.got_slots()),
        thread_ptrs: to_u64(synthetic.thread_ptrs.len()),
        unwind_info: unwind_plan.size(),
        eh_frame: eh_frame_plan.size(),
    };
    let commons: Vec<(u64, u32)> = synthetic
        .commons
        .iter()
        .map(|id| match link.defs.get(id.index()) {
            Some(SymbolDef::Common { size, align, .. }) => (*size, *align),
            _ => (0, 0),
        })
        .collect();
    let mut sectcreate_data = Vec::new();
    let mut sectcreate = Vec::new();
    for (segment, section, path) in &options.darwin.sectcreate {
        let data = std::fs::read(path).map_err(|error| Error::io(path, error))?;
        sectcreate.push((
            segment.as_bytes().to_vec(),
            section.as_bytes().to_vec(),
            to_u64(data.len()),
        ));
        sectcreate_data.push(data);
    }
    let order = match &options.darwin.order_file {
        Some(path) => layout::read_order_file(path, arch.name().unwrap_or(""))?,
        None => hashbrown::HashMap::new(),
    };
    let mut layout = layout::plan(&link, &sizes, &commons, &sectcreate, &order)?;

    // Load commands.
    let mut commands = Commands {
        rpaths: options
            .rpaths
            .iter()
            .map(|p| p.as_os_str().as_encoded_bytes().to_vec())
            .collect(),
        function_starts: options.darwin.function_starts,
        data_in_code: options.darwin.data_in_code,
        stack_size: options.darwin.stack_size.unwrap_or(0),
        ..Commands::default()
    };
    for &index in &synthetic.loaded_dylibs {
        let Some(dylib) = collected.dylibs.get(index) else {
            continue;
        };
        commands.dylibs.push(DylibLoad {
            cmd: DylibLoad::command(
                dylib.mode,
                synthetic
                    .dylib_all_weak
                    .get(index)
                    .copied()
                    .unwrap_or(false),
            ),
            name: dylib.install_name.clone(),
            current_version: dylib.current_version.0,
            compatibility_version: dylib.compatibility_version.0,
        });
    }
    // Two-level namespace images set MH_NOUNDEFS even with flat lookups
    // (`-undefined dynamic_lookup`), as ld64 and lld do.
    commands.no_undefs = !config.flat_namespace;
    // `-init`: LC_ROUTINES_64, its address filled in once known.
    commands.init_address = options.init.as_ref().map(|_| 0);
    let (_, commands_size) = write::commands_size(&config, &layout, &commands);
    let header_size = 32u64
        .saturating_add(commands_size)
        .saturating_add(write::headerpad(&config, &commands));
    layout.assign_addresses(&link, header_size)?;
    fill_section_indices(&mut layout, &synthetic);
    let thunks = super::thunks::plan(&link, &mut layout, &synthetic, header_size)?;

    // `__unwind_info` was sized with an upper bound; its exact size is known
    // once the code has addresses (which it does not change: it follows all
    // code), so shrink it and lay out the rest again.
    let mut unwind_info = Vec::new();
    if unwind_plan.size() > 0 {
        for _ in 0..2 {
            unwind_info = super::unwind::build(
                &Addresses {
                    link: &link,
                    layout: &layout,
                    synthetic: &synthetic,
                    thunks: &thunks,
                },
                &unwind_plan,
            )?;
            let exact = to_u64(unwind_info.len());
            let Some(section) = layout
                .sections
                .iter_mut()
                .find(|s| s.kind == layout::SectionKind::UnwindInfo)
            else {
                break;
            };
            if section.size == exact {
                break;
            }
            section.size = exact;
            layout.assign_addresses(&link, header_size)?;
        }
    }

    let addresses = Addresses {
        link: &link,
        layout: &layout,
        synthetic: &synthetic,
        thunks: &thunks,
    };
    if let Some(id) = entry_id
        && let Some(crate::macho::reloc::Value::Address(address)) = addresses.symbol(id)
    {
        commands.entry_offset = address.saturating_sub(addresses.header_address());
    }
    if let Some(init) = &options.init {
        let address = link
            .symbols
            .lookup(&crate::symbols::SymbolName::new(init.as_bytes()))
            .and_then(|id| addresses.symbol(id));
        match address {
            Some(crate::macho::reloc::Value::Address(address)) => {
                commands.init_address = Some(address);
            }
            _ => {
                return Err(Error::Option(format!(
                    "-init: {init} is not defined in the output"
                )));
            }
        }
    }

    let linkedit_start = layout.segment(b"__LINKEDIT").map_or(0, |s| s.fileoff);
    let mut image = vec![0u8; to_usize(linkedit_start)];
    let pointer_fixups = sections::write(&addresses, &sectcreate_data, &mut image)?;
    super::unwind::write(&layout, &unwind_info, &mut image)?;
    super::eh_frame::write(&addresses, &eh_frame_plan, &mut image)?;

    // __LINKEDIT.
    let mut linkedit = Linkedit::default();
    let base = addresses.header_address();
    if config.chained_fixups {
        linkedit.chained_fixups = fixups::chained(
            &layout,
            base,
            config.page_size,
            &pointer_fixups,
            &synthetic.imports,
            &mut image,
        )?;
    } else {
        let weak_targets: Vec<Option<u64>> = synthetic
            .imports
            .iter()
            .map(|i| addresses.weak_definition(i.symbol))
            .collect();
        let opcodes = fixups::opcodes(
            &layout,
            &pointer_fixups,
            &synthetic.imports,
            &weak_targets,
            &mut image,
        )?;
        linkedit.rebase = opcodes.rebase;
        linkedit.bind = opcodes.bind;
        linkedit.weak_bind = opcodes.weak_bind;
    }
    let tables = symtab::build(&addresses, options, &filter, config.debug_map);
    linkedit.exports = trie::build(&tables.exports);
    super::buf::pad_to(&mut linkedit.exports, 8);
    if commands.function_starts {
        linkedit.function_starts = symtab::function_starts(&addresses);
    }
    if commands.data_in_code {
        linkedit.data_in_code = symtab::data_in_code(&addresses);
    }
    linkedit.symbols = tables.symbols;
    linkedit.symbol_counts = (tables.count, tables.locals, tables.extdefs, tables.undefs);
    linkedit.indirect = tables.indirect;
    linkedit.indirect_count = tables.indirect_count;
    linkedit.strings = tables.strings;
    if tables
        .exports
        .iter()
        .any(|e| e.flags & EXPORT_SYMBOL_FLAGS_WEAK_DEFINITION != 0)
    {
        commands.extra_flags |= MH_WEAK_DEFINES;
    }
    if synthetic.imports.iter().any(|i| {
        i.ordinal == crate::macho::read::consts::BIND_SPECIAL_DYLIB_WEAK_LOOKUP
            || matches!(
                link.defs.get(i.symbol.index()),
                Some(SymbolDef::Dylib { weak: true, .. })
            )
    }) {
        commands.extra_flags |= MH_BINDS_TO_WEAK;
    }

    let offsets = linkedit.offsets(linkedit_start, &config);
    let signature_size = if config.sign {
        codesign::signature_size(&config.identifier, offsets.signature)
    } else {
        0
    };
    let end = if config.sign {
        offsets.signature.saturating_add(signature_size)
    } else {
        offsets.signature
    };
    if let Some(segment) = layout.segments.iter_mut().find(|s| s.name == b"__LINKEDIT") {
        segment.filesize = end.saturating_sub(segment.fileoff);
        segment.vmsize = align_up(segment.filesize, config.page_size);
    }
    image.resize(to_usize(end), 0);
    linkedit.copy_into(&mut image, &offsets)?;
    let uuid_at = write::write_header(
        &HeaderInput {
            config: &config,
            layout: &layout,
            commands: &commands,
            linkedit: &linkedit,
            offsets: &offsets,
            signature_size,
        },
        &mut image,
    )?;
    if let Some(at) = uuid_at {
        let hashed = image.get(..to_usize(offsets.signature)).unwrap_or(&image);
        let mut uuid = Md5::digest(hashed);
        uuid[6] = (uuid[6] & 0x0f) | 0x30;
        uuid[8] = (uuid[8] & 0x3f) | 0x80;
        if let Some(slot) = image.get_mut(at..at.saturating_add(16)) {
            slot.copy_from_slice(&uuid);
        }
    }
    if config.sign {
        let text = layout.segment(b"__TEXT");
        codesign::write_signature(
            &mut image,
            &SignatureInput {
                identifier: &config.identifier,
                code_limit: offsets.signature,
                exec_seg_base: text.map_or(0, |s| s.fileoff),
                exec_seg_limit: text.map_or(0, |s| s.filesize),
                main_binary: config.is_exec(),
            },
        )?;
    }
    Ok(Attempt::Done(image))
}

/// Sets `reserved1` of the sections the indirect symbol table indexes.
fn fill_section_indices(layout: &mut Layout, synthetic: &scan::Synthetic) {
    let got = u32::try_from(synthetic.got_slots()).unwrap_or(0);
    let tlv = u32::try_from(synthetic.thread_ptrs.len()).unwrap_or(0);
    for section in &mut layout.sections {
        section.reserved1 = match section.kind {
            layout::SectionKind::Got => 0,
            layout::SectionKind::ThreadPtrs => got,
            layout::SectionKind::Stubs => got.saturating_add(tlv),
            _ => section.reserved1,
        };
    }
}
