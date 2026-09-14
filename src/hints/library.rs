//! The index of libraries a link could have used.
//!
//! [`LibraryIndex::build`] lists the files `-l` could name in each search
//! directory (`lib*.so`, `lib*.a`, versioned `*.so.N` files, and linker
//! scripts such as glibc's `libc.so`), maps each distinct file once through
//! [`FileTable`], and records which of them define the symbols asked about:
//! `.dynsym` definitions with their versions for shared objects, the symbol
//! index for archives, and the files a script's `GROUP` or `INPUT` pulls in.
//!
//! Only the names asked about are recorded, so the index stays small even
//! over a full `/usr/lib64`. Files are mapped and scanned in parallel on the
//! current rayon pool; everything is recorded in search order, so results
//! never depend on scheduling.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use hashbrown::{HashMap, HashSet};
use rayon::prelude::*;

use crate::elf::read::consts::{STB_LOCAL, STV_HIDDEN, STV_INTERNAL};
use crate::elf::read::{
    Elf32Be, Elf32Le, Elf64Be, Elf64Le, ElfFormat, ElfKind, SharedObject, Source,
};
use crate::input::{Archive, FileFormat, FileTable, LibraryNaming, RealFileSystem, SearchContext};
use crate::script::{CommandKind, InputName, NoIncludes, parse_script};

use super::{LinkedLibrary, SearchScope};

type FastSet<'a> = HashSet<&'a [u8], foldhash::fast::FixedState>;

/// Linker scripts may name other scripts; deeper chains are ignored.
const MAX_SCRIPT_DEPTH: usize = 4;

/// Whether a library is a shared object or an archive.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LibraryKind {
    /// A shared object.
    Shared,
    /// A static archive.
    Static,
}

/// A distinct file the index scanned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Object {
    /// The file, with symbolic links resolved.
    pub path: PathBuf,
    /// How diagnostics name it: a shared object's `DT_SONAME`, or the file
    /// name.
    pub display: String,
    /// Shared object, archive, or `None` for linker scripts and files that
    /// are neither.
    pub kind: Option<LibraryKind>,
}

/// A file in a search directory that `-l` or `-l:` can name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The path as found in the search directory (links not resolved).
    pub path: PathBuf,
    /// Index of the search directory, in search order.
    pub dir: usize,
    /// The libraries it links: itself, or a linker script's members.
    pub objects: Vec<usize>,
    /// Whether the entry is a linker script.
    pub script: bool,
}

/// One definition of a name asked about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Definition {
    /// Index into [`LibraryIndex::objects`].
    pub object: usize,
    /// The symbol version (`GLIBC_2.14`), for versioned shared objects.
    pub version: Option<String>,
    /// Whether the version is hidden (`foo@V` rather than the default
    /// `foo@@V`): only references to exactly that version bind to it.
    pub hidden: bool,
    /// The archive member defining it, for archives.
    pub member: Option<String>,
}

/// What scanning one file found.
#[derive(Debug, Default)]
struct Scan {
    display: String,
    kind: Option<LibraryKind>,
    definitions: Vec<(Vec<u8>, Definition)>,
    /// A linker script's inputs, as written.
    members: Vec<PathBuf>,
    /// The object ids the members resolved to.
    member_ids: Vec<usize>,
    script: bool,
}

/// Symbols defined by the libraries in the search path, for a set of names.
#[derive(Debug, Default)]
pub struct LibraryIndex {
    entries: Vec<Entry>,
    objects: Vec<Object>,
    definitions: HashMap<Vec<u8>, Vec<Definition>>,
    /// Objects the link already uses, and the library that brought each in.
    linked: BTreeMap<usize, LinkedLibrary>,
    /// Archives linked with `-Bstatic`, by `-l` name (`m` for `libm.a`).
    linked_static: BTreeMap<String, PathBuf>,
}

