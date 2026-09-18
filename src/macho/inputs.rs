//! Input resolution for a Mach-O link: search paths, libraries and
//! frameworks, object files, archives, dylibs and text stubs, turned into
//! the file list symbol resolution works on.
//!
//! Collection happens in two phases so that names can be borrowed for the
//! whole link:
//!
//! 1. [`collect`] finds and maps every file, selects universal slices,
//!    expands archives into members, and reads dylibs and `.tbd` stubs
//!    (following their re-exported libraries) into owned [`LoadedDylib`]s.
//! 2. [`Collected::files`] builds the [`MachInput`]s, which borrow symbol
//!    names from the mappings and from the [`Collected`] value.
//!
//! File 0 is the linker's internal file: the entry point, `-u` symbols and
//! the header symbols the linker defines.

#![deny(clippy::arithmetic_side_effects)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use hashbrown::HashSet;

use crate::args::LinkOptions;
use crate::args::darwin::{DarwinInput, DarwinInputKind, LoadMode, MachOutputType};
use crate::diag::{Diagnostic, DiagnosticSink};
use crate::error::{Error, Result};
use crate::ids::FileId;
use crate::input::identify::{FileFormat, TextKind};
use crate::input::source::InputProvider;
use crate::input::{FileTable, InputFile, Source};
use crate::macho::read::commands::PackedVersion;
use crate::macho::read::tbd::{StubSymbolKind, StubTarget, TextStub};
use crate::macho::read::{
    Arch, Dylib, FatFile, LinkerOptionHint, ObjectFile, Source as MachSource,
};
use crate::symbols::{DefinitionKind, InputPosition, ResolveFile, SymbolName, SymbolUse};

use super::config::Config;
use super::object::LinkObject;

/// A symbol a dylib exports.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct DylibExport {
    /// The name.
    pub name: Vec<u8>,
    /// A weak definition.
    pub weak: bool,
    /// A thread-local variable.
    pub tlv: bool,
}

/// A dylib (or text stub) linked against.
#[derive(Clone, Debug)]
pub struct LoadedDylib {
    /// Where it was found.
    pub path: PathBuf,
    /// Install name (`LC_ID_DYLIB`).
    pub install_name: Vec<u8>,
    /// Current version.
    pub current_version: PackedVersion,
    /// Compatibility version.
    pub compatibility_version: PackedVersion,
    /// How it is linked.
    pub mode: LoadMode,
    /// Exported symbols, including those of re-exported libraries, sorted by
    /// name.
    pub exports: Vec<DylibExport>,
    /// Linked implicitly, as a public re-export of another library: its load
    /// command is written only if the output uses it.
    pub implicit: bool,
    /// The `-bundle_loader` executable: its exports resolve the bundle's
    /// references, bound with the main-executable ordinal.
    pub bundle_loader: bool,
}

/// What an input is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputKind {
    /// The linker's internal file.
    Internal,
    /// An object file or archive member.
    Object,
    /// A dylib: index into [`Collected::dylibs`].
    Dylib(usize),
}

/// One input as symbol resolution sees it.
#[derive(Debug)]
pub struct MachInput<'a> {
    /// Command-line position.
    pub position: InputPosition,
    /// What it is.
    pub kind: InputKind,
    /// The mapped file (objects and members).
    pub file: Option<&'a InputFile>,
    /// Live from the start.
    pub live_at_start: bool,
    /// Names a lazy member would define.
    pub lazy_names: Vec<SymbolName<'a>>,
    /// The parsed object, once loaded.
    pub object: Option<Box<LinkObject<'a>>>,
    /// Symbols of internal and dylib inputs.
    pub names: Vec<SymbolName<'a>>,
    /// Their uses.
    pub uses: Vec<SymbolUse>,
    /// Members of a `-hidden-l` archive: their globals become private.
    pub hidden: bool,
    /// Modification time of the object (or archive member), for the debug
    /// map.
    pub mtime: u64,
}

impl<'a> MachInput<'a> {
    /// `path` or `path(member)`, for diagnostics.
    #[must_use]
    pub fn display(&self) -> String {
        match (self.kind, self.file) {
            (InputKind::Internal, _) => "<internal>".into(),
            (_, Some(file)) => match file.member() {
                Some(member) => format!("{}({member})", file.path().display()),
                None => file.path().display().to_string(),
            },
            _ => "<dylib>".into(),
        }
    }
}

impl<'a> ResolveFile<'a> for MachInput<'a> {
    fn position(&self) -> InputPosition {
        self.position
    }

    fn is_live_at_start(&self) -> bool {
        self.live_at_start
    }

    fn lazy_names(&self) -> &[SymbolName<'a>] {
        &self.lazy_names
    }

    fn load(&mut self) -> Result<()> {
        if self.kind != InputKind::Object || self.object.is_some() {
            return Ok(());
        }
        let Some(file) = self.file else {
            return Ok(());
        };
        let object = ObjectFile::parse(file.data(), source_of(file))?;
        self.object = Some(Box::new(LinkObject::new(object)?));
        Ok(())
    }

    fn symbol_names(&self) -> &[SymbolName<'a>] {
        match &self.object {
            Some(object) => &object.names,
            None => &self.names,
        }
    }

    fn symbol_use(&self, index: usize) -> SymbolUse {
        let uses = match &self.object {
            Some(object) => &object.uses,
            None => &self.uses,
        };
        uses.get(index).copied().unwrap_or(SymbolUse::Ignore)
    }
}

