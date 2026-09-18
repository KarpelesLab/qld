//! The resolved link: every input, the symbol table, what each symbol
//! resolved to, the global atom numbering, and which atoms are live.

#![deny(clippy::arithmetic_side_effects)]

use rayon::prelude::*;

use crate::args::darwin::{LoadMode, UndefinedTreatment};
use crate::diag::{Diagnostic, DiagnosticSink};
use crate::error::{Error, Result};
use crate::ids::{FileId, SymbolId};
use crate::macho::read::AtomKind;
use crate::macho::read::consts::{
    N_ABS, N_SECT, S_MOD_INIT_FUNC_POINTERS, S_MOD_TERM_FUNC_POINTERS,
};
use crate::symbols::{DefinitionKind, Resolution, SymbolTable, SymbolUse};

use super::config::Config;
use super::inputs::{InputKind, InternalNames, LoadedDylib, MachInput};
use super::object::{LinkObject, NOT_GLOBAL};
use super::resolve::display_name;

/// "None" in `u32` index vectors.
pub const NONE: u32 = u32::MAX;

/// What a symbol resolved to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SymbolDef {
    /// Nothing defines it.
    Undefined,
    /// Defined in an object: `file` and the symbol table index there.
    Object {
        /// File index.
        file: u32,
        /// Symbol table index.
        symbol: u32,
    },
    /// A tentative definition, allocated in `__DATA,__common`.
    Common {
        /// File index of the winning definition.
        file: u32,
        /// Its symbol table index.
        symbol: u32,
        /// Size in bytes.
        size: u64,
        /// Alignment, as a power of two.
        align: u32,
    },
    /// Exported by a dylib.
    Dylib {
        /// Index into the dylib list.
        dylib: u32,
        /// A weak definition.
        weak: bool,
        /// A thread-local variable.
        tlv: bool,
    },
    /// `__mh_execute_header` and friends: the Mach-O header.
    Header,
    /// `___dso_handle`: the Mach-O header too.
    DsoHandle,
    /// Left undefined, looked up by dyld at run time
    /// (`-undefined dynamic_lookup`, `-U`).
    DynamicLookup,
    /// `section$start$SEG$SECT` and the like.
    Boundary {
        /// `true` for `$start$`, `false` for `$end$`.
        start: bool,
        /// Segment name.
        segment: Vec<u8>,
        /// Section name, or `None` for a `segment$` symbol.
        section: Option<Vec<u8>>,
    },
}

/// The resolved link.
pub struct Link<'a> {
    /// The configuration.
    pub config: &'a Config,
    /// Every input; file 0 is the internal file.
    pub files: Vec<MachInput<'a>>,
    /// The global symbol table.
    pub symbols: SymbolTable<'a>,
    /// The resolution outcome.
    pub resolution: Resolution<'a>,
    /// The dylibs.
    pub dylibs: &'a [LoadedDylib],
    /// The internal names (file 0).
    pub internal: &'a InternalNames,
    /// What each symbol resolved to, by [`SymbolId`].
    pub defs: Vec<SymbolDef>,
    /// Whether any live file refers to each symbol without `N_WEAK_REF`.
    pub strong_ref: Vec<bool>,
    /// Weak definitions every copy of which may be hidden automatically
    /// (`N_WEAK_DEF | N_WEAK_REF`, `.weak_def_can_be_hidden`): a final
    /// link makes them private externs, as ld64 does.
    pub auto_hidden: Vec<bool>,
    /// Index of each file's first atom in the global atom numbering.
    pub atom_base: Vec<u32>,
    /// Total number of atoms.
    pub atom_count: usize,
    /// Live atoms (global numbering).
    pub live: Vec<bool>,
    /// With `-dead_strip`, undefined symbols whose report waits for dead
    /// stripping: only those live code reaches are errors, as in ld64.
    pub pending_undefined: Vec<SymbolId>,
    /// Objective-C metadata the writer rewrites ([`super::objc`]).
    pub objc: super::objc::Plan,
}