impl LibraryIndex {
    /// Scans the search directories of `scope` for definitions of `names`.
    ///
    /// `linked` lists the libraries already on the link line, so hints can
    /// tell a missing library from one that was linked but not used.
    /// Unreadable and malformed files are skipped.
    #[must_use]
    pub fn build(scope: &SearchScope, linked: &[LinkedLibrary], names: &[&[u8]]) -> Self {
        let mut builder = Builder {
            scope,
            wanted: names.iter().copied().collect(),
            loaded: HashMap::default(),
            objects: Vec::new(),
            scans: Vec::new(),
        };

        let listed: Vec<Vec<PathBuf>> = scope
            .directories()
            .par_iter()
            .map(|dir| list_dir(dir))
            .collect();
        let mut entry_paths: Vec<(usize, PathBuf)> = Vec::new();
        for (dir, files) in listed.into_iter().enumerate() {
            entry_paths.extend(files.into_iter().map(|file| (dir, file)));
        }
        let paths: Vec<PathBuf> = entry_paths
            .iter()
            .map(|(_, path)| path.clone())
            .chain(linked.iter().map(|l| l.path.clone()))
            .collect();
        let ids = builder.load_all(&paths, 0);
        let (entry_ids, linked_ids) = ids.split_at(entry_paths.len().min(ids.len()));

        let mut index = Self::default();
        for ((dir, path), id) in entry_paths.into_iter().zip(entry_ids) {
            let Some(id) = *id else {
                continue;
            };
            let objects = builder.expand(id, 0);
            if objects.is_empty() {
                continue;
            }
            let script = builder.scans.get(id).is_some_and(|scan| scan.script);
            index.entries.push(Entry {
                path,
                dir,
                objects,
                script,
            });
        }
        for (library, id) in linked.iter().zip(linked_ids) {
            if let Some(id) = *id {
                for object in builder.expand(id, 0) {
                    index
                        .linked
                        .entry(object)
                        .or_insert_with(|| library.clone());
                }
            }
            if library.static_only
                && let Some(name) = library_name(&library.path, ".a")
            {
                index
                    .linked_static
                    .entry(name)
                    .or_insert_with(|| library.path.clone());
            }
        }
        for (id, (scan, path)) in builder.scans.into_iter().zip(builder.objects).enumerate() {
            for (name, mut definition) in scan.definitions {
                definition.object = id;
                index.definitions.entry(name).or_default().push(definition);
            }
            index.objects.push(Object {
                path,
                display: scan.display,
                kind: scan.kind,
            });
        }
        index
    }

    /// The entries (files `-l` can name), in search order.
    #[must_use]
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// The distinct files scanned.
    #[must_use]
    pub fn objects(&self) -> &[Object] {
        &self.objects
    }

    /// The definitions of `name` found, in the order the files were found.
    #[must_use]
    pub fn definitions(&self, name: &[u8]) -> &[Definition] {
        self.definitions.get(name).map_or(&[], Vec::as_slice)
    }

    /// The linked library that brought in `object`, if it is linked.
    #[must_use]
    pub fn linked(&self, object: usize) -> Option<&LinkedLibrary> {
        self.linked.get(&object)
    }

    /// The archive linked for `-l<name>` because of `-Bstatic`, if any.
    #[must_use]
    pub fn linked_static(&self, name: &str) -> Option<&Path> {
        self.linked_static.get(name).map(PathBuf::as_path)
    }

    /// The preferred entry that links `object`: earliest search directory,
    /// then names `-l` can find (`libm.so` before `libm.so.6`), then by
    /// path.
    #[must_use]
    pub fn best_entry(&self, object: usize) -> Option<&Entry> {
        self.entries
            .iter()
            .filter(|entry| entry.objects.contains(&object))
            .min_by(|a, b| {
                let key = |e: &Entry| {
                    let plain = library_name(&e.path, ".so").is_some()
                        || library_name(&e.path, ".a").is_some();
                    (e.dir, !plain)
                };
                key(a).cmp(&key(b)).then_with(|| a.path.cmp(&b.path))
            })
    }

    /// The command-line flag that links `entry`: `-lname` if that finds this
    /// very file, else `-l:file` if that does, else the path itself.
    #[must_use]
    pub fn flag(scope: &SearchScope, entry: &Entry) -> String {
        let fs = RealFileSystem;
        let context = SearchContext {
            search_paths: &scope.search_paths,
            sysroot: scope.sysroot.as_deref(),
            naming: LibraryNaming::Elf,
            fs: &fs,
        };
        let finds_it = |found: Option<PathBuf>| found.is_some_and(|found| found == entry.path);
        for suffix in [".so", ".a"] {
            if let Some(name) = library_name(&entry.path, suffix)
                && finds_it(context.find_library(&name, false))
            {
                return format!("-l{name}");
            }
        }
        if let Some(file) = entry.path.file_name().and_then(|n| n.to_str())
            && finds_it(context.find_exact(file))
        {
            return format!("-l:{file}");
        }
        entry.path.display().to_string()
    }
}

/// `name` for a file named `lib<name><suffix>`.
pub(crate) fn library_name(path: &Path, suffix: &str) -> Option<String> {
    let file = path.file_name()?.to_str()?;
    let name = file.strip_prefix("lib")?.strip_suffix(suffix)?;
    (!name.is_empty()).then(|| name.to_string())
}

