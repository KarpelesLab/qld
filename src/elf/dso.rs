//! Shared object inputs.
//!
//! A shared object named on the command line (or found by `-l`, or listed
//! in a `GROUP` script) takes part in resolution through its dynamic symbol
//! table ([`SharedInput`]):
//!
//! - every defined global symbol with a default (or no) version is offered
//!   under its plain name, with [`DefinitionKind::Shared`] precedence;
//! - every versioned definition is also offered as `name@VERSION`, so a
//!   reference that names a version explicitly binds to it; a hidden
//!   (non-default) version is offered only that way;
//! - undefined symbols are references, so they extract archive members and
//!   export executable symbols the library needs.
//!
//! After resolution, [`plan_needed`] decides which shared objects get a
//! `DT_NEEDED` entry: all of them, except `--as-needed` ones that satisfy no
//! non-weak reference from a regular object or from another needed library
//! (as GNU ld decides). Symbols that bound to an unneeded library are bound
//! again to a needed one, or become undefined.
//!
//! [`check_shlib_undefined`] implements `--no-allow-shlib-undefined` (the
//! default for executables): undefined references of the needed libraries
//! must be defined by the link or by the libraries' own dependencies, which
//! are found, as GNU ld finds them, through `-rpath-link`, `-rpath`, the
//! libraries' `DT_RUNPATH`/`DT_RPATH`, `LD_LIBRARY_PATH`, the default
//! directories and `-L`.

#![deny(clippy::arithmetic_side_effects)]

use std::path::{Path, PathBuf};

use hashbrown::HashSet;
use rayon::prelude::*;

use crate::args::LinkOptions;
use crate::diag::{Diagnostic, DiagnosticSink};
use crate::elf::read::consts::{
    DT_RPATH, DT_RUNPATH, EM_X86_64, SHN_UNDEF, STB_LOCAL, STB_WEAK, VER_NDX_GLOBAL, VER_NDX_LOCAL,
};
use crate::elf::read::{Elf64Le, SharedObject, Source as ElfSource, VersionKind};
use crate::error::{Error, Result};
use crate::ids::{FileId, SymbolId};
use crate::symbols::{
    Definition, DefinitionKind, Resolution, SymbolFlags, SymbolName, SymbolTable, SymbolUse,
    takes_precedence,
};

use super::inputs::ElfInput;
use super::resolve::ElfRules;

/// Backend flag: a needed shared library references the symbol.
pub const REF_DYNAMIC: SymbolFlags = SymbolFlags::backend(3);
/// Backend flag: a regular object refers to or defines the symbol.
pub const REF_REGULAR: SymbolFlags = SymbolFlags::backend(4);
/// Backend flag: a regular object has a non-weak reference to the symbol.
pub const REF_REGULAR_STRONG: SymbolFlags = SymbolFlags::backend(5);

/// A shared object in the link.
#[derive(Debug)]
pub struct SharedInput<'a> {
    /// The parsed shared object.
    pub elf: SharedObject<'a, Elf64Le>,
    /// The `DT_NEEDED` string for this library: its `DT_SONAME`, or the
    /// name it was found by.
    pub needed_name: Vec<u8>,
    /// `--as-needed` was in effect.
    pub as_needed: bool,
    /// Names offered to resolution (see the module documentation).
    pub names: Vec<SymbolName<'a>>,
    /// How each name takes part in resolution.
    pub uses: Vec<SymbolUse>,
    /// The dynamic symbol index behind each name.
    pub symbols: Vec<u32>,
}

