//! What the dynamic linker sees of each global symbol: the output mode,
//! merged visibility, export to `.dynsym`, preemptibility, and version
//! assignment from version scripts.
//!
//! Runs after resolution, before garbage collection and the relocation
//! scan, and records its answers as backend [`SymbolFlags`] so every later
//! stage can read them lock-free:
//!
//! - **Visibility** is the most restrictive `st_other` visibility any
//!   regular object gives the symbol (internal > hidden > protected >
//!   default), as the ELF specification requires. `--exclude-libs` makes
//!   definitions from the named archives hidden.
//! - **Exported** ([`SymbolFlags::EXPORTED`]): a definition in a regular
//!   object (or a linker-defined symbol) that goes into `.dynsym`. In a
//!   shared object, every default or protected definition that no version
//!   script makes local; in an executable, definitions referenced by a
//!   needed shared library, also defined by one (so the library binds to
//!   the executable's copy), named by `--dynamic-list` or
//!   `--export-dynamic-symbol`, or every definition with `--export-dynamic`.
//! - **Preemptible** ([`PREEMPTIBLE`]): references must go through the GOT
//!   or PLT with symbolic dynamic relocations, because the definition can be
//!   replaced at run time. Definitions in shared libraries, undefined weak
//!   symbols of dynamic outputs, and exported default-visibility definitions
//!   of a shared object that `-Bsymbolic` (or a `--dynamic-list`) does not
//!   bind locally.
//!
//! Version scripts (`--version-script`) follow GNU ld: a definition named
//! `name@@VERSION` or `name@VERSION` keeps its version; otherwise the first
//! exact `global:` pattern, then the first exact `local:` pattern, then the
//! first wildcard `global:`, then wildcard `local:` decides.

#![deny(clippy::arithmetic_side_effects)]

use rayon::prelude::*;

use crate::args::{LinkOptions, OutputKind, SymbolicMode};
use crate::elf::read::consts::{
    STB_LOCAL, STB_WEAK, STT_FUNC, STT_GNU_IFUNC, STV_DEFAULT, STV_HIDDEN, STV_INTERNAL,
    STV_PROTECTED, VERSYM_HIDDEN,
};
use crate::error::{Error, Result};
use crate::ids::{FileId, SymbolId};
use crate::script::{Pattern, VersionNode};
use crate::symbols::{DefinitionKind, Resolution, SymbolFlags, SymbolTable, SymbolUse};

use super::defined::{LinkerSymbols, is_hidden};
use super::dso::{Needed, REF_DYNAMIC};
use super::inputs::{ElfInput, InputRole};
use super::refs::LINKER_FILE;

/// Backend flag: references to the symbol can be preempted at run time.
pub const PREEMPTIBLE: SymbolFlags = SymbolFlags::backend(1);
/// Backend flag: some regular object gives the symbol protected visibility.
pub const VIS_PROTECTED: SymbolFlags = SymbolFlags::backend(12);
/// Backend flag: some regular object gives the symbol hidden visibility.
pub const VIS_HIDDEN: SymbolFlags = SymbolFlags::backend(13);
/// Backend flag: some regular object gives the symbol internal visibility.
pub const VIS_INTERNAL: SymbolFlags = SymbolFlags::backend(14);
/// Backend flag: a version script or `--exclude-libs` makes the symbol
/// local to the output.
pub const FORCED_LOCAL: SymbolFlags = SymbolFlags::backend(15);

/// What kind of image the link produces, from the dynamic linker's point of
/// view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mode {
    /// The output kind from the options.
    pub kind: OutputKind,
    /// Position-independent: loaded at an arbitrary base (PIE, shared
    /// object, static PIE).
    pub pic: bool,
    /// The output has a `.dynamic` section.
    pub dynamic: bool,
    /// A shared object.
    pub shared: bool,
    /// The output asks for a program interpreter (`PT_INTERP`).
    pub interp: bool,
}

impl Mode {
    /// Decides the mode. An executable that links no shared object is
    /// static, as with GNU ld.
    #[must_use]
    pub fn new(options: &LinkOptions, has_shared_inputs: bool) -> Self {
        let kind = options.kind;
        let (pic, dynamic, shared) = match kind {
            OutputKind::Pie => (true, true, false),
            OutputKind::Shared => (true, true, true),
            OutputKind::StaticPie => (true, true, false),
            OutputKind::Executable => (false, has_shared_inputs, false),
            OutputKind::StaticExecutable | OutputKind::Relocatable => (false, false, false),
        };
        let interp =
            dynamic && !shared && kind != OutputKind::StaticPie && !options.no_dynamic_linker;
        Self {
            kind,
            pic,
            dynamic,
            shared,
            interp,
        }
    }

