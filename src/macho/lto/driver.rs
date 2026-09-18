//! The steps of [`prepare`](super::prepare): finding the bitcode, resolving
//! symbols with stand-ins, code generation through libLTO, and the inputs
//! that replace the bitcode.

#![deny(clippy::arithmetic_side_effects)]

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use hashbrown::{HashMap, HashSet};

use crate::args::LinkOptions;
use crate::args::darwin::{DarwinInputKind, LoadMode};
use crate::diag::{Collect, Diagnostic, DiagnosticSink, Severity};
use crate::error::{Error, Result};
use crate::ids::FileId;
use crate::input::FileTable;
use crate::input::archive::Archive;
use crate::macho::config::Config;
use crate::macho::inputs::{self, InputKind, InternalNames, SearchPaths};
use crate::macho::read::consts::{CPU_TYPE_ARM64, CPU_TYPE_X86_64};
use crate::macho::read::{Arch, FatFile, Source as MachSource};
use crate::macho::resolve::MachRules;
use crate::macho::symtab::ExportFilter;
use crate::plugin::liblto::{self, CodegenInput, CodegenSettings, ModuleInfo, Session};
use crate::symbols::{ResolveFile, SymbolTable, SymbolUse, resolve_symbols};

use super::Prepared;
use super::stub::{self, ArchiveMember, StubKind, StubSymbol};

// `lto_symbol_attributes`.
const DEFINITION_MASK: u32 = 0x0000_0700;
const DEFINITION_TENTATIVE: u32 = 0x0000_0200;
const DEFINITION_WEAK: u32 = 0x0000_0300;
const DEFINITION_UNDEFINED: u32 = 0x0000_0400;
const DEFINITION_WEAKUNDEF: u32 = 0x0000_0500;
const SCOPE_MASK: u32 = 0x0000_3800;
const SCOPE_INTERNAL: u32 = 0x0000_0800;
const SCOPE_HIDDEN: u32 = 0x0000_1000;
const SCOPE_PROTECTED: u32 = 0x0000_2000;
const SCOPE_DEFAULT: u32 = 0x0000_1800;

/// The name of the full-LTO object when `-object_path_lto` does not give
/// one: the name LLVM gives the merged module.
const FULL_LTO_NAME: &str = "ld-temp.o";

/// Which command-line list an input is in, and where.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Spec {
    /// `LinkOptions::inputs`.
    Gnu(usize),
    /// `DarwinArgs::inputs`.
    Darwin(usize),
}

/// A command-line input, found.
#[derive(Debug)]
struct Candidate {
    spec: Spec,
    path: PathBuf,
    /// `-force_load` (or `-all_load`).
    force: bool,
    /// `-hidden-l`.
    hidden: bool,
}

/// The inputs of `options` that name a file, in the order input collection
/// visits them. Inputs that cannot be found are left to input collection
/// to report.
fn candidates(options: &LinkOptions) -> Vec<Candidate> {
    let search = SearchPaths::new(options);
    let all_load = options.darwin.all_load;
    let mut out = Vec::new();
    for (index, spec) in options.inputs.iter().enumerate() {
        let path = match &spec.kind {
            crate::args::InputKind::File(path) => Some(path.clone()),
            crate::args::InputKind::Library(name) => search.find_library(name),
            _ => None,
        };
        if let Some(path) = path {
            out.push(Candidate {
                spec: Spec::Gnu(index),
                path,
                force: spec.attrs.whole_archive || all_load,
                hidden: false,
            });
        }
    }
    for (index, input) in options.darwin.inputs.iter().enumerate() {
        let path = match &input.kind {
            DarwinInputKind::File(path) => Some(path.clone()),
            DarwinInputKind::Library(name) => search.find_library(name),
            DarwinInputKind::Framework { name, suffix } => {
                search.find_framework(name, suffix.as_deref())
            }
        };
        if let Some(path) = path {
            out.push(Candidate {
                spec: Spec::Darwin(index),
                path,
                force: input.force_load || all_load,
                hidden: input.mode == LoadMode::Hidden,
            });
        }
    }
    out
}