/// The reader's error context for `file`.
#[must_use]
pub fn source_of(file: &InputFile) -> MachSource<'_> {
    match file.member() {
        Some(member) => MachSource::member(file.path(), member),
        None => MachSource::new(file.path()),
    }
}

/// An object or member found by [`collect`].
#[derive(Debug)]
struct ObjectEntry {
    id: FileId,
    position: InputPosition,
    live: bool,
    hidden: bool,
    mtime: u64,
}

enum Entry {
    Object(ObjectEntry),
    Dylib(usize, InputPosition),
}

/// The linker-defined and linker-referenced names.
#[derive(Debug, Default)]
pub struct InternalNames {
    /// Name and use of each.
    pub names: Vec<(Vec<u8>, SymbolUse)>,
    /// `-alias` definitions among `names`: (alias, target).
    pub aliases: Vec<(Vec<u8>, Vec<u8>)>,
}

impl InternalNames {
    /// The internal symbols of a link with `config`.
    #[must_use]
    pub fn new(options: &LinkOptions, config: &Config) -> Self {
        let reference = SymbolUse::Reference { weak: false };
        let definition = SymbolUse::Definition {
            kind: DefinitionKind::Weak,
            aux: 0,
        };
        let mut names = Vec::new();
        if let Some(entry) = &config.entry {
            names.push((entry.clone(), reference));
        }
        for name in options.undefined.iter().chain(&options.require_defined) {
            names.push((name.as_bytes().to_vec(), reference));
        }
        // `-alias target alias`: the alias is defined here and takes its
        // target's definition once resolution is done.
        let aliases: Vec<(Vec<u8>, Vec<u8>)> = options
            .darwin
            .aliases
            .iter()
            .map(|(target, alias)| (alias.as_bytes().to_vec(), target.as_bytes().to_vec()))
            .collect();
        for (alias, target) in &aliases {
            names.push((target.clone(), reference));
            names.push((
                alias.clone(),
                SymbolUse::Definition {
                    kind: DefinitionKind::Regular,
                    aux: 0,
                },
            ));
        }
        let header = match config.output_type {
            MachOutputType::Execute => b"__mh_execute_header".as_slice(),
            MachOutputType::Dylib => b"__mh_dylib_header",
            MachOutputType::Bundle => b"__mh_bundle_header",
            // A relocatable object has no header of its own to point at:
            // references stay undefined for the final link.
            MachOutputType::Object => return Self { names, aliases },
        };
        names.push((header.to_vec(), definition));
        names.push((b"___dso_handle".to_vec(), definition));
        Self { names, aliases }
    }
}

/// Everything [`collect`] found.
pub struct Collected<'t> {
    table: &'t FileTable,
    entries: Vec<Entry>,
    /// The dylibs, in command-line order (their ordinals, before
    /// `-dead_strip_dylibs`).
    pub dylibs: Vec<LoadedDylib>,
    /// The build version of the first object, for links without
    /// `-platform_version`.
    pub first_platform: Option<(u32, PackedVersion, PackedVersion)>,
    /// Every library and framework requested, on the command line or by
    /// `LC_LINKER_OPTION`.
    pub requested: Vec<DarwinInput>,
}

impl std::fmt::Debug for Collected<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Collected")
            .field("entries", &self.entries.len())
            .field("dylibs", &self.dylibs.len())
            .finish_non_exhaustive()
    }
}

impl<'t> Collected<'t> {
    /// Builds the resolution file list. File 0 is the internal file.
    ///
    /// # Errors
    ///
    /// Malformed archive members.
    pub fn files<'a>(&'a self, internal: &'a InternalNames) -> Result<Vec<MachInput<'a>>>
    where
        't: 'a,
    {
        let mut files = Vec::with_capacity(self.entries.len().saturating_add(1));
        let mut internal_input = MachInput {
            position: InputPosition::new(0, 0),
            kind: InputKind::Internal,
            file: None,
            live_at_start: true,
            lazy_names: Vec::new(),
            object: None,
            names: Vec::new(),
            uses: Vec::new(),
            hidden: false,
            mtime: 0,
        };
        for (name, use_) in &internal.names {
            internal_input.names.push(SymbolName::new(name));
            internal_input.uses.push(*use_);
        }
        files.push(internal_input);
        for entry in &self.entries {
            match entry {
                Entry::Object(object) => {
                    let Some(file) = self.table.get(object.id) else {
                        return Err(Error::Internal("input file missing from table".into()));
                    };
                    let lazy_names = if object.live {
                        Vec::new()
                    } else {
                        let parsed = ObjectFile::parse(file.data(), source_of(file))?;
                        LinkObject::defined_names(&parsed)?
                    };
                    files.push(MachInput {
                        position: object.position,
                        kind: InputKind::Object,
                        file: Some(file),
                        live_at_start: object.live,
                        lazy_names,
                        object: None,
                        names: Vec::new(),
                        uses: Vec::new(),
                        hidden: object.hidden,
                        mtime: object.mtime,
                    });
                }
                Entry::Dylib(index, position) => {
                    let Some(dylib) = self.dylibs.get(*index) else {
                        continue;
                    };
                    let mut input = MachInput {
                        position: *position,
                        kind: InputKind::Dylib(*index),
                        file: None,
                        live_at_start: true,
                        lazy_names: Vec::new(),
                        object: None,
                        names: Vec::with_capacity(dylib.exports.len()),
                        uses: Vec::with_capacity(dylib.exports.len()),
                        hidden: false,
                        mtime: 0,
                    };
                    for export in &dylib.exports {
                        input.names.push(SymbolName::new(&export.name));
                        input.uses.push(SymbolUse::Definition {
                            kind: DefinitionKind::Shared,
                            aux: u64::from(export.weak),
                        });
                    }
                    files.push(input);
                }
            }
        }
        Ok(files)
    }
}