    /// Whether the output is an executable (not a shared object).
    #[must_use]
    pub fn executable(&self) -> bool {
        !self.shared
    }
}

/// One version definition of a shared object output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VersionDef {
    /// The version name.
    pub name: Vec<u8>,
    /// Names of the versions it inherits from.
    pub parents: Vec<Vec<u8>>,
}

/// A compiled version script.
#[derive(Debug, Default)]
pub struct VersionScript {
    /// Named version nodes, in script order; index `i` is version index
    /// `i + 2`.
    pub defs: Vec<VersionDef>,
    /// `(exact name, version index or 0 for the anonymous node, local)`,
    /// sorted by name.
    exact: Vec<(Vec<u8>, u16, bool)>,
    /// `(pattern, version index, local)`, in script order.
    globs: Vec<(Pattern, u16, bool)>,
}

impl VersionScript {
    /// Compiles parsed version nodes.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Limit`] for more than 32 000 versions.
    pub fn new(nodes: &[VersionNode]) -> Result<Self> {
        let mut script = Self::default();
        for node in nodes {
            let index = match &node.name {
                Some(name) => {
                    script.defs.push(VersionDef {
                        name: name.clone(),
                        parents: node.dependencies.clone(),
                    });
                    u16::try_from(script.defs.len())
                        .ok()
                        .and_then(|n| n.checked_add(1))
                        .filter(|&n| n < VERSYM_HIDDEN)
                        .ok_or_else(|| Error::Limit("too many symbol versions".into()))?
                }
                None => 0,
            };
            for (patterns, local) in [(&node.globals, false), (&node.locals, true)] {
                for pattern in patterns {
                    let wildcard = !pattern.literal
                        && pattern
                            .pattern
                            .iter()
                            .any(|b| matches!(b, b'*' | b'?' | b'['));
                    if wildcard {
                        script
                            .globs
                            .push((Pattern::file(&pattern.pattern), index, local));
                    } else {
                        script.exact.push((pattern.pattern.clone(), index, local));
                    }
                }
            }
        }
        // Stable: the first node naming a symbol wins.
        script.exact.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(script)
    }

    /// The index of the version named `name`, if the script defines it.
    #[must_use]
    pub fn index_of(&self, name: &[u8]) -> Option<u16> {
        self.defs
            .iter()
            .position(|d| d.name == name)
            .and_then(|p| u16::try_from(p).ok())
            .and_then(|p| p.checked_add(2))
    }

    /// Looks `name` up: `(version index, local)`.
    #[must_use]
    pub fn lookup(&self, name: &[u8]) -> Option<(u16, bool)> {
        let start = self.exact.partition_point(|e| e.0.as_slice() < name);
        let exact = self
            .exact
            .get(start..)
            .unwrap_or_default()
            .iter()
            .take_while(|e| e.0 == name);
        let mut local_exact = None;
        for (_, index, local) in exact {
            if !local {
                return Some((*index, false));
            }
            local_exact.get_or_insert((*index, true));
        }
        if local_exact.is_some() {
            return local_exact;
        }
        let mut local_glob = None;
        for (pattern, index, local) in &self.globs {
            if pattern.matches(name) {
                if !local {
                    return Some((*index, false));
                }
                local_glob.get_or_insert((*index, true));
            }
        }
        local_glob
    }
}

/// The result of export planning.
#[derive(Debug, Default)]
pub struct Exports {
    /// The version script of a shared object output.
    pub script: Option<VersionScript>,
    /// Version index of every symbol (with [`VERSYM_HIDDEN`] for
    /// non-default versions); 0 when none was assigned. Empty when no
    /// symbol has a version.
    pub versions: Vec<u16>,
}

impl Exports {
    /// The version index assigned to `id`.
    #[must_use]
    pub fn version(&self, id: SymbolId) -> u16 {
        self.versions.get(id.index()).copied().unwrap_or(0)
    }
}