/// Raw bitcode or a bitcode wrapper.
fn is_bitcode(data: &[u8]) -> bool {
    data.starts_with(b"BC\xc0\xde") || data.starts_with(&0x0b17_c0de_u32.to_le_bytes())
}

/// The architecture a target triple names, when it is one qld links.
fn triple_cpu(triple: &str) -> Option<u32> {
    match triple.split('-').next()? {
        "arm64" | "arm64e" | "aarch64" => Some(CPU_TYPE_ARM64),
        "x86_64" | "x86_64h" => Some(CPU_TYPE_X86_64),
        _ => None,
    }
}

/// A bitcode module found in the inputs.
#[derive(Debug)]
struct Module<'t> {
    /// `path` or `path(member)`.
    name: String,
    data: &'t [u8],
    info: ModuleInfo,
}

/// A member of an archive that holds bitcode.
#[derive(Debug)]
enum Piece<'t> {
    Native {
        name: &'t [u8],
        date: u64,
        data: &'t [u8],
    },
    Bitcode {
        name: &'t [u8],
        date: u64,
        module: usize,
    },
}

/// What replaces a command-line input that holds bitcode.
#[derive(Debug)]
enum Replacement<'t> {
    /// A bitcode file, or the bitcode slice of a universal file.
    Module(usize),
    /// An archive with bitcode members, with its members in order.
    Archive(Vec<Piece<'t>>),
    /// Bitcode for another architecture.
    Dropped,
}

#[derive(Debug)]
struct Replaced<'t> {
    candidate: usize,
    kind: Replacement<'t>,
}

/// The bytes of `data` for `arch`: the selected slice of a universal file.
fn slice_for<'t>(data: &'t [u8], path: &'t Path, arch: Arch) -> Option<&'t [u8]> {
    if !FatFile::is_fat(data) {
        return Some(data);
    }
    let fat = FatFile::parse(data, MachSource::new(path)).ok()?;
    fat.select(arch).ok().map(|slice| slice.data)
}