impl<'a> SharedInput<'a> {
    /// Parses the headers of a shared object; symbols are read by
    /// [`load_symbols`](Self::load_symbols).
    ///
    /// `found_as` is the name the library was found by, used as its
    /// `DT_NEEDED` string when it has no `DT_SONAME`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Malformed`] for malformed or non-x86-64 files.
    pub fn parse(
        data: &'a [u8],
        source: ElfSource<'a>,
        found_as: &[u8],
        as_needed: bool,
    ) -> Result<Self> {
        let elf = SharedObject::<Elf64Le>::parse(data, source)?;
        if elf.elf().header().e_machine != EM_X86_64 {
            return Err(source.malformed(18, "ELF machine (incompatible with elf_x86_64)"));
        }
        let needed_name = elf.soname().unwrap_or(found_as).to_vec();
        Ok(Self {
            elf,
            needed_name,
            as_needed,
            names: Vec::new(),
            uses: Vec::new(),
            symbols: Vec::new(),
        })
    }

    /// Reads the dynamic symbols into [`names`](Self::names),
    /// [`uses`](Self::uses) and [`symbols`](Self::symbols).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Malformed`] for malformed symbol or version tables.
    pub fn load_symbols(&mut self) -> Result<()> {
        let symbols = *self.elf.symbols();
        let first = symbols.first_global().max(1);
        let count = symbols.len().saturating_sub(first);
        let mut names = Vec::with_capacity(count.saturating_add(count / 2));
        let mut uses = Vec::with_capacity(names.capacity());
        let mut indices = Vec::with_capacity(names.capacity());
        for index in first..symbols.len() {
            let Some(raw) = symbols.get_raw(index) else {
                break;
            };
            if raw.binding() == STB_LOCAL {
                continue;
            }
            let name = symbols.name(index, &raw)?;
            if name.is_empty() {
                continue;
            }
            let index32 = u32::try_from(index)
                .map_err(|_| Error::Limit("too many dynamic symbols".into()))?;
            if raw.st_shndx == SHN_UNDEF {
                names.push(SymbolName::new(name));
                uses.push(SymbolUse::Reference {
                    weak: raw.binding() == STB_WEAK,
                });
                indices.push(index32);
                continue;
            }
            let version = self.elf.symbol_version(index)?;
            if version.index == VER_NDX_LOCAL {
                continue;
            }
            let definition = SymbolUse::Definition {
                kind: DefinitionKind::Shared,
                aux: 0,
            };
            let versioned = version
                .info
                .filter(|info| version.index > VER_NDX_GLOBAL && !info.is_base())
                .filter(|info| info.kind == VersionKind::Defined)
                .map(|info| info.name);
            if !version.hidden || versioned.is_none() {
                names.push(SymbolName::new(name));
                uses.push(definition);
                indices.push(index32);
            }
            if let Some(version_name) = versioned {
                names.push(SymbolName::with_version(name, Some(version_name)));
                uses.push(definition);
                indices.push(index32);
            }
        }
        self.names = names;
        self.uses = uses;
        self.symbols = indices;
        Ok(())
    }

    /// The `DT_RUNPATH` (or, without one, `DT_RPATH`) of the library.
    #[must_use]
    pub fn search_path(&self) -> Option<&'a [u8]> {
        let mut rpath = None;
        for entry in self.elf.dynamic_entries() {
            match entry.tag {
                DT_RUNPATH => return self.elf.dynamic_string(entry.value).ok(),
                DT_RPATH => rpath = self.elf.dynamic_string(entry.value).ok(),
                _ => {}
            }
        }
        rpath
    }
}

/// Which shared objects are needed, per input file.
#[derive(Debug, Default)]
pub struct Needed {
    /// For each input file, whether it is a shared object that gets a
    /// `DT_NEEDED` entry.
    pub needed: Vec<bool>,
}

impl Needed {
    /// Whether file `index` is a needed shared object.
    #[must_use]
    pub fn is_needed(&self, index: usize) -> bool {
        self.needed.get(index).copied().unwrap_or(false)
    }

    /// Whether the link has any shared object input at all.
    #[must_use]
    pub fn any(&self) -> bool {
        self.needed.iter().any(|&n| n)
    }
}

/// The shared object whose definition a symbol currently has, if any.
fn shared_owner(symbols: &SymbolTable<'_>, id: SymbolId) -> Option<usize> {
    let def = symbols.definition(id);
    (def.kind == DefinitionKind::Shared).then(|| def.file.index())
}