/// Reads the `--version-script` and `--dynamic-list` files.
///
/// # Errors
///
/// Returns I/O and script errors.
pub fn read_scripts(options: &LinkOptions) -> Result<(Option<VersionScript>, Vec<Pattern>)> {
    let mut nodes = Vec::new();
    for path in &options.version_scripts {
        let data = std::fs::read(path).map_err(|e| Error::io(path, e))?;
        nodes.extend(
            crate::script::parse_version_script(&data, path)
                .map_err(|e| Error::Script(Box::new(e)))?,
        );
    }
    let script = if nodes.is_empty() {
        None
    } else {
        Some(VersionScript::new(&nodes)?)
    };
    let mut patterns: Vec<Pattern> = options
        .export_dynamic_symbols
        .iter()
        .map(|p| Pattern::file(p.as_bytes()))
        .collect();
    for path in &options.dynamic_lists {
        let data = std::fs::read(path).map_err(|e| Error::io(path, e))?;
        for node in crate::script::parse_version_script(&data, path)
            .map_err(|e| Error::Script(Box::new(e)))?
        {
            patterns.extend(node.globals.iter().map(|p| Pattern::file(&p.pattern)));
        }
    }
    for path in &options.export_dynamic_symbol_lists {
        let data = std::fs::read(path).map_err(|e| Error::io(path, e))?;
        for line in data.split(|&b| b == b'\n') {
            let line = line.trim_ascii();
            if !line.is_empty() && !line.starts_with(b"#") {
                patterns.push(Pattern::file(line));
            }
        }
    }
    Ok((script, patterns))
}

/// The most restrictive visibility recorded in `flags`.
#[must_use]
pub fn merged_visibility(flags: SymbolFlags) -> u8 {
    if flags.contains(VIS_INTERNAL) {
        STV_INTERNAL
    } else if flags.contains(VIS_HIDDEN) || flags.contains(FORCED_LOCAL) {
        STV_HIDDEN
    } else if flags.contains(VIS_PROTECTED) {
        STV_PROTECTED
    } else {
        STV_DEFAULT
    }
}