/// The library and framework search directories.
#[derive(Clone, Debug, Default)]
pub struct SearchPaths {
    /// Library directories, in search order.
    pub libraries: Vec<PathBuf>,
    /// Framework directories, in search order.
    pub frameworks: Vec<PathBuf>,
    /// The `-syslibroot` directories (an empty path when there are none).
    pub roots: Vec<PathBuf>,
    /// `-search_dylibs_first`.
    pub dylibs_first: bool,
    /// [`LinkOptions::input_provider`]: files it holds exist too.
    pub provider: Option<Arc<dyn InputProvider>>,
}

fn join_root(root: &Path, path: &Path) -> PathBuf {
    if root.as_os_str().is_empty() {
        return path.to_path_buf();
    }
    let relative = path.strip_prefix("/").unwrap_or(path);
    root.join(relative)
}

impl SearchPaths {
    /// The search directories of `options`, as ld64 and lld compute them:
    /// each absolute `-L` and `-F` directory under every root where it
    /// exists (or as given when it exists under none), relative ones as
    /// given, then the default directories under each root unless `-Z`.
    #[must_use]
    pub fn new(options: &LinkOptions) -> Self {
        let darwin = &options.darwin;
        let mut roots: Vec<PathBuf> = darwin.syslibroots.clone();
        if roots.last().is_some_and(|r| r.as_os_str() == "/") {
            roots.clear();
        }
        if roots.is_empty() {
            roots.push(PathBuf::new());
        }
        let expand = |given: &[PathBuf], defaults: &[&str]| {
            let mut out = Vec::new();
            for path in given {
                let mut found = false;
                for root in &roots {
                    // `-L.` is the current directory, not the SDK's root.
                    if root.as_os_str().is_empty() || !path.has_root() {
                        continue;
                    }
                    let candidate = join_root(root, path);
                    if candidate.is_dir() {
                        out.push(candidate);
                        found = true;
                    }
                }
                if !found {
                    out.push(path.clone());
                }
            }
            if !darwin.no_default_search_paths {
                for default in defaults {
                    for root in &roots {
                        let candidate = join_root(root, Path::new(default));
                        if candidate.is_dir() {
                            out.push(candidate);
                        }
                    }
                }
            }
            out
        };
        Self {
            libraries: expand(&options.search_paths, &["/usr/lib", "/usr/local/lib"]),
            frameworks: expand(
                &darwin.framework_paths,
                &["/Library/Frameworks", "/System/Library/Frameworks"],
            ),
            roots,
            dylibs_first: darwin.search_dylibs_first,
            provider: options.input_provider.clone(),
        }
    }

    /// Finds `-l<name>`.
    #[must_use]
    pub fn find_library(&self, name: &str) -> Option<PathBuf> {
        if self.dylibs_first {
            let dynamic = [".tbd", ".dylib", ".so"];
            return self
                .find_in(&self.libraries, &format!("lib{name}"), &dynamic)
                .or_else(|| self.find_in(&self.libraries, &format!("lib{name}"), &[".a"]));
        }
        self.find_in(
            &self.libraries,
            &format!("lib{name}"),
            &[".tbd", ".dylib", ".so", ".a"],
        )
    }

    /// Finds `-framework <name>[,<suffix>]`.
    #[must_use]
    pub fn find_framework(&self, name: &str, suffix: Option<&str>) -> Option<PathBuf> {
        for dir in &self.frameworks {
            let base = dir.join(format!("{name}.framework"));
            let mut candidates = Vec::new();
            if let Some(suffix) = suffix {
                candidates.push(base.join(format!("{name}{suffix}")));
                candidates.push(base.join(format!("{name}{suffix}.tbd")));
            }
            candidates.push(base.join(name));
            candidates.push(base.join(format!("{name}.tbd")));
            if let Some(found) = candidates.into_iter().find(|c| self.exists(c)) {
                return Some(found);
            }
        }
        None
    }

    /// Finds a re-exported library by install name under the roots, trying
    /// the `.tbd` spelling first as lld does.
    #[must_use]
    pub fn find_install_name(&self, install_name: &[u8]) -> Option<PathBuf> {
        let name = std::str::from_utf8(install_name).ok()?;
        if name.starts_with('@') {
            return None;
        }
        let path = Path::new(name);
        for root in &self.roots {
            let candidate = join_root(root, path);
            let tbd = match candidate.extension() {
                Some(ext) if ext == "dylib" => candidate.with_extension("tbd"),
                _ => {
                    let mut with = candidate.clone().into_os_string();
                    with.push(".tbd");
                    PathBuf::from(with)
                }
            };
            if self.exists(&tbd) {
                return Some(tbd);
            }
            if self.exists(&candidate) {
                return Some(candidate);
            }
        }
        None
    }
}