/// The files of `dir` that `-l` or `-l:` could name, sorted by name.
fn list_dir(dir: &Path) -> Vec<PathBuf> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = read
        .filter_map(Result::ok)
        .filter(|entry| {
            entry.file_name().to_str().is_some_and(|name| {
                name.ends_with(".so") || name.ends_with(".a") || name.contains(".so.")
            })
        })
        .map(|entry| entry.path())
        .collect();
    files.sort();
    files
}

struct Builder<'s, 'w> {
    scope: &'s SearchScope,
    wanted: FastSet<'w>,
    /// Canonical path to object id.
    loaded: HashMap<PathBuf, usize>,
    objects: Vec<PathBuf>,
    scans: Vec<Scan>,
}

impl Builder<'_, '_> {
    /// Loads every path, scanning new files in parallel and following linker
    /// scripts, and returns each path's object id (`None` if unreadable).
    fn load_all(&mut self, paths: &[PathBuf], depth: usize) -> Vec<Option<usize>> {
        let canonical: Vec<Option<PathBuf>> = paths
            .par_iter()
            .map(|path| std::fs::canonicalize(path).ok().filter(|p| p.is_file()))
            .collect();
        let mut fresh: Vec<PathBuf> = Vec::new();
        {
            let mut seen: HashSet<&Path> = HashSet::new();
            for path in canonical.iter().flatten() {
                if !self.loaded.contains_key(path) && seen.insert(path) {
                    fresh.push(path.clone());
                }
            }
        }
        let wanted = &self.wanted;
        let scans: Vec<Scan> = fresh
            .par_iter()
            .map(|path| scan_file(path, wanted))
            .collect();
        let mut scripts = Vec::new();
        for (path, scan) in fresh.into_iter().zip(scans) {
            let id = self.objects.len();
            if scan.script {
                scripts.push(id);
            }
            self.loaded.insert(path.clone(), id);
            self.objects.push(path);
            self.scans.push(scan);
        }
        if depth < MAX_SCRIPT_DEPTH {
            for script in scripts {
                let script_path = self.objects.get(script).cloned().unwrap_or_default();
                let members: Vec<PathBuf> = self
                    .scans
                    .get(script)
                    .map(|scan| {
                        scan.members
                            .iter()
                            .map(|member| resolve_member(self.scope, &script_path, member))
                            .collect()
                    })
                    .unwrap_or_default();
                let ids: Vec<usize> = self
                    .load_all(&members, depth.saturating_add(1))
                    .into_iter()
                    .flatten()
                    .collect();
                if let Some(scan) = self.scans.get_mut(script) {
                    scan.member_ids = ids;
                }
            }
        }
        canonical
            .into_iter()
            .map(|path| path.and_then(|p| self.loaded.get(&p).copied()))
            .collect()
    }

    /// The libraries an object links: itself, or a script's members.
    fn expand(&self, id: usize, depth: usize) -> Vec<usize> {
        let Some(scan) = self.scans.get(id) else {
            return Vec::new();
        };
        if !scan.script {
            return if scan.kind.is_some() {
                vec![id]
            } else {
                Vec::new()
            };
        }
        if depth >= MAX_SCRIPT_DEPTH {
            return Vec::new();
        }
        let mut out = Vec::new();
        for &member in &scan.member_ids {
            for object in self.expand(member, depth.saturating_add(1)) {
                if !out.contains(&object) {
                    out.push(object);
                }
            }
        }
        out
    }
}

/// Resolves a path written in a linker script, the way GNU ld does: `=` and
/// `$SYSROOT` prefixes, absolute paths inside the sysroot when the script
/// itself is in it, relative paths next to the script, then in the search
/// directories. `-l` names are searched for.
fn resolve_member(scope: &SearchScope, script: &Path, member: &Path) -> PathBuf {
    let fs = RealFileSystem;
    let context = SearchContext {
        search_paths: &scope.search_paths,
        sysroot: scope.sysroot.as_deref(),
        naming: LibraryNaming::Elf,
        fs: &fs,
    };
    if let Some(name) = member.to_str().and_then(|m| m.strip_prefix("-l")) {
        return context.find_library(name, false).unwrap_or_default();
    }
    let text = member.as_os_str();
    if text
        .to_str()
        .is_some_and(|t| t.starts_with('=') || t.starts_with("$SYSROOT"))
    {
        return crate::input::search::apply_sysroot(member, scope.sysroot.as_deref());
    }
    if member.is_absolute() {
        if let Some(sysroot) = &scope.sysroot
            && let Ok(root) = std::fs::canonicalize(sysroot)
            && script.starts_with(&root)
        {
            let relative = member.strip_prefix("/").unwrap_or(member);
            return root.join(relative);
        }
        return member.to_path_buf();
    }
    if let Some(dir) = script.parent() {
        let beside = dir.join(member);
        if beside.is_file() {
            return beside;
        }
    }
    context.find_script(member).unwrap_or_default()
}