/// Plans exports and preemptibility; see the [module documentation](self).
///
/// # Errors
///
/// Returns an error when a symbol names a version the script does not
/// define.
#[allow(clippy::too_many_arguments)]
pub fn plan(
    files: &[ElfInput<'_>],
    symbols: &SymbolTable<'_>,
    resolution: &Resolution<'_>,
    needed: &Needed,
    options: &LinkOptions,
    mode: Mode,
    script: Option<VersionScript>,
    dynamic_patterns: &[Pattern],
    linker: &LinkerSymbols,
) -> Result<Exports> {
    // Merged visibility, and definitions also made by needed libraries.
    files
        .par_iter()
        .enumerate()
        .filter(|(index, _)| resolution.is_live(FileId::new(*index)))
        .for_each(|(index, file)| {
            let ids = resolution.symbol_ids(FileId::new(index));
            if let Some(object) = &file.object {
                let table = object.elf.symbols();
                for (local, &id) in ids.iter().enumerate() {
                    let Some(raw) = local
                        .checked_add(object.first_global)
                        .and_then(|i| table.get_raw(i))
                    else {
                        continue;
                    };
                    let flag = match raw.visibility() {
                        STV_PROTECTED => VIS_PROTECTED,
                        STV_HIDDEN => VIS_HIDDEN,
                        STV_INTERNAL => VIS_INTERNAL,
                        _ => continue,
                    };
                    if raw.binding() != STB_LOCAL {
                        symbols.set_flags(id, flag);
                    }
                }
            } else if let Some(shared) = &file.shared
                && mode.dynamic
                && !mode.shared
                && needed.is_needed(index)
            {
                for (local, &id) in ids.iter().enumerate() {
                    if matches!(shared.uses.get(local), Some(SymbolUse::Definition { .. }))
                        && symbols.definition_kind(id) != DefinitionKind::Shared
                    {
                        symbols.set_flags(id, REF_DYNAMIC);
                    }
                }
            }
        });

    let exclude_all = options.exclude_libs.iter().any(|l| l == "ALL");
    let excluded = |file: &ElfInput<'_>| -> bool {
        if options.exclude_libs.is_empty() || file.role != InputRole::Member {
            return false;
        }
        if exclude_all {
            return true;
        }
        let archive = file.path();
        let base = archive
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        options.exclude_libs.contains(&base)
    };

    let any_explicit_version = mode.shared
        && files
            .iter()
            .filter_map(|f| f.object.as_ref())
            .any(|o| o.names.iter().any(|n| n.version().is_some()) || o.has_default_versions);
    let assign_versions = mode.shared && (script.is_some() || any_explicit_version);

    let ids: Vec<SymbolId> = symbols.ids().collect();
    let results: Vec<Result<u16>> = ids
        .par_iter()
        .map(|&id| -> Result<u16> {
            let def = symbols.definition(id);
            let flags = symbols.flags(id);
            let mut visibility = merged_visibility(flags);
            let mut set = SymbolFlags::EMPTY;
            let mut version = 0u16;
            match def.kind {
                DefinitionKind::Shared => {
                    if mode.dynamic {
                        set |= PREEMPTIBLE;
                    }
                }
                DefinitionKind::Undefined | DefinitionKind::Lazy => {
                    let weak_only = !flags.contains(SymbolFlags::REFERENCED);
                    if mode.dynamic
                        && visibility == STV_DEFAULT
                        && (mode.shared || weak_only)
                        && mode.kind != OutputKind::StaticPie
                    {
                        set |= PREEMPTIBLE;
                    }
                }
                DefinitionKind::Regular | DefinitionKind::Weak | DefinitionKind::Common => {
                    if !mode.dynamic {
                        return Ok(0);
                    }
                    let name = symbols.name(id);
                    let file = files.get(def.file.index());
                    let linker_defined = def.file == LINKER_FILE || def.file.index() == 0;
                    let raw = file.and_then(|f| {
                        let object = f.object.as_ref()?;
                        object
                            .elf
                            .symbols()
                            .get_raw((def.index as usize).checked_add(object.first_global)?)
                    });
                    if file.is_some_and(excluded) && !linker_defined {
                        set |= FORCED_LOCAL;
                        visibility = STV_HIDDEN;
                    }
                    // `_GLOBAL_OFFSET_TABLE_`, `_DYNAMIC`, `__ehdr_start` and the
                    // other PROVIDE_HIDDEN symbols are per-module: never exported,
                    // not even with --export-dynamic (as in GNU ld and lld).
                    if def.file == LINKER_FILE
                        && linker
                            .get(def.index)
                            .is_some_and(|(_, value)| is_hidden(value))
                    {
                        visibility = STV_HIDDEN;
                    }
                    // Explicit versions from `name@VERSION` and
                    // `name@@VERSION`.
                    let explicit = file
                        .and_then(|f| f.object.as_ref())
                        .and_then(|o| o.default_version(def.index as usize))
                        .map(|v| (v, false))
                        .or_else(|| name.version().map(|v| (v, true)));
                    if assign_versions && visibility != STV_HIDDEN && visibility != STV_INTERNAL {
                        match (explicit, &script) {
                            (Some((v, hidden)), Some(script)) => {
                                let index = script.index_of(v).ok_or_else(|| {
                                    Error::Option(format!(
                                        "version node not found for symbol {}@{}",
                                        String::from_utf8_lossy(name.bytes()),
                                        String::from_utf8_lossy(v)
                                    ))
                                })?;
                                version = if hidden { index | VERSYM_HIDDEN } else { index };
                            }
                            (Some(_), None) => {}
                            (None, Some(script)) => match script.lookup(name.bytes()) {
                                Some((_, true)) => {
                                    set |= FORCED_LOCAL;
                                    visibility = STV_HIDDEN;
                                }
                                Some((index, false)) => version = index,
                                None => {}
                            },
                            (None, None) => {}
                        }
                    }
                    let local_visibility = matches!(visibility, STV_HIDDEN | STV_INTERNAL);
                    let listed = !dynamic_patterns.is_empty()
                        && name.version().is_none()
                        && dynamic_patterns.iter().any(|p| p.matches(name.bytes()));
                    let exported = !local_visibility
                        && (mode.shared
                            || (options.export_dynamic && mode.kind != OutputKind::StaticPie)
                            || flags.contains(REF_DYNAMIC)
                            || listed);
                    if exported && !(linker_defined && mode.shared) {
                        set |= SymbolFlags::EXPORTED;
                        if mode.shared && visibility == STV_DEFAULT {
                            let kind = raw.map_or(0, |r| r.kind());
                            let function = kind == STT_FUNC || kind == STT_GNU_IFUNC;
                            let weak = raw.is_some_and(|r| r.binding() == STB_WEAK);
                            let symbolic = match options.symbolic {
                                SymbolicMode::None => false,
                                SymbolicMode::All => true,
                                SymbolicMode::Functions => function,
                                SymbolicMode::NonWeak => !weak,
                                SymbolicMode::NonWeakFunctions => function && !weak,
                            };
                            let bound_by_list = !options.dynamic_lists.is_empty() && !listed;
                            let absolute = raw
                                .is_some_and(|r| r.st_shndx == crate::elf::read::consts::SHN_ABS);
                            if !symbolic && !bound_by_list && !absolute {
                                set |= PREEMPTIBLE;
                            }
                        }
                    }
                }
            }
            if !set.is_empty() {
                symbols.set_flags(id, set);
            }
            Ok(version)
        })
        .collect();
    let mut versions = Vec::new();
    if assign_versions {
        versions.reserve(results.len());
    }
    for result in results {
        let version = result?;
        if assign_versions {
            versions.push(version);
        }
    }
    Ok(Exports { script, versions })
}