impl SearchPaths {
    /// Whether `path` is a file, in the input provider or on disk.
    fn exists(&self, path: &Path) -> bool {
        self.provider.as_ref().is_some_and(|p| p.contains(path)) || path.is_file()
    }

    fn find_in(&self, dirs: &[PathBuf], stem: &str, extensions: &[&str]) -> Option<PathBuf> {
        for dir in dirs {
            for ext in extensions {
                let candidate = dir.join(format!("{stem}{ext}"));
                if self.exists(&candidate) {
                    return Some(candidate);
                }
            }
        }
        None
    }

    /// The contents of `path`, from the input provider or the disk.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] when it cannot be read.
    pub fn read(&self, path: &Path) -> Result<Vec<u8>> {
        read_input(self.provider.as_deref(), path)
    }
}

/// The contents of `path`, from `provider` or the disk.
///
/// # Errors
///
/// [`Error::Io`] when it cannot be read.
pub fn read_input(provider: Option<&dyn InputProvider>, path: &Path) -> Result<Vec<u8>> {
    if let Some(data) = provider.and_then(|p| p.read(path)) {
        return Ok(data.to_vec());
    }
    std::fs::read(path).map_err(|error| Error::io(path, error))
}

struct Pending {
    path: PathBuf,
    input: DarwinInput,
}

/// Finds, maps and classifies every input of a link for `arch`.
///
/// # Errors
///
/// [`Error::NotFound`] for missing libraries and frameworks, [`Error::Io`]
/// for unreadable files, [`Error::Unimplemented`] for LLVM bitcode, and
/// parse errors.
pub fn collect<'t>(
    options: &LinkOptions,
    config: &Config,
    table: &'t FileTable,
    diagnostics: &dyn DiagnosticSink,
    generated: &[(PathBuf, Arc<[u8]>)],
) -> Result<Collected<'t>> {
    let search = SearchPaths::new(options);
    let mut specs: Vec<DarwinInput> = Vec::new();
    for spec in &options.inputs {
        match &spec.kind {
            crate::args::InputKind::File(path) => specs.push(DarwinInput {
                kind: DarwinInputKind::File(path.clone()),
                mode: LoadMode::Normal,
                force_load: spec.attrs.whole_archive,
            }),
            crate::args::InputKind::Library(name) => specs.push(DarwinInput {
                kind: DarwinInputKind::Library(name.clone()),
                mode: LoadMode::Normal,
                force_load: false,
            }),
            _ => {
                return Err(Error::Unimplemented(format!(
                    "input {:?} for Mach-O links",
                    spec.kind
                )));
            }
        }
    }
    // The bundle loader comes first, as in lld.
    if let Some(loader) = &options.darwin.bundle_loader {
        specs.insert(
            0,
            DarwinInput {
                kind: DarwinInputKind::File(loader.clone()),
                mode: LoadMode::Normal,
                force_load: false,
            },
        );
    }
    specs.extend(options.darwin.inputs.iter().cloned());

    let mut walker = Walker {
        options,
        config,
        table,
        search: &search,
        diagnostics,
        entries: Vec::new(),
        dylibs: Vec::new(),
        seen_dylibs: HashSet::new(),
        ordinal: 0,
        first_platform: None,
        linker_option_libraries: Vec::new(),
    };
    let pending = resolve_specs(&search, &specs)?;
    walker.walk(&pending)?;
    // Objects the linker generates (Objective-C selector stubs) follow the
    // command line.
    for (name, data) in generated {
        let id = table.add_bytes(name.clone(), Arc::clone(data))?;
        walker.add(
            id,
            &DarwinInput {
                kind: DarwinInputKind::File(name.clone()),
                mode: LoadMode::Normal,
                force_load: false,
            },
        )?;
    }

    // Libraries and frameworks requested by `LC_LINKER_OPTION` in the
    // objects, searched after everything on the command line. Missing ones
    // are warnings, as in ld64.
    // `-r` passes them on to the final link instead.
    let mut requested: Vec<DarwinInput> = Vec::new();
    let mut seen = HashSet::new();
    let linker_options = std::mem::take(&mut walker.linker_option_libraries);
    for request in linker_options
        .into_iter()
        .filter(|_| !config.is_relocatable())
    {
        if seen.insert(request.clone()) {
            requested.push(request);
        }
    }
    let mut extra = Vec::new();
    for request in &requested {
        match resolve_spec(&search, request) {
            Ok(path) => extra.push(Pending {
                path,
                input: request.clone(),
            }),
            Err(Error::NotFound(message)) => {
                diagnostics.emit(Diagnostic::warning(format!(
                    "{message} (requested by LC_LINKER_OPTION)"
                )));
            }
            Err(other) => return Err(other),
        }
    }
    walker.walk(&extra)?;
    requested.extend(specs);

    Ok(Collected {
        table,
        entries: walker.entries,
        dylibs: walker.dylibs,
        first_platform: walker.first_platform,
        requested,
    })
}