/// The modification time in an `ar` member header.
fn member_date(archive: &[u8], header_offset: u64) -> u64 {
    usize::try_from(header_offset)
        .ok()
        .and_then(|at| archive.get(at.checked_add(16)?..at.checked_add(28)?))
        .and_then(|field| std::str::from_utf8(field).ok())
        .and_then(|text| text.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

/// The inputs found to hold bitcode, before libLTO reads it.
struct Scan<'t> {
    replaced: Vec<Replaced<'t>>,
    /// Name and bytes of each module, in command-line order.
    modules: Vec<(String, &'t [u8])>,
}

/// Whether the file at `path` may hold bitcode (it is bitcode, a
/// universal file or an archive), from its first bytes. Reading only those
/// keeps links without bitcode from mapping their inputs twice. Unreadable
/// files are left to input collection to report.
fn may_hold_bitcode(path: &Path) -> bool {
    use std::io::Read;
    let mut magic = [0u8; 8];
    let read = std::fs::File::open(path).and_then(|mut file| file.read_exact(&mut magic));
    read.is_ok()
        && (is_bitcode(&magic)
            || magic.starts_with(crate::input::archive::MAGIC)
            || magic.starts_with(&[0xca, 0xfe, 0xba, 0xbe])
            || magic.starts_with(&[0xca, 0xfe, 0xba, 0xbf]))
}

/// Finds the bitcode in `candidates`, loading the files that may hold some
/// into `table`.
fn scan<'t>(candidates: &'t [Candidate], table: &'t FileTable, arch: Arch) -> Result<Scan<'t>> {
    let mut scan = Scan {
        replaced: Vec::new(),
        modules: Vec::new(),
    };
    for (index, candidate) in candidates.iter().enumerate() {
        if !may_hold_bitcode(&candidate.path) {
            continue;
        }
        let Some(file) = table
            .load_path(&candidate.path)
            .ok()
            .and_then(|id| table.get(id))
        else {
            continue;
        };
        let Some(data) = slice_for(file.data(), &candidate.path, arch) else {
            // Input collection reports universal files without the slice.
            continue;
        };
        if is_bitcode(data) {
            scan.modules
                .push((candidate.path.display().to_string(), data));
            scan.replaced.push(Replaced {
                candidate: index,
                kind: Replacement::Module(scan.modules.len().saturating_sub(1)),
            });
            continue;
        }
        if !data.starts_with(crate::input::archive::MAGIC) {
            continue;
        }
        let archive = Archive::parse(&candidate.path, data)?;
        let mut members = Vec::new();
        for member in archive.members() {
            members.push(member?);
        }
        if !members.iter().any(|m| m.bytes().is_some_and(is_bitcode)) {
            continue;
        }
        let mut pieces = Vec::with_capacity(members.len());
        for member in &members {
            let Some(bytes) = member.bytes() else {
                continue;
            };
            let date = member_date(data, member.header_offset);
            if is_bitcode(bytes) {
                scan.modules.push((
                    format!("{}({})", candidate.path.display(), member.display_name()),
                    bytes,
                ));
                pieces.push(Piece::Bitcode {
                    name: member.name,
                    date,
                    module: scan.modules.len().saturating_sub(1),
                });
            } else {
                pieces.push(Piece::Native {
                    name: member.name,
                    date,
                    data: bytes,
                });
            }
        }
        scan.replaced.push(Replaced {
            candidate: index,
            kind: Replacement::Archive(pieces),
        });
    }
    Ok(scan)
}

/// Whether a module built for `info` belongs in an `arch` link.
fn matches_arch(info: &ModuleInfo, arch: Arch) -> bool {
    match info.cpu {
        Some((cpu, _)) => cpu == arch.cpu_type,
        None => triple_cpu(&info.triple).is_none_or(|cpu| cpu == arch.cpu_type),
    }
}

/// The stand-in symbols of a module.
fn stub_symbols(info: &ModuleInfo) -> Vec<StubSymbol<'_>> {
    let mut out = Vec::with_capacity(info.symbols.len());
    for symbol in &info.symbols {
        if symbol.name.is_empty() {
            continue;
        }
        let scope = symbol.attributes & SCOPE_MASK;
        let kind = match symbol.attributes & DEFINITION_MASK {
            DEFINITION_UNDEFINED => StubKind::Undefined { weak: false },
            DEFINITION_WEAKUNDEF => StubKind::Undefined { weak: true },
            _ if scope == SCOPE_INTERNAL => continue,
            DEFINITION_TENTATIVE => StubKind::Common,
            definition => StubKind::Defined {
                weak: definition == DEFINITION_WEAK,
                hidden: scope == SCOPE_HIDDEN,
            },
        };
        out.push(StubSymbol {
            name: &symbol.name,
            kind,
        });
    }
    for name in &info.asm_undefined {
        if !name.is_empty() {
            out.push(StubSymbol {
                name,
                kind: StubKind::Undefined { weak: false },
            });
        }
    }
    out
}

/// The `LC_LINKER_OPTION` commands for a module's linker options
/// (`-lfoo`, `-framework Foo`, separated by spaces).
fn linker_options(options: &str) -> Vec<Vec<&[u8]>> {
    let mut out = Vec::new();
    let mut words = options.split_ascii_whitespace();
    while let Some(word) = words.next() {
        if word == "-framework" {
            if let Some(name) = words.next() {
                out.push(vec![word.as_bytes(), name.as_bytes()]);
            }
        } else if word.starts_with("-l") && word.len() > 2 {
            out.push(vec![word.as_bytes()]);
        }
    }
    out
}

fn stand_in(module: &Module<'_>, arch: Arch) -> Arc<[u8]> {
    let cpu = module.info.cpu.unwrap_or((arch.cpu_type, arch.cpu_subtype));
    Arc::from(stub::object(
        cpu,
        &stub_symbols(&module.info),
        &linker_options(&module.info.linker_options),
        module.info.objc_category,
    ))
}

/// The inputs that replace the bitcode inputs, for resolution (with
/// stand-ins) or for the final link (without).
#[derive(Default)]
struct Generated {
    inputs: Vec<(PathBuf, Arc<[u8]>)>,
    /// The address of each stand-in's bytes, to its module.
    stand_ins: HashMap<usize, usize>,
}

fn member_path(archive: &Path, member: &[u8]) -> PathBuf {
    PathBuf::from(format!(
        "{}({})",
        archive.display(),
        String::from_utf8_lossy(member)
    ))
}

/// Builds the replacement inputs. `stand_ins` holds each live module's
/// stand-in, or `None` for the final link.
fn generate(
    replaced: &[Replaced<'_>],
    candidates: &[Candidate],
    stand_ins: Option<&[Option<Arc<[u8]>>]>,
) -> Generated {
    let mut out = Generated::default();
    let stand_in = |module: usize| stand_ins.and_then(|s| s.get(module)?.clone());
    for item in replaced {
        let Some(candidate) = candidates.get(item.candidate) else {
            continue;
        };
        match &item.kind {
            Replacement::Module(module) => {
                if let Some(bytes) = stand_in(*module) {
                    out.stand_ins.insert(bytes.as_ptr() as usize, *module);
                    out.inputs.push((candidate.path.clone(), bytes));
                }
            }
            Replacement::Dropped => {}
            Replacement::Archive(pieces) if candidate.force => {
                for piece in pieces {
                    match piece {
                        Piece::Native { name, data, .. } => out
                            .inputs
                            .push((member_path(&candidate.path, name), Arc::from(*data))),
                        Piece::Bitcode { name, module, .. } => {
                            if let Some(bytes) = stand_in(*module) {
                                out.stand_ins.insert(bytes.as_ptr() as usize, *module);
                                out.inputs.push((member_path(&candidate.path, name), bytes));
                            }
                        }
                    }
                }
            }
            Replacement::Archive(pieces) => {
                let stand_in_of: Vec<Option<Arc<[u8]>>> = pieces
                    .iter()
                    .map(|piece| match piece {
                        Piece::Bitcode { module, .. } => stand_in(*module),
                        Piece::Native { .. } => None,
                    })
                    .collect();
                let mut archive_members = Vec::with_capacity(pieces.len());
                let mut modules = Vec::with_capacity(pieces.len());
                for (piece, bytes) in pieces.iter().zip(&stand_in_of) {
                    match (piece, bytes) {
                        (Piece::Native { name, date, data }, _) => {
                            archive_members.push(ArchiveMember {
                                name,
                                date: *date,
                                data,
                            });
                            modules.push(None);
                        }
                        (Piece::Bitcode { name, date, module }, Some(bytes)) => {
                            archive_members.push(ArchiveMember {
                                name,
                                date: *date,
                                data: bytes,
                            });
                            modules.push(Some(*module));
                        }
                        (Piece::Bitcode { .. }, None) => {}
                    }
                }
                if archive_members.is_empty() {
                    continue;
                }
                let (bytes, offsets) = stub::archive(&archive_members);
                let bytes: Arc<[u8]> = Arc::from(bytes);
                let base = bytes.as_ptr() as usize;
                for (module, offset) in modules.iter().zip(&offsets) {
                    if let Some(module) = module {
                        out.stand_ins.insert(base.wrapping_add(*offset), *module);
                    }
                }
                out.inputs.push((candidate.path.clone(), bytes));
            }
        }
    }
    out
}

/// What symbol resolution with stand-ins decided.
#[derive(Debug, Default)]
struct Decision {
    /// The modules in the link, in command-line order.
    included: Vec<usize>,
    /// Names that live native objects and the linker use or define.
    native_names: HashSet<Vec<u8>>,
}

/// Resolves symbols over `options` with the stand-ins in `generated`.
fn resolve(
    options: &LinkOptions,
    arch: Arch,
    generated: &Generated,
    module_count: usize,
) -> Result<Decision> {
    let mut options = Cow::Borrowed(options);
    // Warnings are left to the real link.
    let quiet = Collect::new();
    for _ in 0..8 {
        let config = Config::new(&options, arch, None)?;
        let table = FileTable::new();
        let collected = inputs::collect(&options, &config, &table, &quiet, &generated.inputs)?;
        let internal = InternalNames::new(&options, &config);
        let mut files = collected.files(&internal)?;
        let mut symbols = SymbolTable::new();
        let resolution = resolve_symbols(&mut symbols, &MachRules, &mut files)?;
        let live = |index: usize| resolution.is_live(FileId::new(index));
        if !config.is_relocatable() {
            let more = inputs::missing_linker_options(&options, &collected, &files, live);
            if !more.is_empty() {
                options.to_mut().darwin.inputs.extend(more);
                continue;
            }
        }
        let mut decision = Decision::default();
        let mut included = vec![false; module_count];
        for (index, file) in files.iter().enumerate() {
            if !live(index) {
                continue;
            }
            match file.kind {
                InputKind::Dylib(_) => continue,
                InputKind::Internal => {}
                InputKind::Object => {
                    let address = file.file.map_or(0, |f| f.data().as_ptr() as usize);
                    if let Some(&module) = generated.stand_ins.get(&address) {
                        if let Some(slot) = included.get_mut(module) {
                            *slot = true;
                        }
                        continue;
                    }
                }
            }
            for (symbol, name) in file.symbol_names().iter().enumerate() {
                if file.symbol_use(symbol) != SymbolUse::Ignore {
                    decision.native_names.insert(name.bytes().to_vec());
                }
            }
        }
        decision.included = included
            .iter()
            .enumerate()
            .filter_map(|(module, &live)| live.then_some(module))
            .collect();
        return Ok(decision);
    }
    Err(Error::Internal(
        "LTO: LC_LINKER_OPTION requests did not settle".into(),
    ))
}

/// The symbols libLTO must keep, and (for ThinLTO) those one module
/// references in another, in a deterministic order.
#[derive(Debug, Default)]
struct Preserved {
    preserve: Vec<Vec<u8>>,
    cross_referenced: Vec<Vec<u8>>,
}

/// Computes [`Preserved`] for the modules in `decision`.
fn preserved(
    options: &LinkOptions,
    modules: &[Module<'_>],
    decision: &Decision,
) -> Result<Preserved> {
    let darwin = &options.darwin;
    let relocatable = darwin.output_type == crate::args::darwin::MachOutputType::Object;
    let is_exec = darwin.output_type == crate::args::darwin::MachOutputType::Execute;
    let has_export_list = !darwin.exported_symbols.is_empty()
        || !darwin.exported_symbols_lists.is_empty()
        || !darwin.unexported_symbols.is_empty()
        || !darwin.unexported_symbols_lists.is_empty()
        || darwin.no_exported_symbols;
    let keep_exported = !is_exec || darwin.lto.export_dynamic || has_export_list;
    let filter = ExportFilter::new(options)?;
    let mut roots: HashSet<&[u8]> = HashSet::new();
    if let Some(init) = &options.init {
        roots.insert(init.as_bytes());
    }

    let mut preserve: Vec<Vec<u8>> = Vec::new();
    let mut seen: HashSet<&[u8]> = HashSet::new();
    let mut defined_by: HashMap<&[u8], usize> = HashMap::new();
    for &index in &decision.included {
        let Some(module) = modules.get(index) else {
            continue;
        };
        for symbol in &module.info.symbols {
            let definition = symbol.attributes & DEFINITION_MASK;
            let scope = symbol.attributes & SCOPE_MASK;
            if matches!(definition, DEFINITION_UNDEFINED | DEFINITION_WEAKUNDEF)
                || scope == SCOPE_INTERNAL
                || symbol.name.is_empty()
            {
                continue;
            }
            defined_by.entry(&symbol.name).or_insert(index);
            let name = symbol.name.as_slice();
            let exported = keep_exported
                && matches!(scope, SCOPE_DEFAULT | SCOPE_PROTECTED)
                && filter.exports(name);
            let keep = relocatable
                || exported
                || roots.contains(name)
                || decision.native_names.contains(name);
            if keep && seen.insert(name) {
                preserve.push(name.to_vec());
            }
        }
    }
    let mut cross: Vec<Vec<u8>> = Vec::new();
    let mut cross_seen: HashSet<&[u8]> = HashSet::new();
    for &index in &decision.included {
        let Some(module) = modules.get(index) else {
            continue;
        };
        let undefined = module
            .info
            .symbols
            .iter()
            .filter(|s| {
                matches!(
                    s.attributes & DEFINITION_MASK,
                    DEFINITION_UNDEFINED | DEFINITION_WEAKUNDEF
                )
            })
            .map(|s| s.name.as_slice())
            .chain(module.info.asm_undefined.iter().map(Vec::as_slice));
        for name in undefined {
            if defined_by.get(name).is_some_and(|&by| by != index) && cross_seen.insert(name) {
                cross.push(name.to_vec());
            }
        }
    }
    Ok(Preserved {
        preserve,
        cross_referenced: cross,
    })
}

/// `path`, made specific to `arch` when the link has several.
fn per_arch(options: &LinkOptions, path: &Path, arch: Arch) -> PathBuf {
    if options.darwin.archs.len() <= 1 {
        return path.to_path_buf();
    }
    let mut out = path.as_os_str().to_os_string();
    out.push(".");
    out.push(arch.name().unwrap_or("unknown"));
    PathBuf::from(out)
}

fn emit_messages(messages: Vec<(Severity, String)>, diagnostics: &dyn DiagnosticSink) {
    for (severity, text) in messages {
        diagnostics.emit(Diagnostic::new(severity, text));
    }
}

/// Runs code generation for the modules in `decision`; returns the objects
/// with their names.
fn codegen(
    session: &mut Session,
    options: &LinkOptions,
    arch: Arch,
    modules: &[Module<'_>],
    decision: &Decision,
    diagnostics: &dyn DiagnosticSink,
) -> Result<Vec<(PathBuf, Arc<[u8]>)>> {
    let included: Vec<&Module<'_>> = decision
        .included
        .iter()
        .filter_map(|&index| modules.get(index))
        .collect();
    if included.is_empty() {
        return Ok(Vec::new());
    }
    let preserved = preserved(options, modules, decision)?;
    let preserve: Vec<&[u8]> = preserved.preserve.iter().map(Vec::as_slice).collect();
    let lto = &options.darwin.lto;
    let relocatable = options.darwin.output_type == crate::args::darwin::MachOutputType::Object;
    let thin = included.iter().all(|m| m.info.thin);
    let object_path = lto
        .object_path
        .as_deref()
        .map(|p| per_arch(options, p, arch));

    // ThinLTO identifiers must be unique.
    let mut names: Vec<String> = Vec::with_capacity(included.len());
    let mut used: HashSet<String> = HashSet::new();
    for module in &included {
        let mut name = module.name.clone();
        let mut suffix = 1u32;
        while !used.insert(name.clone()) {
            suffix = suffix.saturating_add(1);
            name = format!("{}#{suffix}", module.name);
        }
        names.push(name);
    }
    let inputs: Vec<CodegenInput<'_>> = included
        .iter()
        .zip(&names)
        .map(|(module, name)| CodegenInput {
            name,
            data: module.data,
        })
        .collect();
    let settings = CodegenSettings {
        cpu: lto.mcpu.clone(),
        llvm_options: lto.mllvm.clone(),
        internalize: !relocatable,
        codegen_only: lto.codegen_only,
        cache_dir: lto.cache_path.clone(),
        prune_interval: lto.prune_interval,
        prune_after: lto.prune_after,
        max_relative_cache_size: lto.max_relative_cache_size,
        objects_dir: if thin { object_path.clone() } else { None },
    };

    let mut out = Vec::new();
    if thin {
        let cross: Vec<&[u8]> = preserved
            .cross_referenced
            .iter()
            .map(Vec::as_slice)
            .collect();
        let output = session.compile_thin(&inputs, &preserve, &cross, &settings)?;
        emit_messages(output.messages, diagnostics);
        for (object, name) in output.objects.into_iter().zip(&names) {
            if object.data.is_empty() {
                continue;
            }
            let path = object
                .path
                .unwrap_or_else(|| PathBuf::from(format!("{name}.lto.o")));
            out.push((path, Arc::from(object.data)));
        }
    } else {
        let output = session.compile_full(&inputs, &preserve, &settings)?;
        emit_messages(output.messages, diagnostics);
        for object in output.objects {
            let path = match &object_path {
                Some(path) if path.is_dir() => {
                    path.join(format!("0.{}.lto.o", arch.name().unwrap_or("unknown")))
                }
                Some(path) => path.clone(),
                None => PathBuf::from(FULL_LTO_NAME),
            };
            if object_path.is_some() {
                std::fs::write(&path, &object.data).map_err(|error| Error::io(&path, error))?;
            }
            out.push((path, Arc::from(object.data)));
        }
    }
    Ok(out)
}

/// See [`prepare`](super::prepare).
pub(super) fn prepare<'o>(
    options: &'o LinkOptions,
    arch: Arch,
    diagnostics: &dyn DiagnosticSink,
) -> Result<Prepared<'o>> {
    let candidates = candidates(options);
    let table = FileTable::new();
    let Scan {
        mut replaced,
        modules: found,
    } = scan(&candidates, &table, arch)?;
    if replaced.is_empty() {
        return Ok(Prepared {
            options: Cow::Borrowed(options),
            inputs: Vec::new(),
        });
    }

    // The libLTO lock is never held across parallel work: a rayon worker
    // waiting in it could pick up another architecture's link, which would
    // take the lock again.
    let lib = liblto::find(options.darwin.lto_library.as_deref())?;
    let mut modules = Vec::with_capacity(found.len());
    {
        let session = lib.lock();
        for (name, data) in found {
            let info = session.read_module(data, &name)?;
            modules.push(Module { name, data, info });
        }
    }

    // Bitcode for another architecture: dropped with a warning for files,
    // skipped silently for archive members, as for native objects.
    let wrong_arch: Vec<bool> = modules
        .iter()
        .map(|m| !matches_arch(&m.info, arch))
        .collect();
    for item in &mut replaced {
        match &mut item.kind {
            Replacement::Module(module) => {
                if wrong_arch.get(*module).copied().unwrap_or(false) {
                    if let Some(module) = modules.get(*module) {
                        diagnostics.emit(Diagnostic::warning(format!(
                            "ignoring file {}, built for {} (linking for {arch})",
                            module.name, module.info.triple
                        )));
                    }
                    item.kind = Replacement::Dropped;
                }
            }
            Replacement::Archive(pieces) => pieces.retain(|piece| match piece {
                Piece::Bitcode { module, .. } => !wrong_arch.get(*module).copied().unwrap_or(false),
                Piece::Native { .. } => true,
            }),
            Replacement::Dropped => {}
        }
    }
    for item in &replaced {
        if let Some(candidate) = candidates.get(item.candidate)
            && candidate.hidden
            && matches!(item.kind, Replacement::Archive(_))
        {
            diagnostics.emit(Diagnostic::warning(format!(
                "{}: -hidden-l is not applied to archives with bitcode members",
                candidate.path.display()
            )));
        }
    }

    // The options without the replaced inputs.
    let mut without = options.clone();
    let mut gnu = HashSet::new();
    let mut darwin = HashSet::new();
    for item in &replaced {
        match candidates.get(item.candidate).map(|c| c.spec) {
            Some(Spec::Gnu(index)) => {
                gnu.insert(index);
            }
            Some(Spec::Darwin(index)) => {
                darwin.insert(index);
            }
            None => {}
        }
    }
    let mut index = 0usize;
    without.inputs.retain(|_| {
        let keep = !gnu.contains(&index);
        index = index.saturating_add(1);
        keep
    });
    let mut index = 0usize;
    without.darwin.inputs.retain(|_| {
        let keep = !darwin.contains(&index);
        index = index.saturating_add(1);
        keep
    });

    // Resolution with stand-ins.
    let stand_ins: Vec<Option<Arc<[u8]>>> = modules
        .iter()
        .zip(&wrong_arch)
        .map(|(module, &wrong)| (!wrong).then(|| stand_in(module, arch)))
        .collect();
    let decision = {
        let generated = generate(&replaced, &candidates, Some(&stand_ins));
        resolve(&without, arch, &generated, modules.len())?
    };

    let mut inputs = codegen(
        &mut lib.lock(),
        options,
        arch,
        &modules,
        &decision,
        diagnostics,
    )?;
    inputs.extend(generate(&replaced, &candidates, None).inputs);
    Ok(Prepared {
        options: Cow::Owned(without),
        inputs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitcode_magic() {
        assert!(is_bitcode(b"BC\xc0\xde\x35\x14"));
        assert!(is_bitcode(&[0xde, 0xc0, 0x17, 0x0b, 0, 0, 0, 0]));
        assert!(!is_bitcode(b"\xcf\xfa\xed\xfe"));
        assert!(!is_bitcode(b"BC"));
    }

    #[test]
    fn triples() {
        assert_eq!(triple_cpu("arm64-apple-macosx13.0.0"), Some(CPU_TYPE_ARM64));
        assert_eq!(
            triple_cpu("x86_64-apple-macosx10.15"),
            Some(CPU_TYPE_X86_64)
        );
        assert_eq!(triple_cpu("riscv64-unknown-elf"), None);
    }

    #[test]
    fn autolink_options() {
        let options = linker_options(" -lz -framework Foundation -lc++ -weird");
        assert_eq!(
            options,
            [
                vec![b"-lz".as_slice()],
                vec![b"-framework".as_slice(), b"Foundation"],
                vec![b"-lc++".as_slice()],
            ]
        );
        assert!(linker_options(" -framework").is_empty());
    }

    #[test]
    fn stand_in_symbols_follow_attributes() {
        let symbol = |name: &[u8], attributes| liblto::ModuleSymbol {
            name: name.to_vec(),
            attributes,
        };
        let info = ModuleInfo {
            symbols: vec![
                symbol(b"_main", 0x100 | SCOPE_DEFAULT),
                symbol(b"_local", 0x100 | SCOPE_INTERNAL),
                symbol(b"_hidden", DEFINITION_WEAK | SCOPE_HIDDEN),
                symbol(b"_common", DEFINITION_TENTATIVE | SCOPE_DEFAULT),
                symbol(b"_puts", DEFINITION_UNDEFINED),
                symbol(b"_weak", DEFINITION_WEAKUNDEF),
            ],
            asm_undefined: vec![b"_from_asm".to_vec()],
            ..ModuleInfo::default()
        };
        let symbols = stub_symbols(&info);
        let kinds: Vec<(&[u8], StubKind)> = symbols.iter().map(|s| (s.name, s.kind)).collect();
        assert_eq!(
            kinds,
            [
                (
                    b"_main".as_slice(),
                    StubKind::Defined {
                        weak: false,
                        hidden: false
                    }
                ),
                (
                    b"_hidden",
                    StubKind::Defined {
                        weak: true,
                        hidden: true
                    }
                ),
                (b"_common", StubKind::Common),
                (b"_puts", StubKind::Undefined { weak: false }),
                (b"_weak", StubKind::Undefined { weak: true }),
                (b"_from_asm", StubKind::Undefined { weak: false }),
            ]
        );
    }
}