/// Decides which shared objects are needed (see the module documentation),
/// rebinds symbols defined only by unneeded ones, and sets
/// [`REF_REGULAR`] and [`REF_DYNAMIC`].
#[must_use]
pub fn plan_needed(
    files: &[ElfInput<'_>],
    symbols: &SymbolTable<'_>,
    rules: &ElfRules,
    resolution: &Resolution<'_>,
) -> Needed {
    let count = files.len();
    let mut needed: Vec<bool> = files
        .iter()
        .map(|f| f.shared.as_ref().is_some_and(|s| !s.as_needed))
        .collect();

    // Regular objects: references and definitions.
    let from_objects: Vec<Vec<usize>> = files
        .par_iter()
        .enumerate()
        .filter(|(index, file)| file.shared.is_none() && resolution.is_live(FileId::new(*index)))
        .map(|(index, file)| {
            let ids = resolution.symbol_ids(FileId::new(index));
            let mut owners = Vec::new();
            for (local, &id) in ids.iter().enumerate() {
                let use_ = match &file.object {
                    Some(object) => object.uses.get(local).copied(),
                    None => file.internal.uses.get(local).copied(),
                };
                match use_ {
                    Some(SymbolUse::Reference { weak }) => {
                        if weak {
                            symbols.set_flags(id, REF_REGULAR);
                        } else {
                            symbols.set_flags(id, REF_REGULAR | REF_REGULAR_STRONG);
                            if let Some(owner) = shared_owner(symbols, id) {
                                owners.push(owner);
                            }
                        }
                    }
                    Some(SymbolUse::Definition { .. }) => {
                        symbols.set_flags(id, REF_REGULAR);
                    }
                    _ => {}
                }
            }
            owners.sort_unstable();
            owners.dedup();
            owners
        })
        .collect();
    for owner in from_objects.into_iter().flatten() {
        if let Some(slot) = needed.get_mut(owner) {
            *slot = true;
        }
    }

    // Needed libraries' references, to a fixpoint. A reference does not make
    // a library needed when the referring library lists it in DT_NEEDED
    // itself.
    let mut done = vec![false; count];
    loop {
        let round: Vec<usize> = (0..count)
            .filter(|&i| needed.get(i) == Some(&true) && done.get(i) == Some(&false))
            .collect();
        if round.is_empty() {
            break;
        }
        let found: Vec<Vec<usize>> = round
            .par_iter()
            .map(|&index| {
                let Some(shared) = files.get(index).and_then(|f| f.shared.as_ref()) else {
                    return Vec::new();
                };
                let own_needed: Vec<&[u8]> = shared.elf.needed().filter_map(|n| n.ok()).collect();
                let ids = resolution.symbol_ids(FileId::new(index));
                let mut owners = Vec::new();
                for (local, &id) in ids.iter().enumerate() {
                    if shared.uses.get(local) != Some(&SymbolUse::Reference { weak: false }) {
                        continue;
                    }
                    let Some(owner) = shared_owner(symbols, id) else {
                        continue;
                    };
                    let listed = files
                        .get(owner)
                        .and_then(|f| f.shared.as_ref())
                        .is_some_and(|o| own_needed.contains(&o.needed_name.as_slice()));
                    if !listed {
                        owners.push(owner);
                    }
                }
                owners.sort_unstable();
                owners.dedup();
                owners
            })
            .collect();
        for &index in &round {
            if let Some(slot) = done.get_mut(index) {
                *slot = true;
            }
        }
        for owner in found.into_iter().flatten() {
            if let Some(slot) = needed.get_mut(owner) {
                *slot = true;
            }
        }
    }

    rebind_unneeded(files, symbols, rules, resolution, &needed);

    // References from needed libraries.
    files
        .par_iter()
        .enumerate()
        .filter(|(index, _)| needed.get(*index) == Some(&true))
        .for_each(|(index, file)| {
            let Some(shared) = &file.shared else {
                return;
            };
            let ids = resolution.symbol_ids(FileId::new(index));
            for (local, &id) in ids.iter().enumerate() {
                if let Some(SymbolUse::Reference { .. }) = shared.uses.get(local) {
                    symbols.set_flags(id, REF_DYNAMIC);
                }
            }
        });

    Needed { needed }
}

/// Binds symbols whose best definition after resolution is an unextracted
/// archive member to their earliest shared library definition, if any.
///
/// [`ElfRules`] lets a member of an archive that comes before a shared
/// library win, so that a non-weak reference extracts it as in GNU ld. A
/// symbol with only weak references (or none) keeps the lazy definition,
/// which would leave it undefined; GNU ld binds it to the shared library,
/// and so does this. Returns the number of rebound symbols.
pub fn bind_unextracted(
    files: &[ElfInput<'_>],
    symbols: &SymbolTable<'_>,
    resolution: &Resolution<'_>,
) -> usize {
    let mut candidates: Vec<(SymbolId, Definition)> = files
        .par_iter()
        .enumerate()
        .filter(|(index, file)| file.shared.is_some() && resolution.is_live(FileId::new(*index)))
        .flat_map_iter(|(index, file)| {
            let ids = resolution.symbol_ids(FileId::new(index));
            let uses = file.shared.as_ref().map_or(&[][..], |s| s.uses.as_slice());
            ids.iter()
                .zip(uses)
                .enumerate()
                .filter_map(move |(local, (&id, use_))| {
                    let SymbolUse::Definition { kind, aux } = *use_ else {
                        return None;
                    };
                    if symbols.definition_kind(id) != DefinitionKind::Lazy {
                        return None;
                    }
                    Some((
                        id,
                        Definition {
                            kind,
                            file: FileId::new(index),
                            index: u32::try_from(local).ok()?,
                            position: file.position,
                            aux,
                        },
                    ))
                })
        })
        .collect();
    candidates.sort_unstable_by_key(|(id, def)| (*id, def.tie_key()));
    candidates.dedup_by_key(|(id, _)| *id);
    for (id, def) in &candidates {
        symbols.replace_definition(*id, def);
    }
    candidates.len()
}

/// Binds symbols whose definition is in an unneeded shared object to the
/// best definition in a needed one, or leaves them undefined.
fn rebind_unneeded(
    files: &[ElfInput<'_>],
    symbols: &SymbolTable<'_>,
    rules: &ElfRules,
    resolution: &Resolution<'_>,
    needed: &[bool],
) {
    let unneeded = |file: usize| {
        files.get(file).is_some_and(|f| f.shared.is_some()) && needed.get(file) != Some(&true)
    };
    if !(0..files.len()).any(unneeded) {
        return;
    }
    let affected: Vec<SymbolId> = symbols
        .ids()
        .collect::<Vec<_>>()
        .into_par_iter()
        .filter(|&id| shared_owner(symbols, id).is_some_and(unneeded))
        .collect();
    if affected.is_empty() {
        return;
    }
    let mut marks = vec![false; symbols.len()];
    for id in &affected {
        if let Some(mark) = marks.get_mut(id.index()) {
            *mark = true;
        }
    }
    let mut candidates: Vec<(SymbolId, Definition)> = files
        .par_iter()
        .enumerate()
        .filter(|(index, file)| file.shared.is_some() && needed.get(*index) == Some(&true))
        .flat_map_iter(|(index, file)| {
            let marks = &marks;
            let ids = resolution.symbol_ids(FileId::new(index));
            let uses = file.shared.as_ref().map_or(&[][..], |s| s.uses.as_slice());
            ids.iter()
                .zip(uses)
                .enumerate()
                .filter_map(move |(local, (&id, use_))| {
                    if !marks.get(id.index()).copied().unwrap_or(false) {
                        return None;
                    }
                    let SymbolUse::Definition { kind, aux } = *use_ else {
                        return None;
                    };
                    Some((
                        id,
                        Definition {
                            kind,
                            file: FileId::new(index),
                            index: u32::try_from(local).ok()?,
                            position: file.position,
                            aux,
                        },
                    ))
                })
        })
        .collect();
    candidates.sort_unstable_by_key(|(id, def)| (*id, def.tie_key()));
    let mut best: Vec<(SymbolId, Definition)> = Vec::new();
    for (id, def) in candidates {
        match best.last_mut() {
            Some((last, current)) if *last == id => {
                if takes_precedence(rules, &def, current) {
                    *current = def;
                }
            }
            _ => best.push((id, def)),
        }
    }
    for id in affected {
        let replacement = best
            .binary_search_by_key(&id, |(i, _)| *i)
            .ok()
            .and_then(|at| best.get(at))
            .map_or_else(Definition::undefined, |(_, def)| *def);
        symbols.replace_definition(id, &replacement);
    }
}

/// The directories searched for a library's dependencies, in GNU ld's
/// order, except the library's own run path, which is added per library.
fn dependency_dirs(options: &LinkOptions) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = options
        .rpath_links
        .iter()
        .map(|p| options.resolve_sysroot(p))
        .collect();
    dirs.extend(options.rpaths.iter().cloned());
    if options.rpaths.is_empty()
        && let Some(run_path) = std::env::var_os("LD_RUN_PATH")
    {
        dirs.extend(std::env::split_paths(&run_path));
    }
    if let Some(library_path) = std::env::var_os("LD_LIBRARY_PATH") {
        dirs.extend(std::env::split_paths(&library_path));
    }
    for default in ["/lib64", "/usr/lib64", "/lib", "/usr/lib"] {
        dirs.push(options.resolve_sysroot(Path::new(&format!("={default}"))));
    }
    dirs.extend(
        options
            .search_paths
            .iter()
            .map(|p| options.resolve_sysroot(p)),
    );
    dirs
}

/// Expands `$ORIGIN` in a run path entry.
fn expand_origin(entry: &str, library: &Path) -> PathBuf {
    let origin = library
        .parent()
        .map_or_else(|| ".".to_string(), |p| p.display().to_string());
    PathBuf::from(
        entry
            .replace("${ORIGIN}", &origin)
            .replace("$ORIGIN", &origin),
    )
}

/// A dependency loaded only to check undefined symbols.
struct Dependency {
    data: Vec<u8>,
    path: PathBuf,
    /// The library whose `DT_NEEDED` list named it.
    needed_by: PathBuf,
}

/// Loads the transitive dependencies of the needed libraries that are not
/// in the link. Returns them, and the libraries whose dependencies could
/// not all be found.
fn load_dependencies(
    files: &[ElfInput<'_>],
    needed: &Needed,
    options: &LinkOptions,
    diagnostics: &dyn DiagnosticSink,
) -> (Vec<Dependency>, Vec<usize>) {
    let mut known: HashSet<Vec<u8>, foldhash::fast::FixedState> =
        HashSet::with_hasher(foldhash::fast::FixedState::with_seed(0x6465_7073));
    for file in files {
        if let Some(shared) = &file.shared {
            known.insert(shared.needed_name.clone());
        }
    }
    let base_dirs = dependency_dirs(options);
    let mut loaded: Vec<Dependency> = Vec::new();
    let mut incomplete = Vec::new();
    // (needed name, requesting library path, requesting library run path,
    // requesting input file or usize::MAX for a dependency)
    type Request = (Vec<u8>, PathBuf, Option<Vec<u8>>, usize);
    let mut queue: Vec<Request> = Vec::new();
    for (index, file) in files.iter().enumerate() {
        if !needed.is_needed(index) {
            continue;
        }
        let Some(shared) = &file.shared else {
            continue;
        };
        for name in shared.elf.needed().filter_map(|n| n.ok()) {
            queue.push((
                name.to_vec(),
                file.path(),
                shared.search_path().map(<[u8]>::to_vec),
                index,
            ));
        }
    }
    let mut cursor = 0usize;
    while let Some((name, requester, run_path, owner)) = queue.get(cursor).cloned() {
        cursor = cursor.saturating_add(1);
        if !known.insert(name.clone()) {
            continue;
        }
        let text = String::from_utf8_lossy(&name).into_owned();
        let mut dirs: Vec<PathBuf> = Vec::new();
        if let Some(run_path) = &run_path {
            for entry in String::from_utf8_lossy(run_path).split(':') {
                if !entry.is_empty() {
                    dirs.push(expand_origin(entry, &requester));
                }
            }
        }
        dirs.extend(base_dirs.iter().cloned());
        let found = if text.contains('/') {
            std::fs::read(&text)
                .ok()
                .map(|data| (data, PathBuf::from(&text)))
        } else {
            dirs.iter().find_map(|dir| {
                let path = dir.join(&text);
                let data = std::fs::read(&path).ok()?;
                let ok = SharedObject::<Elf64Le>::parse(&data, ElfSource::new(&path))
                    .is_ok_and(|so| so.elf().header().e_machine == EM_X86_64);
                ok.then_some((data, path))
            })
        };
        let Some((data, path)) = found else {
            diagnostics.emit(Diagnostic::warning(format!(
                "{text}, needed by {}, not found (try using -rpath or -rpath-link)",
                requester.display()
            )));
            if owner != usize::MAX {
                incomplete.push(owner);
            }
            continue;
        };
        if let Ok(so) = SharedObject::<Elf64Le>::parse(&data, ElfSource::new(&path)) {
            let run_path = SharedInputView(&so).search_path().map(<[u8]>::to_vec);
            for dependency in so.needed().filter_map(|n| n.ok()) {
                queue.push((dependency.to_vec(), path.clone(), run_path.clone(), owner));
            }
        }
        loaded.push(Dependency {
            data,
            path,
            needed_by: requester,
        });
    }
    incomplete.sort_unstable();
    incomplete.dedup();
    (loaded, incomplete)
}

/// For each of `names` (undefined symbols of the link), a dependency of the
/// needed libraries that is not itself in the link and defines it, with the
/// library that needs it: GNU ld's "DSO missing from command line" case.
#[must_use]
pub fn defined_in_dependencies(
    files: &[ElfInput<'_>],
    needed: &Needed,
    options: &LinkOptions,
    names: &[&[u8]],
) -> Vec<Option<(PathBuf, PathBuf)>> {
    let mut found = vec![None; names.len()];
    if names.is_empty() || !needed.any() {
        return found;
    }
    // Missing dependencies were already reported by the shared library
    // check, or will be.
    let silent = crate::diag::Collect::new();
    let (dependencies, _) = load_dependencies(files, needed, options, &silent);
    let mut wanted: Vec<(&[u8], usize)> = names
        .iter()
        .enumerate()
        .map(|(index, &name)| (name, index))
        .collect();
    wanted.sort_unstable();
    for dependency in &dependencies {
        let Ok(so) =
            SharedObject::<Elf64Le>::parse(&dependency.data, ElfSource::new(&dependency.path))
        else {
            continue;
        };
        for symbol in so.symbols().iter().flatten() {
            if symbol.is_undefined() || symbol.is_local() {
                continue;
            }
            let start = wanted.partition_point(|(name, _)| *name < symbol.name);
            for &(name, index) in wanted.get(start..).unwrap_or_default() {
                if name != symbol.name {
                    break;
                }
                if let Some(slot) = found.get_mut(index)
                    && slot.is_none()
                {
                    *slot = Some((dependency.path.clone(), dependency.needed_by.clone()));
                }
            }
        }
    }
    found
}

/// Run path lookup on a bare [`SharedObject`].
struct SharedInputView<'s, 'a>(&'s SharedObject<'a, Elf64Le>);

impl<'a> SharedInputView<'_, 'a> {
    fn search_path(&self) -> Option<&'a [u8]> {
        let mut rpath = None;
        for entry in self.0.dynamic_entries() {
            match entry.tag {
                DT_RUNPATH => return self.0.dynamic_string(entry.value).ok(),
                DT_RPATH => rpath = self.0.dynamic_string(entry.value).ok(),
                _ => {}
            }
        }
        rpath
    }
}

/// Reports undefined symbols of needed shared libraries that nothing in the
/// link, nor the libraries' own dependencies, defines. Returns the number
/// of errors.
pub fn check_shlib_undefined(
    files: &[ElfInput<'_>],
    symbols: &SymbolTable<'_>,
    resolution: &Resolution<'_>,
    needed: &Needed,
    options: &LinkOptions,
    diagnostics: &dyn DiagnosticSink,
) -> usize {
    let shared_output = options.kind == crate::args::OutputKind::Shared;
    let check = match options.allow_shlib_undefined {
        Some(allow) => !allow,
        None => !shared_output,
    };
    if !check
        || matches!(
            options.unresolved_symbols,
            Some(
                crate::args::UnresolvedSymbols::IgnoreAll
                    | crate::args::UnresolvedSymbols::IgnoreInSharedLibs
            )
        )
    {
        return 0;
    }
    // (file, name) pairs left undefined.
    let mut missing: Vec<(usize, SymbolId)> = files
        .par_iter()
        .enumerate()
        .filter(|(index, _)| needed.is_needed(*index))
        .flat_map_iter(|(index, file)| {
            let ids = resolution.symbol_ids(FileId::new(index));
            let uses = file.shared.as_ref().map_or(&[][..], |s| s.uses.as_slice());
            ids.iter()
                .zip(uses)
                .filter(|(_, use_)| **use_ == SymbolUse::Reference { weak: false })
                .filter(|(id, _)| {
                    matches!(
                        symbols.definition_kind(**id),
                        DefinitionKind::Undefined | DefinitionKind::Lazy
                    )
                })
                .map(move |(&id, _)| (index, id))
                .collect::<Vec<_>>()
        })
        .collect();
    if missing.is_empty() {
        return 0;
    }
    let (dependencies, incomplete) = load_dependencies(files, needed, options, diagnostics);
    // Symbols of every library in the link, needed or not, and of the
    // dependencies loaded for the check.
    let mut defined: HashSet<&[u8], foldhash::fast::FixedState> =
        HashSet::with_hasher(foldhash::fast::FixedState::with_seed(0x756e_6466));
    let parsed: Vec<SharedObject<'_, Elf64Le>> = dependencies
        .iter()
        .filter_map(|d| SharedObject::<Elf64Le>::parse(&d.data, ElfSource::new(&d.path)).ok())
        .collect();
    let in_link = files
        .iter()
        .filter_map(|f| f.shared.as_ref())
        .map(|s| &s.elf);
    for so in parsed.iter().chain(in_link) {
        for symbol in so.symbols().iter().flatten() {
            if !symbol.is_undefined() && !symbol.is_local() {
                defined.insert(symbol.name);
            }
        }
    }
    missing.retain(|(file, id)| {
        incomplete.binary_search(file).is_err()
            && !defined.contains(symbols.name(*id).bytes())
            && !options
                .ignore_unresolved_symbols
                .iter()
                .any(|s| s.as_bytes() == symbols.name(*id).bytes())
    });
    missing.sort_unstable_by_key(|&(file, id)| (id, file));
    missing.dedup_by_key(|(_, id)| *id);
    let mut errors = 0usize;
    for (file, id) in missing {
        let display = files.get(file).map_or_else(String::new, ElfInput::display);
        let message = format!("undefined reference: {}", symbols.name(id).display());
        let diagnostic = if options.warn_unresolved_symbols {
            Diagnostic::warning(message)
        } else {
            errors = errors.saturating_add(1);
            Diagnostic::error(message)
        };
        diagnostics.emit(
            diagnostic
                .detail(format!(
                    "referenced by {display} (disallowed by --no-allow-shlib-undefined)"
                ))
                .order(files.get(file).map_or(0, |f| f.position.raw())),
        );
    }
    errors
}