/// The `LC_LINKER_OPTION` libraries and frameworks of the live objects in
/// `files` that `collected` did not link, and that the search paths can
/// find. Archive members extracted during resolution can ask for libraries
/// the first collection never saw.
#[must_use]
pub fn missing_linker_options(
    options: &LinkOptions,
    collected: &Collected<'_>,
    files: &[MachInput<'_>],
    live: impl Fn(usize) -> bool,
) -> Vec<DarwinInput> {
    let search = SearchPaths::new(options);
    let mut out: Vec<DarwinInput> = Vec::new();
    for (index, file) in files.iter().enumerate() {
        if !live(index) {
            continue;
        }
        let Some(object) = &file.object else {
            continue;
        };
        let Ok(hints) = object.file.linker_option_hints() else {
            continue;
        };
        for hint in hints {
            let kind = match hint {
                LinkerOptionHint::Library(name) => {
                    DarwinInputKind::Library(String::from_utf8_lossy(name).into_owned())
                }
                LinkerOptionHint::Framework(name) => DarwinInputKind::Framework {
                    name: String::from_utf8_lossy(name).into_owned(),
                    suffix: None,
                },
                LinkerOptionHint::Other => continue,
            };
            let input = DarwinInput {
                kind,
                mode: LoadMode::Normal,
                force_load: false,
            };
            let known = collected.requested.iter().any(|r| r.kind == input.kind)
                || out.iter().any(|r| r.kind == input.kind);
            if !known && resolve_spec(&search, &input).is_ok() {
                out.push(input);
            }
        }
    }
    out
}

fn resolve_spec(search: &SearchPaths, input: &DarwinInput) -> Result<PathBuf> {
    match &input.kind {
        DarwinInputKind::File(path) => Ok(path.clone()),
        DarwinInputKind::Library(name) => search
            .find_library(name)
            .ok_or_else(|| Error::NotFound(format!("library not found for -l{name}"))),
        DarwinInputKind::Framework { name, suffix } => search
            .find_framework(name, suffix.as_deref())
            .ok_or_else(|| Error::NotFound(format!("framework not found for -framework {name}"))),
    }
}

fn resolve_specs(search: &SearchPaths, specs: &[DarwinInput]) -> Result<Vec<Pending>> {
    specs
        .iter()
        .map(|input| {
            Ok(Pending {
                path: resolve_spec(search, input)?,
                input: input.clone(),
            })
        })
        .collect()
}

struct Walker<'o, 't> {
    options: &'o LinkOptions,
    config: &'o Config,
    table: &'t FileTable,
    search: &'o SearchPaths,
    diagnostics: &'o dyn DiagnosticSink,
    entries: Vec<Entry>,
    dylibs: Vec<LoadedDylib>,
    seen_dylibs: HashSet<Vec<u8>>,
    ordinal: u32,
    first_platform: Option<(u32, PackedVersion, PackedVersion)>,
    linker_option_libraries: Vec<DarwinInput>,
}

impl<'t> Walker<'_, 't> {
    fn next_position(&mut self) -> Result<u32> {
        self.ordinal = self
            .ordinal
            .checked_add(1)
            .ok_or_else(|| Error::Limit("too many inputs".into()))?;
        Ok(self.ordinal)
    }

    fn walk(&mut self, pending: &[Pending]) -> Result<()> {
        let sources: Vec<Source> = pending
            .iter()
            .map(|p| Source::Path(p.path.clone()))
            .collect();
        let loaded = self.table.load_all(&sources);
        for (entry, id) in pending.iter().zip(loaded) {
            let id = id?;
            self.add(id, &entry.input)?;
        }
        Ok(())
    }

    fn file(&self, id: FileId) -> Result<&'t InputFile> {
        self.table
            .get(id)
            .ok_or_else(|| Error::Internal("loaded file missing from table".into()))
    }

    fn arch_mismatch(&self, file: &InputFile, found: Arch) {
        self.diagnostics.emit(Diagnostic::warning(format!(
            "ignoring file {}, built for {found} (linking for {})",
            file.path().display(),
            self.config.arch
        )));
    }

    fn add(&mut self, id: FileId, input: &DarwinInput) -> Result<()> {
        let file = self.file(id)?;
        match file.format() {
            FileFormat::MachO(ident) => {
                let arch = Arch::new(ident.cpu_type, ident.cpu_subtype);
                match ident.file_type {
                    crate::macho::read::consts::MH_OBJECT => {
                        if arch.cpu_type != self.config.arch.cpu_type {
                            self.arch_mismatch(file, arch);
                            return Ok(());
                        }
                        let position = self.next_position()?;
                        self.note_object(file)?;
                        self.entries.push(Entry::Object(ObjectEntry {
                            id,
                            position: InputPosition::new(position, 0),
                            live: true,
                            hidden: false,
                            mtime: file_mtime(file.path()),
                        }));
                        Ok(())
                    }
                    crate::macho::read::consts::MH_DYLIB
                    | crate::macho::read::consts::MH_DYLIB_STUB => {
                        if arch.cpu_type != self.config.arch.cpu_type {
                            self.arch_mismatch(file, arch);
                            return Ok(());
                        }
                        let dylib = Dylib::parse(file.data(), source_of(file))?;
                        let path = file.path().to_path_buf();
                        self.add_binary_dylib(&dylib, path, input.mode)
                    }
                    crate::macho::read::consts::MH_EXECUTE
                        if self.options.darwin.bundle_loader.is_some() =>
                    {
                        if arch.cpu_type != self.config.arch.cpu_type {
                            self.arch_mismatch(file, arch);
                            return Ok(());
                        }
                        let image = Dylib::parse_image(file.data(), source_of(file))?;
                        let path = file.path().to_path_buf();
                        self.add_binary_dylib(&image, path, input.mode)?;
                        if let Some(loader) = self.dylibs.last_mut() {
                            loader.bundle_loader = true;
                        }
                        Ok(())
                    }
                    other => Err(file.malformed(
                        12,
                        format!("Mach-O file type {other} (expected an object or a dylib)"),
                    )),
                }
            }
            FileFormat::Fat(_) => {
                let fat = FatFile::parse(file.data(), source_of(file))?;
                let slice = fat.select(self.config.arch)?;
                let name = file.path().to_path_buf();
                let data: Arc<[u8]> = Arc::from(slice.data);
                let slice_id = self.table.add_bytes(name, data)?;
                if matches!(self.file(slice_id)?.format(), FileFormat::Fat(_)) {
                    return Err(file.malformed(0, "universal file (nested universal slice)"));
                }
                self.add(slice_id, input)
            }
            FileFormat::Archive => self.add_archive(id, input),
            FileFormat::ThinArchive => Err(Error::Unimplemented(format!(
                "thin archive {} in a Mach-O link",
                file.path().display()
            ))),
            FileFormat::Text(TextKind::Tbd) => {
                let path = file.path().to_path_buf();
                let stub = TextStub::parse(file.data(), source_of(file))?;
                self.add_stub(&stub, path, input.mode)
            }
            FileFormat::LlvmBitcode(_) => Err(Error::Unimplemented(format!(
                "LLVM bitcode input {} (Mach-O LTO)",
                file.path().display()
            ))),
            FileFormat::Empty => Ok(()),
            _ => Err(file.malformed(
                0,
                "file format not recognized (expected a Mach-O object, archive, dylib or .tbd)",
            )),
        }
    }

    /// Records the build version of the first object and its
    /// `LC_LINKER_OPTION` requests.
    fn note_object(&mut self, file: &InputFile) -> Result<()> {
        let object = ObjectFile::parse(file.data(), source_of(file))?;
        if self.first_platform.is_none()
            && let Some(version) = object.build_version()
        {
            self.first_platform = Some((version.platform, version.minos, version.sdk));
        }
        for hint in object.linker_option_hints()? {
            let kind = match hint {
                LinkerOptionHint::Library(name) => {
                    DarwinInputKind::Library(String::from_utf8_lossy(name).into_owned())
                }
                LinkerOptionHint::Framework(name) => DarwinInputKind::Framework {
                    name: String::from_utf8_lossy(name).into_owned(),
                    suffix: None,
                },
                _ => continue,
            };
            self.linker_option_libraries.push(DarwinInput {
                kind,
                mode: LoadMode::Normal,
                force_load: false,
            });
        }
        Ok(())
    }

    fn add_archive(&mut self, id: FileId, input: &DarwinInput) -> Result<()> {
        let file = self.file(id)?;
        let archive = file.archive()?;
        let position = self.next_position()?;
        let force = input.force_load || self.options.darwin.all_load;
        let hidden = input.mode == LoadMode::Hidden;
        let mut members = Vec::new();
        for member in archive.members() {
            members.push(member?);
        }
        for (ordinal, member) in members.iter().enumerate() {
            let ordinal = u32::try_from(ordinal)
                .map_err(|_| Error::Limit("too many archive members".into()))?;
            let member_id = self.table.add_member(id, member)?;
            let member_file = self.file(member_id)?;
            let FileFormat::MachO(ident) = member_file.format() else {
                // Symbol tables (`__.SYMDEF`) and anything else that is not
                // an object.
                if matches!(member_file.format(), FileFormat::LlvmBitcode(_)) {
                    return Err(Error::Unimplemented(format!(
                        "LLVM bitcode member {}({}) (Mach-O LTO)",
                        member_file.path().display(),
                        member_file.member().unwrap_or("")
                    )));
                }
                continue;
            };
            if ident.file_type != crate::macho::read::consts::MH_OBJECT
                || ident.cpu_type != self.config.arch.cpu_type
            {
                continue;
            }
            let live = force || (self.options.darwin.objc && defines_objc(member_file)?);
            if live {
                self.note_object(member_file)?;
            }
            // The member's modification time, from its `ar` header, for the
            // debug map.
            let date = usize::try_from(member.header_offset)
                .ok()
                .and_then(|at| file.data().get(at.checked_add(16)?..at.checked_add(28)?))
                .and_then(|field| std::str::from_utf8(field).ok())
                .and_then(|text| text.trim().parse::<u64>().ok())
                .unwrap_or(0);
            self.entries.push(Entry::Object(ObjectEntry {
                id: member_id,
                position: InputPosition::new(position, ordinal),
                live,
                hidden,
                mtime: if zero_mtime() { 0 } else { date },
            }));
        }
        Ok(())
    }

    fn push_dylib(&mut self, dylib: LoadedDylib) -> Result<()> {
        if !self.seen_dylibs.insert(dylib.install_name.clone()) {
            return Ok(());
        }
        let position = self.next_position()?;
        let index = self.dylibs.len();
        self.dylibs.push(dylib);
        self.entries
            .push(Entry::Dylib(index, InputPosition::new(position, 0)));
        Ok(())
    }

    fn stub_target(&self) -> StubTarget {
        let name = self.config.arch.name().unwrap_or("arm64");
        StubTarget::new(name, self.config.platform.platform)
    }

    fn add_stub(&mut self, stub: &TextStub, path: PathBuf, mode: LoadMode) -> Result<()> {
        let Some(main) = stub.main() else {
            return Ok(());
        };
        let target = self.stub_target();
        if main.select_target(&target).is_none() {
            self.diagnostics.emit(Diagnostic::warning(format!(
                "ignoring {}: no target compatible with {target}",
                path.display()
            )));
            return Ok(());
        }
        let mut exports = Vec::new();
        let mut gather = Gather::default();
        self.stub_exports(
            stub,
            main.install_name.as_bytes(),
            &target,
            &mut exports,
            &mut gather,
            0,
        )?;
        exports.sort();
        exports.dedup_by(|a, b| a.name == b.name);
        self.push_dylib(LoadedDylib {
            path,
            install_name: main.install_name.as_bytes().to_vec(),
            current_version: main.current_version,
            compatibility_version: main.compatibility_version,
            mode,
            exports,
            implicit: false,
            bundle_loader: false,
        })?;
        // Implicitly linked re-exports follow their parent, as lld loads
        // them.
        for dylib in gather.implicit {
            self.push_dylib(dylib)?;
        }
        Ok(())
    }

    /// Collects the exports of `install_name` (inlined in `stub`, or found
    /// under the roots) and of the libraries it re-exports. `gather.visited`
    /// holds the install names already collected, so cycles end. Returns
    /// the library's current and compatibility versions, when found.
    fn stub_exports(
        &self,
        stub: &TextStub,
        install_name: &[u8],
        target: &StubTarget,
        out: &mut Vec<DylibExport>,
        gather: &mut Gather,
        depth: usize,
    ) -> Result<Option<(PackedVersion, PackedVersion)>> {
        if depth > 32 || !gather.visited.insert(install_name.to_vec()) {
            return Ok(None);
        }
        let name = String::from_utf8_lossy(install_name);
        let Some(library) = stub.library(&name) else {
            return self.external_exports(install_name, out, gather, depth);
        };
        let Some(selected) = library.select_target(target) else {
            return Ok(None);
        };
        let selected = selected.clone();
        for symbol in library.exports_for(&selected) {
            out.push(DylibExport {
                name: symbol.name.into_bytes(),
                weak: symbol.kind == StubSymbolKind::Weak,
                tlv: symbol.kind == StubSymbolKind::ThreadLocal,
            });
        }
        let reexports: Vec<String> = library
            .reexported_libraries_for(&selected)
            .into_iter()
            .map(str::to_owned)
            .collect();
        for reexport in reexports {
            self.reexport(
                Some(stub),
                reexport.as_bytes(),
                out,
                gather,
                depth.saturating_add(1),
            )?;
        }
        Ok(Some((
            library.current_version,
            library.compatibility_version,
        )))
    }

    /// Follows a re-exported library. Libraries in public locations
    /// (`/usr/lib`, `/System/Library/Frameworks`) are linked implicitly, as
    /// ld64 and lld do: their symbols bind to their own load command, added
    /// when something uses them. Others' exports count as the parent's.
    fn reexport(
        &self,
        stub: Option<&TextStub>,
        install_name: &[u8],
        out: &mut Vec<DylibExport>,
        gather: &mut Gather,
        depth: usize,
    ) -> Result<()> {
        if gather.visited.contains(install_name) {
            return Ok(());
        }
        let target = self.stub_target();
        let mut own = Vec::new();
        let implicit = is_implicitly_linked(install_name);
        let destination = if implicit { &mut own } else { &mut *out };
        let versions = match stub {
            Some(stub) => {
                self.stub_exports(stub, install_name, &target, destination, gather, depth)?
            }
            None => {
                gather.visited.insert(install_name.to_vec());
                self.external_exports(install_name, destination, gather, depth)?
            }
        };
        if implicit && let Some((current_version, compatibility_version)) = versions {
            own.sort();
            own.dedup_by(|a, b| a.name == b.name);
            gather.implicit.push(LoadedDylib {
                path: PathBuf::from(String::from_utf8_lossy(install_name).into_owned()),
                install_name: install_name.to_vec(),
                current_version,
                compatibility_version,
                mode: LoadMode::Normal,
                exports: own,
                implicit: true,
                bundle_loader: false,
            });
        }
        Ok(())
    }

    /// Exports of a re-exported library that is not inlined in the stub
    /// that names it: found on disk under the roots, as a `.tbd` or a dylib.
    /// The caller has already added `install_name` to `visited`.
    fn external_exports(
        &self,
        install_name: &[u8],
        out: &mut Vec<DylibExport>,
        gather: &mut Gather,
        depth: usize,
    ) -> Result<Option<(PackedVersion, PackedVersion)>> {
        let Some(path) = self.search.find_install_name(install_name) else {
            self.diagnostics.emit(Diagnostic::warning(format!(
                "unable to locate re-exported library {}",
                String::from_utf8_lossy(install_name)
            )));
            return Ok(None);
        };
        let data = self.search.read(&path)?;
        let source = MachSource::new(&path);
        let data = match crate::input::identify(&data) {
            FileFormat::Fat(_) => {
                let fat = FatFile::parse(&data, source)?;
                fat.select(self.config.arch)?.data.to_vec()
            }
            _ => data,
        };
        match crate::input::identify(&data) {
            FileFormat::Text(TextKind::Tbd) => {
                let stub = TextStub::parse(&data, source)?;
                let target = self.stub_target();
                // The file describes `install_name` itself: collect it
                // without the visited check that already passed.
                gather.visited.remove(install_name);
                self.stub_exports(
                    &stub,
                    install_name,
                    &target,
                    out,
                    gather,
                    depth.saturating_add(1),
                )
            }
            FileFormat::MachO(_) => {
                let dylib = Dylib::parse(&data, source)?;
                self.binary_exports(&dylib, out, gather, depth.saturating_add(1))
            }
            _ => Ok(None),
        }
    }

    fn binary_exports(
        &self,
        dylib: &Dylib<'_>,
        out: &mut Vec<DylibExport>,
        gather: &mut Gather,
        depth: usize,
    ) -> Result<Option<(PackedVersion, PackedVersion)>> {
        let versions = dylib.id().map_or(
            (PackedVersion::new(1, 0, 0), PackedVersion::new(1, 0, 0)),
            |id| (id.current_version, id.compatibility_version),
        );
        if dylib.exports_trie().is_empty() {
            for symbol in dylib.symbols().iter() {
                let symbol = symbol?;
                if symbol.is_external() && symbol.is_defined() && !symbol.is_private_external() {
                    out.push(DylibExport {
                        name: symbol.name.to_vec(),
                        weak: symbol.is_weak_def(),
                        tlv: false,
                    });
                }
            }
        } else {
            for export in dylib.exports() {
                let export = export?;
                out.push(DylibExport {
                    weak: export.is_weak(),
                    tlv: export.is_thread_local(),
                    name: export.name,
                });
            }
        }
        if depth > 32 {
            return Ok(Some(versions));
        }
        let reexports: Vec<Vec<u8>> = dylib.reexports().map(|d| d.name.to_vec()).collect();
        for name in reexports {
            self.reexport(None, &name, out, gather, depth.saturating_add(1))?;
        }
        Ok(Some(versions))
    }

    fn add_binary_dylib(&mut self, dylib: &Dylib<'_>, path: PathBuf, mode: LoadMode) -> Result<()> {
        let mut exports = Vec::new();
        let mut gather = Gather::default();
        gather.visited.insert(dylib.install_name().to_vec());
        self.binary_exports(dylib, &mut exports, &mut gather, 0)?;
        exports.sort();
        exports.dedup_by(|a, b| a.name == b.name);
        let (current_version, compatibility_version) = dylib.id().map_or(
            (PackedVersion::new(1, 0, 0), PackedVersion::new(1, 0, 0)),
            |id| (id.current_version, id.compatibility_version),
        );
        let install_name = if dylib.install_name().is_empty() {
            path.as_os_str().as_encoded_bytes().to_vec()
        } else {
            dylib.install_name().to_vec()
        };
        self.push_dylib(LoadedDylib {
            path,
            install_name,
            current_version,
            compatibility_version,
            mode,
            exports,
            implicit: false,
            bundle_loader: false,
        })?;
        // Implicitly linked re-exports follow their parent, as lld loads
        // them.
        for dylib in gather.implicit {
            self.push_dylib(dylib)?;
        }
        Ok(())
    }
}