/// Maps and scans one file.
fn scan_file(path: &Path, wanted: &FastSet<'_>) -> Scan {
    let display = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut scan = Scan {
        display,
        ..Scan::default()
    };
    let table = FileTable::new();
    let Ok(id) = table.load_path(path) else {
        return scan;
    };
    let Some(file) = table.get(id) else {
        return scan;
    };
    let data = file.data();
    match file.format() {
        FileFormat::Elf(ident) if ident.is_shared() => {
            scan.kind = Some(LibraryKind::Shared);
            let source = Source::new(path);
            let _ = match ElfKind::identify(data) {
                Some(ElfKind::Elf64Le) => scan_shared::<Elf64Le>(data, source, wanted, &mut scan),
                Some(ElfKind::Elf64Be) => scan_shared::<Elf64Be>(data, source, wanted, &mut scan),
                Some(ElfKind::Elf32Le) => scan_shared::<Elf32Le>(data, source, wanted, &mut scan),
                Some(ElfKind::Elf32Be) => scan_shared::<Elf32Be>(data, source, wanted, &mut scan),
                None => None,
            };
        }
        FileFormat::Archive | FileFormat::ThinArchive => {
            scan.kind = Some(LibraryKind::Static);
            scan_archive(path, data, wanted, &mut scan);
        }
        FileFormat::Text(_) => {
            if let Ok(script) = parse_script(data, path, &mut NoIncludes) {
                scan.script = true;
                for command in &script.commands {
                    let (CommandKind::Group(files) | CommandKind::Input(files)) = &command.kind
                    else {
                        continue;
                    };
                    for input in files {
                        scan.members.push(match &input.name {
                            InputName::Path(bytes) => bytes_to_path(bytes),
                            InputName::Library(name) => {
                                let mut flag = b"-l".to_vec();
                                flag.extend_from_slice(name);
                                bytes_to_path(&flag)
                            }
                        });
                    }
                }
            }
        }
        _ => {}
    }
    scan
}

/// Records the `.dynsym` definitions of names in `wanted`.
fn scan_shared<F: ElfFormat>(
    data: &[u8],
    source: Source<'_>,
    wanted: &FastSet<'_>,
    scan: &mut Scan,
) -> Option<()> {
    let object = SharedObject::<F>::parse(data, source).ok()?;
    if let Some(soname) = object.soname() {
        scan.display = String::from_utf8_lossy(soname).into_owned();
    }
    let symbols = object.symbols();
    for (index, raw) in symbols.iter_raw().enumerate() {
        if raw.st_shndx == 0
            || raw.binding() == STB_LOCAL
            || matches!(raw.visibility(), STV_HIDDEN | STV_INTERNAL)
        {
            continue;
        }
        let Ok(name) = symbols.name(index, &raw) else {
            continue;
        };
        if !wanted.contains(name) {
            continue;
        }
        let Ok(version) = object.symbol_version(index) else {
            continue;
        };
        // Index 0 is a local (non-exported) symbol.
        if version.index == 0 {
            continue;
        }
        let version_name = version
            .info
            .filter(|info| !info.is_base())
            .map(|info| String::from_utf8_lossy(info.name).into_owned());
        scan.definitions.push((
            name.to_vec(),
            Definition {
                object: 0,
                version: version_name,
                hidden: version.hidden,
                member: None,
            },
        ));
    }
    Some(())
}

/// Records the archive symbol index entries of names in `wanted`.
fn scan_archive(path: &Path, data: &[u8], wanted: &FastSet<'_>, scan: &mut Scan) {
    let Ok(archive) = Archive::parse(path, data) else {
        return;
    };
    let Some(symbols) = archive.symbol_index() else {
        return;
    };
    for symbol in symbols.iter() {
        let Ok(symbol) = symbol else {
            break;
        };
        if !wanted.contains(symbol.name) {
            continue;
        }
        let member = archive
            .member_at(symbol.member_offset)
            .ok()
            .map(|member| member.display_name());
        scan.definitions.push((
            symbol.name.to_vec(),
            Definition {
                object: 0,
                version: None,
                hidden: false,
                member,
            },
        ));
    }
}

#[cfg(unix)]
fn bytes_to_path(bytes: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
}

#[cfg(not(unix))]
fn bytes_to_path(bytes: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
}