impl std::fmt::Debug for Link<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Link")
            .field("files", &self.files.len())
            .field("atoms", &self.atom_count)
            .finish_non_exhaustive()
    }
}

/// Parses `section$start$SEG$SECT`, `section$end$SEG$SECT`,
/// `segment$start$SEG` and `segment$end$SEG`.
fn boundary(name: &[u8]) -> Option<SymbolDef> {
    let text = std::str::from_utf8(name).ok()?;
    let (is_section, rest) = if let Some(rest) = text.strip_prefix("section$") {
        (true, rest)
    } else {
        (false, text.strip_prefix("segment$")?)
    };
    let (start, rest) = if let Some(rest) = rest.strip_prefix("start$") {
        (true, rest)
    } else {
        (false, rest.strip_prefix("end$")?)
    };
    if is_section {
        let (segment, section) = rest.split_once('$')?;
        Some(SymbolDef::Boundary {
            start,
            segment: segment.as_bytes().to_vec(),
            section: Some(section.as_bytes().to_vec()),
        })
    } else {
        Some(SymbolDef::Boundary {
            start,
            segment: rest.as_bytes().to_vec(),
            section: None,
        })
    }
}

impl<'a> Link<'a> {
    /// Builds the resolved state from a finished resolution.
    ///
    /// # Errors
    ///
    /// [`Error::Reported`] when undefined symbols were reported, and
    /// [`Error::Limit`] for too many atoms.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: &'a Config,
        options: &crate::args::LinkOptions,
        files: Vec<MachInput<'a>>,
        symbols: SymbolTable<'a>,
        resolution: Resolution<'a>,
        dylibs: &'a [LoadedDylib],
        internal: &'a InternalNames,
        diagnostics: &dyn DiagnosticSink,
    ) -> Result<Self> {
        let mut atom_base = Vec::with_capacity(files.len());
        let mut count = 0u32;
        for file in &files {
            atom_base.push(count);
            if let Some(object) = &file.object
                && resolution.is_live(FileId::new(atom_base.len().saturating_sub(1)))
            {
                let atoms = u32::try_from(object.atoms.atoms().len())
                    .map_err(|_| Error::Limit("too many atoms".into()))?;
                count = count
                    .checked_add(atoms)
                    .ok_or_else(|| Error::Limit("too many atoms".into()))?;
            }
        }
        let mut link = Self {
            config,
            files,
            symbols,
            resolution,
            dylibs,
            internal,
            defs: Vec::new(),
            strong_ref: Vec::new(),
            auto_hidden: Vec::new(),
            atom_base,
            atom_count: usize::try_from(count).unwrap_or(usize::MAX),
            live: Vec::new(),
            pending_undefined: Vec::new(),
            objc: super::objc::Plan::default(),
        };
        link.classify(options, diagnostics)?;
        Ok(link)
    }

    /// The loaded object of file `file`, if it is a live object.
    #[must_use]
    pub fn object(&self, file: usize) -> Option<&LinkObject<'a>> {
        let input = self.files.get(file)?;
        if !self.resolution.is_live(FileId::new(file)) {
            return None;
        }
        input.object.as_deref()
    }

    /// The global index of `atom` in `file`.
    #[must_use]
    pub fn atom_id(&self, file: usize, atom: usize) -> usize {
        let base = self.atom_base.get(file).copied().unwrap_or(0);
        usize::try_from(base)
            .unwrap_or(usize::MAX)
            .saturating_add(atom)
    }

    /// Whether `atom` of `file` is live.
    #[must_use]
    pub fn is_live(&self, file: usize, atom: usize) -> bool {
        self.live
            .get(self.atom_id(file, atom))
            .copied()
            .unwrap_or(false)
    }

    fn classify(
        &mut self,
        options: &crate::args::LinkOptions,
        diagnostics: &dyn DiagnosticSink,
    ) -> Result<()> {
        let count = self.symbols.len();
        let mut defs = vec![SymbolDef::Undefined; count];
        for (index, def) in defs.iter_mut().enumerate() {
            let id = SymbolId::new(index);
            let definition = self.symbols.definition(id);
            let file_index = definition.file.index();
            let file = self.files.get(file_index);
            *def = match (definition.kind, file.map(|f| f.kind)) {
                (DefinitionKind::Shared, Some(InputKind::Dylib(dylib))) => {
                    let export = self
                        .dylibs
                        .get(dylib)
                        .and_then(|d| d.exports.get(usize::try_from(definition.index).ok()?));
                    SymbolDef::Dylib {
                        dylib: u32::try_from(dylib).unwrap_or(NONE),
                        weak: export.is_some_and(|e| e.weak),
                        tlv: export.is_some_and(|e| e.tlv),
                    }
                }
                (DefinitionKind::Regular | DefinitionKind::Weak, Some(InputKind::Internal)) => {
                    let name = self
                        .internal
                        .names
                        .get(usize::try_from(definition.index).unwrap_or(usize::MAX))
                        .map(|(n, _)| n.as_slice());
                    if name == Some(b"___dso_handle") {
                        SymbolDef::DsoHandle
                    } else if name.is_some_and(|n| self.internal.aliases.iter().any(|a| a.0 == n)) {
                        // Filled in below, from the target.
                        SymbolDef::Undefined
                    } else {
                        SymbolDef::Header
                    }
                }
                (
                    DefinitionKind::Regular | DefinitionKind::Weak | DefinitionKind::Common,
                    Some(InputKind::Object),
                ) => {
                    let Some(object) = file.and_then(|f| f.object.as_deref()) else {
                        continue;
                    };
                    let symbol = object
                        .global_symbols
                        .get(usize::try_from(definition.index).unwrap_or(usize::MAX))
                        .copied()
                        .unwrap_or(NONE);
                    let file = u32::try_from(file_index).unwrap_or(NONE);
                    if definition.kind == DefinitionKind::Common {
                        let align = object
                            .file
                            .symbols()
                            .get(symbol)
                            .map_or(0, |s| u32::from(s.common_align()));
                        SymbolDef::Common {
                            file,
                            symbol,
                            size: definition.aux,
                            align,
                        }
                    } else {
                        SymbolDef::Object { file, symbol }
                    }
                }
                _ => SymbolDef::Undefined,
            };
        }

        // `-alias`: the alias resolves to what its target resolved to.
        for (alias, target) in &self.internal.aliases {
            let lookup = |name: &[u8]| self.symbols.lookup(&crate::symbols::SymbolName::new(name));
            let (Some(alias_id), Some(target_id)) = (lookup(alias), lookup(target)) else {
                continue;
            };
            let def = match defs.get(target_id.index()) {
                Some(def @ SymbolDef::Object { .. }) => def.clone(),
                _ => {
                    return Err(Error::Option(format!(
                        "-alias: {} is not defined in an object",
                        display_name(target, options.demangle)
                    )));
                }
            };
            if let Some(slot) = defs.get_mut(alias_id.index()) {
                *slot = def;
            }
        }

        // References: strong and weak.
        let mut strong_ref = vec![false; count];
        let mut referenced = vec![false; count];
        for (index, file) in self.files.iter().enumerate() {
            if !self.resolution.is_live(FileId::new(index)) {
                continue;
            }
            let uses: &[SymbolUse] = match &file.object {
                Some(object) => &object.uses,
                None => &file.uses,
            };
            let ids = self.resolution.symbol_ids(FileId::new(index));
            for (use_, id) in uses.iter().zip(ids) {
                if let SymbolUse::Reference { weak } = use_ {
                    if let Some(slot) = referenced.get_mut(id.index()) {
                        *slot = true;
                    }
                    if !weak && let Some(slot) = strong_ref.get_mut(id.index()) {
                        *slot = true;
                    }
                }
            }
        }

        // Undefined symbols. A relocatable object keeps them undefined for
        // the final link, boundary symbols included.
        let darwin = &options.darwin;
        let mut errors = 0usize;
        let mut pending = Vec::new();
        for (index, def) in defs.iter_mut().enumerate() {
            if *def != SymbolDef::Undefined
                || !referenced.get(index).copied().unwrap_or(false)
                || self.config.is_relocatable()
            {
                continue;
            }
            let id = SymbolId::new(index);
            let name = self.symbols.name(id).bytes();
            if let Some(boundary) = boundary(name) {
                *def = boundary;
                continue;
            }
            let allowed = darwin
                .dynamic_lookup_symbols
                .iter()
                .any(|s| s.as_bytes() == name);
            let treatment = if allowed {
                UndefinedTreatment::Suppress
            } else {
                self.config.undefined
            };
            let internal = self.referenced_internally(id);
            match treatment {
                UndefinedTreatment::Error if self.config.dead_strip && !internal => {
                    pending.push(id);
                }
                UndefinedTreatment::Error => {
                    let shown = display_name(name, options.demangle);
                    let mut diagnostic = Diagnostic::error(format!("undefined symbol: {shown}"));
                    for file in self.referencing_files(id).into_iter().take(3) {
                        diagnostic = diagnostic.detail(format!("referenced by {file}"));
                    }
                    diagnostics.emit(diagnostic);
                    errors = errors.saturating_add(1);
                }
                UndefinedTreatment::Warning => {
                    let shown = display_name(name, options.demangle);
                    diagnostics.emit(Diagnostic::warning(format!("undefined symbol: {shown}")));
                    *def = SymbolDef::DynamicLookup;
                }
                UndefinedTreatment::Suppress | UndefinedTreatment::DynamicLookup => {
                    *def = SymbolDef::DynamicLookup;
                }
            }
        }
        self.defs = defs;
        self.strong_ref = strong_ref;
        self.pending_undefined = pending;
        self.auto_hidden = self.auto_hidden_definitions();
        if errors > 0 && !options.noinhibit_exec {
            return Err(Error::Reported { errors });
        }
        Ok(())
    }

    /// Which symbols have only definitions that can be hidden
    /// automatically. A relocatable object keeps the marks for the final
    /// link instead.
    fn auto_hidden_definitions(&self) -> Vec<bool> {
        let count = self.symbols.len();
        if self.config.is_relocatable() {
            return vec![false; count];
        }
        let mut defined = vec![false; count];
        let mut all = vec![true; count];
        for index in 0..self.files.len() {
            let Some(object) = self.object(index) else {
                continue;
            };
            let ids = self.resolution.symbol_ids(FileId::new(index));
            for (&symbol, id) in object.global_symbols.iter().zip(ids) {
                let Ok(entry) = object.file.symbols().get(symbol) else {
                    continue;
                };
                if !entry.is_defined() && !entry.is_common() {
                    continue;
                }
                if let Some(slot) = defined.get_mut(id.index()) {
                    *slot = true;
                }
                if !(entry.is_weak_def() && entry.is_weak_ref())
                    && let Some(slot) = all.get_mut(id.index())
                {
                    *slot = false;
                }
            }
        }
        defined.iter().zip(all).map(|(&d, a)| d && a).collect()
    }

    /// Whether the definition of `id` (symbol table entry `entry` of file
    /// `file`) stays out of the exports: a private extern, a member of a
    /// `-hidden-l` archive, or an automatically hidden weak definition.
    #[must_use]
    pub fn is_hidden(
        &self,
        id: SymbolId,
        file: usize,
        entry: &crate::macho::read::Symbol<'_>,
    ) -> bool {
        entry.is_private_external()
            || self.files.get(file).is_some_and(|f| f.hidden)
            || self.auto_hidden.get(id.index()).copied().unwrap_or(false)
    }

    /// Whether the linker's internal file refers to `id` (the entry point,
    /// `-u`, an `-alias` target): such references are never dead.
    fn referenced_internally(&self, id: SymbolId) -> bool {
        let Some(file) = self.files.first() else {
            return false;
        };
        let ids = self.resolution.symbol_ids(FileId::new(0));
        file.uses
            .iter()
            .zip(ids)
            .any(|(u, i)| *i == id && matches!(u, SymbolUse::Reference { .. }))
    }

    /// Reports the undefined symbols [`Link::mark_live`] left pending that
    /// live code refers to, with the files that refer to them. The others
    /// stay undefined and unused.
    ///
    /// # Errors
    ///
    /// [`Error::Reported`] when any was reported (unless
    /// `-noinhibit-exec`), and malformed relocations.
    pub fn report_live_undefined(
        &self,
        options: &crate::args::LinkOptions,
        diagnostics: &dyn DiagnosticSink,
    ) -> Result<()> {
        if self.pending_undefined.is_empty() {
            return Ok(());
        }
        let mut pending = vec![false; self.symbols.len()];
        for id in &self.pending_undefined {
            if let Some(slot) = pending.get_mut(id.index()) {
                *slot = true;
            }
        }
        // (symbol, file) pairs of live references, per file.
        let per_file: Vec<Result<Vec<(SymbolId, usize)>>> = (0..self.files.len())
            .into_par_iter()
            .map(|file| {
                let mut out = Vec::new();
                let Some(object) = self.object(file) else {
                    return Ok(out);
                };
                for (section_index, relocations) in object.relocations.iter().enumerate() {
                    let Some(section) = object.file.sections().get(section_index) else {
                        continue;
                    };
                    // `__eh_frame` records are not atoms of their own: a
                    // personality there counts whatever uses it.
                    let eh_frame = section.is(b"__TEXT", b"__eh_frame");
                    if !eh_frame
                        && super::layout::is_consumed(
                            section.segname,
                            section.sectname,
                            section.flags,
                        )
                        && !section.is(b"__LD", b"__compact_unwind")
                    {
                        continue;
                    }
                    let data = object.file.section_data(section_index)?;
                    for relocation in relocations {
                        if !eh_frame && !self.is_live(file, relocation.atom) {
                            continue;
                        }
                        let decoded = super::reloc::decode(
                            self,
                            file,
                            object,
                            section_index,
                            data,
                            &relocation.relocation,
                        )?;
                        for referent in [Some(decoded.referent), decoded.subtrahend]
                            .into_iter()
                            .flatten()
                        {
                            if let super::reloc::Referent::Global(id) = referent
                                && pending.get(id.index()).copied().unwrap_or(false)
                            {
                                out.push((id, file));
                            }
                        }
                    }
                }
                Ok(out)
            })
            .collect();
        let mut referrers: Vec<Vec<usize>> = vec![Vec::new(); self.symbols.len()];
        for result in per_file {
            for (id, file) in result? {
                if let Some(list) = referrers.get_mut(id.index())
                    && list.last() != Some(&file)
                {
                    list.push(file);
                }
            }
        }
        let mut errors = 0usize;
        for &id in &self.pending_undefined {
            let Some(files) = referrers.get(id.index()).filter(|f| !f.is_empty()) else {
                continue;
            };
            let shown = display_name(self.symbols.name(id).bytes(), options.demangle);
            let mut diagnostic = Diagnostic::error(format!("undefined symbol: {shown}"));
            for &file in files.iter().take(3) {
                if let Some(input) = self.files.get(file) {
                    diagnostic = diagnostic.detail(format!("referenced by {}", input.display()));
                }
            }
            diagnostics.emit(diagnostic);
            errors = errors.saturating_add(1);
        }
        if errors > 0 && !options.noinhibit_exec {
            return Err(Error::Reported { errors });
        }
        Ok(())
    }

    fn referencing_files(&self, id: SymbolId) -> Vec<String> {
        let mut out = Vec::new();
        for (index, file) in self.files.iter().enumerate() {
            if !self.resolution.is_live(FileId::new(index)) {
                continue;
            }
            let uses: &[SymbolUse] = match &file.object {
                Some(object) => &object.uses,
                None => &file.uses,
            };
            let ids = self.resolution.symbol_ids(FileId::new(index));
            if uses
                .iter()
                .zip(ids)
                .any(|(u, i)| *i == id && matches!(u, SymbolUse::Reference { .. }))
            {
                out.push(file.display());
            }
        }
        out
    }

    /// The definition of global `global` of object `file`, when that
    /// global's own definition won resolution.
    #[must_use]
    pub fn global_wins(&self, file: usize, global: usize) -> bool {
        let ids = self.resolution.symbol_ids(FileId::new(file));
        let Some(id) = ids.get(global) else {
            return false;
        };
        let definition = self.symbols.definition(*id);
        definition.file.index() == file
            && usize::try_from(definition.index).ok() == Some(global)
            && matches!(
                definition.kind,
                DefinitionKind::Regular | DefinitionKind::Weak
            )
    }

    /// The symbol ID of global `global` of file `file`.
    #[must_use]
    pub fn global_id(&self, file: usize, global: u32) -> Option<SymbolId> {
        self.resolution
            .symbol_ids(FileId::new(file))
            .get(usize::try_from(global).ok()?)
            .copied()
    }

    /// Whether a symbol is bound at run time (a dylib export or a dynamic
    /// lookup).
    #[must_use]
    pub fn is_imported(&self, id: SymbolId) -> bool {
        matches!(
            self.defs.get(id.index()),
            Some(SymbolDef::Dylib { .. } | SymbolDef::DynamicLookup)
        )
    }

    /// The load mode of the dylib defining `id`, if any.
    #[must_use]
    pub fn dylib_mode(&self, id: SymbolId) -> Option<LoadMode> {
        match self.defs.get(id.index()) {
            Some(SymbolDef::Dylib { dylib, .. }) => self
                .dylibs
                .get(usize::try_from(*dylib).ok()?)
                .map(|d| d.mode),
            _ => None,
        }
    }

    /// Computes live atoms: coalesced weak definitions are dropped, and with
    /// `-dead_strip` everything unreachable from the roots.
    ///
    /// # Errors
    ///
    /// Malformed relocations.
    pub fn mark_live(&mut self, options: &crate::args::LinkOptions) -> Result<()> {
        let mut live = vec![!self.config.dead_strip; self.atom_count];
        // Atoms of files that never became live have no global numbers.
        let coalesced = self.coalesced_atoms();
        for &atom in &coalesced {
            if let Some(slot) = live.get_mut(atom) {
                *slot = false;
            }
        }
        // Debug and compact unwind sections are consumed, not copied; they
        // are marked live so that later passes can read them.
        if !self.config.dead_strip {
            self.live = live;
            return Ok(());
        }
        let graph = self.edges()?;
        let mut is_coalesced = vec![false; self.atom_count];
        for &atom in &coalesced {
            if let Some(slot) = is_coalesced.get_mut(atom) {
                *slot = true;
            }
        }
        let mut work: Vec<usize> = Vec::new();
        let mark = |atom: usize, live: &mut Vec<bool>, work: &mut Vec<usize>| {
            if is_coalesced.get(atom).copied().unwrap_or(true) {
                return;
            }
            if let Some(slot) = live.get_mut(atom)
                && !*slot
            {
                *slot = true;
                work.push(atom);
            }
        };
        for root in self.roots(options) {
            mark(root, &mut live, &mut work);
        }
        while let Some(atom) = work.pop() {
            if let Some(targets) = graph.get(atom) {
                for &target in targets {
                    mark(target, &mut live, &mut work);
                }
            }
        }
        self.live = live;
        Ok(())
    }

    /// Atoms whose every global symbol lost resolution to another weak
    /// definition.
    fn coalesced_atoms(&self) -> Vec<usize> {
        let mut out = Vec::new();
        for (file_index, file) in self.files.iter().enumerate() {
            let Some(object) = self.object(file_index) else {
                continue;
            };
            let _ = file;
            for (atom_index, _) in object.atoms.atoms().iter().enumerate() {
                let symbols = object.atoms.atom_symbols(atom_index);
                let mut any_global = false;
                let mut all_lost = true;
                for &symbol in symbols {
                    let global = object
                        .global_of_symbol
                        .get(usize::try_from(symbol).unwrap_or(usize::MAX))
                        .copied()
                        .unwrap_or(NOT_GLOBAL);
                    if global == NOT_GLOBAL {
                        // Assembler-temporary labels (`ltmp0`, `l_…`, as
                        // arm64 assemblers put at section starts) do not
                        // keep a coalesced definition alive, as in ld64.
                        let temporary =
                            object.file.symbols().get(symbol).is_ok_and(|s| {
                                s.name.starts_with(b"l") || s.name.starts_with(b"L")
                            });
                        if !temporary {
                            all_lost = false;
                        }
                        continue;
                    }
                    any_global = true;
                    if self.global_wins(file_index, usize::try_from(global).unwrap_or(usize::MAX)) {
                        all_lost = false;
                    }
                }
                if any_global && all_lost {
                    out.push(self.atom_id(file_index, atom_index));
                }
            }
        }
        out
    }

    /// The atom defining symbol `id`, for objects.
    #[must_use]
    pub fn symbol_atom(&self, id: SymbolId) -> Option<usize> {
        match self.defs.get(id.index())? {
            SymbolDef::Object { file, symbol } => {
                let file = usize::try_from(*file).ok()?;
                let object = self.object(file)?;
                let atom = object.atoms.symbol_atom(*symbol)?;
                Some(self.atom_id(file, atom))
            }
            _ => None,
        }
    }

    /// Reference edges between atoms, including the reverse edges that keep
    /// live-support atoms (compact unwind records, `S_ATTR_LIVE_SUPPORT`)
    /// alive with what they describe.
    fn edges(&self) -> Result<Vec<Vec<usize>>> {
        let per_file: Vec<Result<Vec<(usize, usize)>>> = (0..self.files.len())
            .into_par_iter()
            .map(|file| {
                let mut edges = Vec::new();
                let Some(object) = self.object(file) else {
                    return Ok(edges);
                };
                let atom_of = |place: super::reloc::Place| match place {
                    super::reloc::Place::Atom { atom, .. } => Some(atom),
                    _ => None,
                };
                for (section_index, relocations) in object.relocations.iter().enumerate() {
                    let Some(section) = object.file.sections().get(section_index) else {
                        continue;
                    };
                    if section.is(b"__TEXT", b"__eh_frame") {
                        continue;
                    }
                    let reverse =
                        section.is_live_support() || section.is(b"__LD", b"__compact_unwind");
                    let data = object.file.section_data(section_index)?;
                    for relocation in relocations {
                        let decoded = super::reloc::decode(
                            self,
                            file,
                            object,
                            section_index,
                            data,
                            &relocation.relocation,
                        )?;
                        let from = self.atom_id(file, relocation.atom);
                        let mut push = |target: Option<usize>| {
                            if let Some(to) = target {
                                if reverse {
                                    edges.push((to, from));
                                }
                                edges.push((from, to));
                            }
                        };
                        let target = super::reloc::place(
                            self,
                            file,
                            object,
                            decoded.referent,
                            decoded.addend,
                        )
                        .ok()
                        .and_then(atom_of);
                        push(target);
                        if let Some(subtrahend) = decoded.subtrahend {
                            push(
                                super::reloc::place(self, file, object, subtrahend, 0)
                                    .ok()
                                    .and_then(atom_of),
                            );
                        }
                    }
                }
                // An FDE keeps its function's LSDA and its CIE's personality
                // alive.
                if let Some((section, frame)) = super::eh_frame::parse(object)? {
                    let data = object.file.section_data(section)?;
                    let target = |pointer| match super::eh_frame::pointer_target(
                        self, file, object, section, data, pointer,
                    ) {
                        Ok(Some(super::eh_frame::Target::Place(place))) => atom_of(place),
                        Ok(Some(super::eh_frame::Target::Got(id, _))) => self.symbol_atom(id),
                        _ => None,
                    };
                    for record in &frame.records {
                        let crate::macho::read::EhFrameKind::Fde(fde) = &record.kind else {
                            continue;
                        };
                        let Some(function) = target(&fde.pc_begin) else {
                            continue;
                        };
                        if let Some(lsda) = fde.lsda.as_ref().and_then(target) {
                            edges.push((function, lsda));
                        }
                        let personality =
                            frame
                                .records
                                .get(fde.cie_index)
                                .and_then(|cie| match &cie.kind {
                                    crate::macho::read::EhFrameKind::Cie(cie) => {
                                        cie.personality.as_ref().and_then(target)
                                    }
                                    crate::macho::read::EhFrameKind::Fde(_) => None,
                                });
                        if let Some(personality) = personality {
                            edges.push((function, personality));
                        }
                    }
                }
                Ok(edges)
            })
            .collect();
        let mut graph = vec![Vec::new(); self.atom_count];
        for edges in per_file {
            for (from, to) in edges? {
                if let Some(list) = graph.get_mut(from) {
                    list.push(to);
                }
            }
        }
        Ok(graph)
    }

    fn roots(&self, options: &crate::args::LinkOptions) -> Vec<usize> {
        let mut roots = Vec::new();
        let root_symbol = |id: SymbolId, roots: &mut Vec<usize>| {
            if let Some(atom) = self.symbol_atom(id) {
                roots.push(atom);
            }
        };
        let exports_are_roots = !self.config.is_exec();
        for (index, _) in self.internal.names.iter().enumerate() {
            if let Some(id) = self.global_id(0, u32::try_from(index).unwrap_or(NONE)) {
                root_symbol(id, &mut roots);
            }
        }
        for name in options.darwin.exported_symbols.iter().chain(&options.init) {
            if let Some(id) = self
                .symbols
                .lookup(&crate::symbols::SymbolName::new(name.as_bytes()))
            {
                root_symbol(id, &mut roots);
            }
        }
        for file_index in 0..self.files.len() {
            let Some(object) = self.object(file_index) else {
                continue;
            };
            for (section_index, section) in object.file.sections().iter().enumerate() {
                let kind = section.section_type();
                if (section.is_no_dead_strip()
                    || kind == S_MOD_INIT_FUNC_POINTERS
                    || kind == S_MOD_TERM_FUNC_POINTERS
                    || section.is(b"__DATA", b"__objc_imageinfo"))
                    && let Some(range) = object.atoms.section_range(section_index)
                {
                    for atom in range {
                        roots.push(self.atom_id(file_index, atom));
                    }
                }
            }
            for symbol in object.file.symbols().iter().flatten() {
                if !matches!(symbol.n_type & 0x0e, N_SECT) {
                    continue;
                }
                let global = object
                    .global_of_symbol
                    .get(usize::try_from(symbol.index).unwrap_or(usize::MAX))
                    .copied()
                    .unwrap_or(NOT_GLOBAL);
                let exported = global != NOT_GLOBAL
                    && self
                        .global_id(file_index, global)
                        .is_some_and(|id| !self.is_hidden(id, file_index, &symbol));
                let keep = symbol.is_no_dead_strip()
                    || symbol.is_referenced_dynamically()
                    || (exports_are_roots && exported);
                if !keep {
                    continue;
                }
                if global != NOT_GLOBAL
                    && !self.global_wins(file_index, usize::try_from(global).unwrap_or(usize::MAX))
                {
                    continue;
                }
                if let Some(atom) = object.atoms.symbol_atom(symbol.index) {
                    roots.push(self.atom_id(file_index, atom));
                }
            }
            let _ = N_ABS;
            let _ = AtomKind::Regular;
        }
        roots
    }
}

/// The atom of section `section` containing `offset`, allowing the end of
/// the last atom.
#[must_use]
pub fn atom_containing(object: &LinkObject<'_>, section: usize, offset: u64) -> Option<usize> {
    let range = object.atoms.section_range(section)?;
    let atoms = object.atoms.atoms().get(range.clone())?;
    let position = atoms.partition_point(|atom| atom.offset <= offset);
    let candidate = position.checked_sub(1)?;
    let atom = atoms.get(candidate)?;
    let end = atom.offset.saturating_add(atom.size);
    let last = candidate.checked_add(1) == Some(atoms.len());
    if offset < end || (last && offset == end) || (atom.size == 0 && offset == atom.offset) {
        range.start.checked_add(candidate)
    } else {
        None
    }
}