/// State shared while following re-exports.
#[derive(Default)]
struct Gather {
    visited: HashSet<Vec<u8>>,
    implicit: Vec<LoadedDylib>,
}

/// Whether a re-exported library is linked implicitly: its install name is
/// directly in `/usr/lib`, or is a framework's binary in
/// `/System/Library/Frameworks` (lld's rule).
#[must_use]
pub fn is_implicitly_linked(install_name: &[u8]) -> bool {
    let Ok(name) = std::str::from_utf8(install_name) else {
        return false;
    };
    let path = Path::new(name);
    if path.parent() == Some(Path::new("/usr/lib")) {
        return true;
    }
    if let Some(rest) = name.strip_prefix("/System/Library/Frameworks/") {
        let framework = rest.split('.').next().unwrap_or("");
        return path.file_name().and_then(|f| f.to_str()) == Some(framework);
    }
    false
}

/// `ZERO_AR_DATE`: record zero modification times in the debug map, for
/// reproducible outputs (ld64 and lld read it too).
fn zero_mtime() -> bool {
    std::env::var_os("ZERO_AR_DATE").is_some_and(|v| !v.is_empty() && v != "0")
}

/// The modification time of `path` in seconds, for the debug map.
fn file_mtime(path: &Path) -> u64 {
    if zero_mtime() {
        return 0;
    }
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs())
}

/// Whether an archive member defines an Objective-C class or category, for
/// `-ObjC`.
fn defines_objc(file: &InputFile) -> Result<bool> {
    let object = ObjectFile::parse(file.data(), source_of(file))?;
    if object
        .sections()
        .iter()
        .any(|s| s.sectname == b"__objc_catlist" || s.sectname == b"__objc_classlist")
    {
        return Ok(true);
    }
    for symbol in object.symbols().iter() {
        let symbol = symbol?;
        if symbol.is_external() && symbol.is_defined() && symbol.name.starts_with(b"_OBJC_CLASS_$_")
        {
            return Ok(true);
        }
    }
    Ok(false)
}
