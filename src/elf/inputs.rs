//! Input resolution and loading: from `InputSpec`s to the file list that
//! symbol resolution works on.
//!
//! Command-line files are mapped in parallel ([`FileTable::load_all`]).
//! Then a sequential pass, in command-line order, turns each one into link
//! inputs:
//!
//! - an ELF relocatable object becomes one live [`ElfInput`];
//! - an archive becomes one lazy [`ElfInput`] per member, whose lazy names
//!   come from the archive symbol index (or, without one, from parsing the
//!   member); `--whole-archive` members are live from the start;
//! - a text file is parsed as a linker script, and its `INPUT`, `GROUP` and
//!   `LIB` entries are resolved and expanded in place.
//!
//! Every input gets an [`InputPosition`] from this walk, so positions (and
//! [`FileId`]s, which are indices into the list) follow command-line order,
//! with archive members numbered within their archive. File 0 is the
//! linker's own internal file, holding the entry point and `-u` references.

#![deny(clippy::arithmetic_side_effects)]

use std::path::{Path, PathBuf};

use rayon::prelude::*;

use crate::args::{InputAttrs, InputKind, LinkOptions};
use crate::elf::read::consts::{ET_DYN, ET_REL};
use crate::elf::read::{Elf64Le, ElfFormat, ObjectFile, SectionIndex, Source as ElfSource};
use crate::error::{Error, Result};
use crate::ids::FileId;
use crate::input::archive::Member;
use crate::input::identify::FileFormat;
use crate::input::{FileTable, InputFile, LibraryNaming, MemberEntry, SearchContext, Source};
use crate::script::{self, CommandKind, InputName};
use crate::symbols::{DefinitionKind, InputPosition, ResolveFile, SymbolName, SymbolUse};
use crate::target::Target;

use super::dso::SharedInput;
use super::lto::{self, IrKind, IrSymbols};
use super::object::{GccLto, ObjectInput, ParseConfig, WrapTable};

/// What kind of link input an [`ElfInput`] is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputRole {
    /// The linker's internal file (entry point, `-u`, `--defsym`).
    Internal,
    /// An object named on the command line (or in a script).
    Object,
    /// An archive member.
    Member,
    /// A shared object.
    Shared,
}

/// Symbols the linker itself references or defines.
#[derive(Debug, Default)]
pub struct InternalSymbols<'a> {
    /// Names, in order.
    pub names: Vec<SymbolName<'a>>,
    /// How each is used.
    pub uses: Vec<SymbolUse>,
}

impl<'a> InternalSymbols<'a> {
    fn push(&mut self, name: &'a [u8], use_: SymbolUse) {
        self.names.push(SymbolName::new(name));
        self.uses.push(use_);
    }
}

/// How a link treats inputs that carry compiler IR (LTO).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LtoMode {
    /// No `-plugin` was given: IR inputs are errors.
    NoPlugin,
    /// `-plugin` was given, but qld was built without the `plugin` feature.
    Unsupported,
    /// IR inputs are claimed through the plugins as resolution loads them
    /// (the [`super::lto`] module).
    Claim,
    /// The resolution after LTO code generation: IR inputs that were not
    /// part of LTO are errors.
    AfterLto,
    /// An object an LTO plugin generated. GCC IR in it (GCC's incremental
    /// `-r` output is IR again) is linked as a regular object.
    Generated,
}

impl LtoMode {
    /// The mode for a link with `options`.
    #[must_use]
    pub fn for_options(options: &LinkOptions) -> Self {
        if options.plugins.is_empty() {
            Self::NoPlugin
        } else if cfg!(feature = "plugin") {
            Self::Claim
        } else {
            Self::Unsupported
        }
    }
}

/// A member of a thin archive, loaded from disk when it is extracted.
#[derive(Debug)]
struct ThinMember<'a> {
    archive: FileId,
    member: Member<'a>,
}

/// One input as symbol resolution sees it.
#[derive(Debug)]
pub struct ElfInput<'a, F: ElfFormat = Elf64Le> {
    /// Where the input sits on the command line.
    pub position: InputPosition,
    /// What kind of input this is.
    pub role: InputRole,
    /// The loaded file (for thin archive members, once extracted).
    pub file: Option<&'a InputFile>,
    /// Live from the start (objects, `--whole-archive` members).
    pub live_at_start: bool,
    /// Names a lazy member would define.
    pub lazy_names: Vec<SymbolName<'a>>,
    /// The parsed object, once loaded.
    pub object: Option<ObjectInput<'a, F>>,
    /// For the internal file, its symbols.
    pub internal: InternalSymbols<'a>,
    /// For shared objects, the parsed library.
    pub shared: Option<SharedInput<'a, F>>,
    /// For IR inputs an LTO plugin claimed, the symbols it reported.
    pub ir: Option<Box<IrSymbols<'a>>>,
    thin: Option<ThinMember<'a>>,
    table: &'a FileTable,
    config: ParseConfig<'a>,
    lto: LtoMode,
    /// A lazy IR member whose defined names the archive index does not
    /// list: the plugin must claim it before resolution to learn them.
    needs_claim: bool,
}

impl<'a, F: ElfFormat> ElfInput<'a, F> {
    /// A display name for diagnostics: `path` or `path(member)`.
    #[must_use]
    pub fn display(&self) -> String {
        match self.file {
            None if self.role == InputRole::Internal => "<internal>".to_string(),
            None => "<unloaded>".to_string(),
            Some(file) => match file.member() {
                Some(member) => format!("{}({member})", file.path().display()),
                None => file.path().display().to_string(),
            },
        }
    }

    /// The file path (the archive path for members).
    #[must_use]
    pub fn path(&self) -> PathBuf {
        self.file
            .map_or_else(|| PathBuf::from("<internal>"), |f| f.path().to_path_buf())
    }

    /// The archive member name, for members.
    #[must_use]
    pub fn member(&self) -> Option<String> {
        self.file.and_then(|f| f.member()).map(str::to_owned)
    }

    /// The file table the link's inputs live in.
    #[must_use]
    pub fn table(&self) -> &'a FileTable {
        self.table
    }

    /// The `--wrap` table.
    #[must_use]
    pub fn wrap(&self) -> &'a WrapTable {
        self.config.wrap
    }

    /// How this input treats IR.
    #[must_use]
    pub fn lto_mode(&self) -> LtoMode {
        self.lto
    }

    /// Whether this is a lazy IR member the plugins must claim before
    /// resolution, because the archive index does not name its symbols.
    #[must_use]
    pub fn needs_claim(&self) -> bool {
        self.needs_claim && !self.live_at_start
    }

    /// The kind of IR a loaded input carries and no plugin has claimed yet:
    /// LLVM bitcode, or a GCC object with `.gnu.lto_*` sections (slim or
    /// fat). `None` for claimed inputs and ordinary ones.
    #[must_use]
    pub fn pending_ir(&self) -> Option<IrKind> {
        if self.ir.is_some() {
            return None;
        }
        if let Some(object) = &self.object {
            return match object.gcc_lto {
                GccLto::None => None,
                GccLto::Slim => Some(IrKind::GccSlim),
                GccLto::Fat => Some(IrKind::GccFat),
            };
        }
        if self.shared.is_some() {
            return None;
        }
        match self.file?.format() {
            FileFormat::LlvmBitcode(_) => Some(IrKind::LlvmBitcode),
            FileFormat::GccLtoIr(_) => Some(IrKind::GccSlim),
            _ => None,
        }
    }

    /// Records that no plugin claimed this input: a fat GCC object is
    /// linked from its native code; any other IR is an error.
    ///
    /// # Errors
    ///
    /// Returns an error naming the file for IR without native code.
    pub fn claim_declined(&self) -> Result<()> {
        match self.pending_ir() {
            None | Some(IrKind::GccFat) => Ok(()),
            Some(kind) => Err(lto::ir_error(&self.display(), kind, self.lto)),
        }
    }

    /// Learns the defined names of a lazy member that no plugin claimed
    /// from its native symbol table (a fat GCC object).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Malformed`] if the member cannot be parsed.
    pub fn use_native_lazy_names(&mut self) -> Result<()> {
        if let Some(file) = self.file
            && matches!(file.format(), FileFormat::Elf(ident) if ident.is_relocatable())
        {
            self.lazy_names = defined_names::<F>(file)?.0;
        }
        Ok(())
    }

    /// Prepares an input of the first resolution for the resolution after
    /// LTO: inputs that were live stay live (archive extractions are kept),
    /// discarded COMDAT groups stay discarded (as in GNU ld, the first
    /// copy loaded wins even when it was IR), and IR can no longer be
    /// claimed.
    pub fn prepare_after_lto(&mut self, live: bool) {
        self.live_at_start |= live;
        self.ir = None;
        self.lto = LtoMode::AfterLto;
        self.needs_claim = false;
    }

    /// A live input for `file`, an object an LTO plugin produced, placed at
    /// `position`.
    #[must_use]
    pub fn lto_object(&self, file: &'a InputFile, position: InputPosition) -> Self {
        ElfInput {
            position,
            role: InputRole::Object,
            file: Some(file),
            live_at_start: true,
            lazy_names: Vec::new(),
            object: None,
            internal: InternalSymbols::default(),
            shared: None,
            ir: None,
            thin: None,
            table: self.table,
            config: self.config,
            lto: LtoMode::Generated,
            needs_claim: false,
        }
    }
}

impl<'a, F: ElfFormat> ResolveFile<'a> for ElfInput<'a, F> {
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
        if let Some(shared) = &mut self.shared {
            return shared.load_symbols();
        }
        if self.role == InputRole::Internal || self.object.is_some() {
            return Ok(());
        }
        if let Some(thin) = self.thin.take() {
            let id = self.table.add_member(thin.archive, &thin.member)?;
            self.file = self.table.get(id);
        }
        let Some(file) = self.file else {
            return Err(Error::Internal("input file missing at load".into()));
        };
        let source = match file.member() {
            Some(member) => ElfSource::member(file.path(), member),
            None => ElfSource::new(file.path()),
        };
        match file.format() {
            FileFormat::Elf(ident) if ident.is_relocatable() => {}
            FileFormat::Elf(ident) if ident.file_type == ET_DYN => {
                return Err(Error::Unimplemented(format!(
                    "linking against shared object {} (roadmap M2: dynamic ELF)",
                    self.display()
                )));
            }
            format if format.is_ir() => {
                // Claimed once the round's files are loaded, in input order
                // (the round hook in `lto`).
                return match self.lto {
                    LtoMode::Claim => Ok(()),
                    mode => Err(lto::ir_error(&self.display(), IrKind::LlvmBitcode, mode)),
                };
            }
            _ => {
                return Err(source.malformed(0, "archive member (not an ELF relocatable object)"));
            }
        }
        let object = ObjectInput::parse(file.data(), source, &self.config)?;
        if object.gcc_lto == GccLto::Slim
            && !matches!(self.lto, LtoMode::Claim | LtoMode::Generated)
        {
            return Err(lto::ir_error(&self.display(), IrKind::GccSlim, self.lto));
        }
        self.object = Some(object);
        Ok(())
    }

    fn symbol_names(&self) -> &[SymbolName<'a>] {
        if let Some(ir) = &self.ir {
            return &ir.names;
        }
        match (&self.object, &self.shared) {
            (Some(object), _) => &object.names,
            (None, Some(shared)) => &shared.names,
            (None, None) => &self.internal.names,
        }
    }

    fn symbol_use(&self, index: usize) -> SymbolUse {
        let uses = match (&self.ir, &self.object, &self.shared) {
            (Some(ir), _, _) => &ir.uses,
            (None, Some(object), _) => &object.uses,
            (None, None, Some(shared)) => &shared.uses,
            (None, None, None) => &self.internal.uses,
        };
        uses.get(index).copied().unwrap_or(SymbolUse::Ignore)
    }

    /// Members of regular archives: parsing one only fills `object`, and
    /// maybe adds decompressed sections to the file table, which nothing
    /// reads unless the member is extracted. (Loading a thin member adds
    /// the member itself to the table, so it waits for extraction.)
    fn can_load_early(&self) -> bool {
        self.role == InputRole::Member
            && self.thin.is_none()
            && self.object.is_none()
            && self.ir.is_none()
    }

    fn unload(&mut self) {
        self.object = None;
    }
}

/// Everything collected from the command line.
#[derive(Debug)]
pub struct Inputs<'a, F: crate::elf::read::ElfFormat = crate::elf::read::Elf64Le> {
    /// All inputs, file 0 being the internal file.
    pub files: Vec<ElfInput<'a, F>>,
    /// The target: from `-m`, or inferred from the first input that names
    /// one (see [`super::target`]). `None` when nothing did, and
    /// [`super::target::default_target`] applies.
    pub target: Option<Target>,
}

/// Owned strings the internal file borrows.
#[derive(Debug, Default)]
pub struct InternalNames {
    /// Entry point, `-u`, `--require-defined` and `--defsym` names.
    pub names: Vec<(Vec<u8>, SymbolUse)>,
}

impl InternalNames {
    /// Collects the linker-created references and definitions for `options`.
    #[must_use]
    pub fn new(options: &LinkOptions) -> Self {
        let mut names = Vec::new();
        let reference = SymbolUse::Reference { weak: false };
        match &options.entry {
            Some(entry) if parse_number(entry).is_none() => {
                names.push((entry.as_bytes().to_vec(), reference));
            }
            Some(_) => {}
            // Shared objects and relocatable output have no entry point
            // unless one is named.
            None if matches!(
                options.kind,
                crate::args::OutputKind::Shared | crate::args::OutputKind::Relocatable
            ) => {}
            None => names.push((b"_start".to_vec(), reference)),
        }
        for name in options.undefined.iter().chain(&options.require_defined) {
            names.push((name.as_bytes().to_vec(), reference));
        }
        for (name, expr) in &options.defsym {
            for target in super::defined::defsym_references(name, expr) {
                names.push((target, reference));
            }
            names.push((
                name.as_bytes().to_vec(),
                SymbolUse::Definition {
                    kind: DefinitionKind::Regular,
                    aux: 0,
                },
            ));
        }
        Self { names }
    }
}

/// Parses a GNU-style number: decimal, `0x` hex, or octal with a leading 0.
#[must_use]
pub fn parse_number(text: &str) -> Option<u64> {
    let text = text.trim();
    if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        return u64::from_str_radix(hex, 16).ok();
    }
    if let Some(hex) = text.strip_suffix('h').or_else(|| text.strip_suffix('H')) {
        return u64::from_str_radix(hex, 16).ok();
    }
    if text.len() > 1
        && let Some(octal) = text.strip_prefix('0')
    {
        return u64::from_str_radix(octal, 8).ok();
    }
    text.parse().ok()
}

/// A `--defsym` expression qld understands: `number`, `symbol`,
/// `symbol+number` or `symbol-number`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DefsymExpr {
    /// An absolute value.
    Absolute(u64),
    /// A symbol plus an offset.
    Symbol(String, i64),
}

/// Parses a `--defsym` right-hand side.
#[must_use]
pub fn parse_defsym(expr: &str) -> Option<DefsymExpr> {
    let expr = expr.trim();
    if let Some(value) = parse_number(expr) {
        return Some(DefsymExpr::Absolute(value));
    }
    let split = expr.rfind(['+', '-']).filter(|&at| at > 0);
    let (symbol, offset) = match split {
        Some(at) => {
            let (symbol, rest) = expr.split_at(at);
            let negative = rest.starts_with('-');
            let value = i64::try_from(parse_number(rest.get(1..)?)?).ok()?;
            (
                symbol.trim(),
                if negative {
                    value.checked_neg()?
                } else {
                    value
                },
            )
        }
        None => (expr, 0),
    };
    let valid = !symbol.is_empty()
        && symbol
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'$' | b'@'));
    valid.then(|| DefsymExpr::Symbol(symbol.to_string(), offset))
}

/// One entry of the expanded input list, before loading.
struct Pending {
    source: Source,
    attrs: InputAttrs,
    what: String,
    /// The name a shared object found this way is recorded by in
    /// `DT_NEEDED` when it has no `DT_SONAME`.
    found_as: Vec<u8>,
}

/// The `DT_NEEDED` fallback name of a library found by `-l`: its file name.
fn base_name_of(path: &Path) -> Vec<u8> {
    path.file_name()
        .map_or_else(Vec::new, |n| n.as_encoded_bytes().to_vec())
}

/// Resolves, loads and expands every input.
///
/// # Errors
///
/// Returns [`Error::NotFound`] for missing libraries, [`Error::Io`] for files
/// that cannot be read, [`Error::Unimplemented`] for inputs that need a later
/// milestone, and parse errors for malformed archives and scripts.
pub fn collect<'a, F: crate::elf::read::ElfFormat>(
    options: &LinkOptions,
    table: &'a FileTable,
    internal: &'a InternalNames,
    config: ParseConfig<'a>,
) -> Result<Inputs<'a, F>> {
    // The file table looks in `options.input_provider` first.
    let fs = table;
    let search = SearchContext {
        search_paths: &options.search_paths,
        sysroot: options.sysroot.as_deref(),
        naming: LibraryNaming::Elf,
        fs: &fs,
    };

    let mut pending = Vec::with_capacity(options.inputs.len());
    for spec in &options.inputs {
        match &spec.kind {
            InputKind::JustSymbols(path) => {
                return Err(Error::Unimplemented(format!(
                    "--just-symbols={} (roadmap M3)",
                    path.display()
                )));
            }
            InputKind::Script(path) => {
                // `-T`: a script that may drive layout. Input-only scripts
                // are expanded below; anything else waits for M3.
                let source = search.resolve(spec)?;
                pending.push(Pending {
                    source,
                    attrs: spec.attrs,
                    what: format!("-T {}", path.display()),
                    found_as: Vec::new(),
                });
            }
            kind => {
                let source = search.resolve(spec)?;
                let found_as = match (kind, &source) {
                    (InputKind::Library(_), Source::Path(path)) => base_name_of(path),
                    (InputKind::LibraryExact(name), _) => name.as_bytes().to_vec(),
                    (InputKind::File(path), _) => path.as_os_str().as_encoded_bytes().to_vec(),
                    _ => Vec::new(),
                };
                pending.push(Pending {
                    source,
                    attrs: spec.attrs,
                    what: String::new(),
                    found_as,
                });
            }
        }
    }
    let sources: Vec<Source> = pending.iter().map(|p| p.source.clone()).collect();
    let loaded = table.load_all(&sources);
    // The members of the archives on the command line are found and
    // described in parallel (reading every member header is most of the
    // walk); the walk below adds them in order.
    let mut prepared: Vec<Option<Result<Vec<PreparedMember<'a>>>>> = loaded
        .par_iter()
        .map(|id| {
            let file = table.get(*id.as_ref().ok()?)?;
            (file.format() == FileFormat::Archive).then(|| prepare_members(table, file, id))
        })
        .collect();

    let mut files = Vec::new();
    let mut internal_symbols = InternalSymbols::default();
    for (name, use_) in &internal.names {
        internal_symbols.push(name, *use_);
    }
    files.push(ElfInput {
        position: InputPosition::new(0, 0),
        role: InputRole::Internal,
        file: None,
        live_at_start: true,
        lazy_names: Vec::new(),
        object: None,
        internal: internal_symbols,
        shared: None,
        ir: None,
        thin: None,
        table,
        config,
        lto: LtoMode::for_options(options),
        needs_claim: false,
    });

    let mut walker = Walker {
        table,
        search,
        config,
        files,
        ordinal: 0,
        target: options.target,
        depth: 0,
        sonames: Vec::new(),
        static_output: matches!(
            options.kind,
            crate::args::OutputKind::StaticExecutable | crate::args::OutputKind::StaticPie
        ),
        lto: LtoMode::for_options(options),
        deferred: Vec::new(),
    };
    for ((entry, id), members) in pending.iter().zip(loaded).zip(prepared.iter_mut()) {
        // An archive index read later still reports its error before this
        // input's.
        let added = id.and_then(|id| {
            walker.add(
                id,
                entry.attrs,
                &entry.what,
                &entry.found_as,
                members.take(),
            )
        });
        if let Err(error) = added {
            walker.finish()?;
            return Err(error);
        }
    }
    walker.finish()?;
    let target = walker.target.unwrap_or_else(super::target::default_target);
    if super::arch::Arch::from_target(target).is_none() {
        return Err(Error::Unimplemented(format!(
            "linking for {:?} (roadmap M4: more ELF architectures)",
            target.arch
        )));
    }
    Ok(Inputs {
        files: walker.files,
        target: walker.target,
    })
}

struct Walker<'a, 's, F: crate::elf::read::ElfFormat = crate::elf::read::Elf64Le> {
    table: &'a FileTable,
    search: SearchContext<'s>,
    config: ParseConfig<'a>,
    files: Vec<ElfInput<'a, F>>,
    ordinal: u32,
    target: Option<Target>,
    depth: u32,
    /// `DT_NEEDED` names of the shared objects added so far.
    sonames: Vec<Vec<u8>>,
    /// The output is a static executable or static PIE.
    static_output: bool,
    /// How new inputs treat IR.
    lto: LtoMode,
    /// Archives whose symbol index is read by [`Walker::finish`].
    deferred: Vec<DeferredIndex<'a>>,
}

/// An archive member found before the walk: the member and its file table
/// entry, not added yet.
struct PreparedMember<'a> {
    member: Member<'a>,
    entry: MemberEntry,
}

/// Finds the members of the (regular) archive `file`, whose ID is `id`, and
/// describes each as a file table entry, as [`Walker`] would one by one.
fn prepare_members<'a>(
    table: &FileTable,
    file: &'a InputFile,
    id: &Result<FileId>,
) -> Result<Vec<PreparedMember<'a>>> {
    let id = *id
        .as_ref()
        .map_err(|_| Error::Internal("archive not loaded".into()))?;
    let archive = file.archive()?;
    archive
        .members()
        .map(|member| {
            let member = member?;
            let entry = table.member_entry(id, &member)?;
            Ok(PreparedMember { member, entry })
        })
        .collect()
}

/// An indexed archive whose symbol index is read after the walk, in
/// parallel with the other archives' ([`Walker::finish`]).
struct DeferredIndex<'a> {
    file: &'a InputFile,
    /// Index in `files` of the archive's first member.
    first: usize,
    /// Member header offset and index in `files`, for every member, sorted.
    by_offset: Vec<(u64, usize)>,
}

impl<'a, F: crate::elf::read::ElfFormat> Walker<'a, '_, F> {
    /// Reads the symbol indexes of the archives added so far, in parallel:
    /// each gives its members their lazy names (and, with a plugin, marks
    /// the IR members the index does not describe). On failure, returns the
    /// error of the first such archive in input order.
    fn finish(&mut self) -> Result<()> {
        let deferred = std::mem::take(&mut self.deferred);
        if deferred.is_empty() {
            return Ok(());
        }
        let mut slices: Vec<&mut [ElfInput<'a, F>]> = Vec::with_capacity(deferred.len());
        let mut rest: &mut [ElfInput<'a, F>] = &mut self.files;
        let mut consumed = 0usize;
        let layout = || Error::Internal("archive members out of order".into());
        for archive in &deferred {
            let skip = archive.first.checked_sub(consumed).ok_or_else(layout)?;
            let count = archive.by_offset.len();
            if skip.checked_add(count).is_none_or(|end| end > rest.len()) {
                return Err(layout());
            }
            let (_, tail) = std::mem::take(&mut rest).split_at_mut(skip);
            let (members, tail) = tail.split_at_mut(count);
            slices.push(members);
            rest = tail;
            consumed = archive.first.saturating_add(count);
        }
        let lto = self.lto;
        let results: Vec<Result<()>> = deferred
            .par_iter()
            .zip(slices)
            .map(|(archive, members)| read_symbol_index(archive, members, lto))
            .collect();
        results.into_iter().collect()
    }

    fn next_position(&mut self) -> Result<u32> {
        self.ordinal = self
            .ordinal
            .checked_add(1)
            .ok_or_else(|| Error::Limit("too many inputs".into()))?;
        Ok(self.ordinal)
    }

    fn input(&self, position: InputPosition, role: InputRole) -> ElfInput<'a, F> {
        ElfInput {
            position,
            role,
            file: None,
            live_at_start: false,
            lazy_names: Vec::new(),
            object: None,
            internal: InternalSymbols::default(),
            shared: None,
            ir: None,
            thin: None,
            table: self.table,
            config: self.config,
            lto: self.lto,
            needs_claim: false,
        }
    }

    /// Takes the target from `file` if none is known yet: from the ELF
    /// header of an object, a shared library or a GCC LTO object, or from
    /// the triple of LLVM bitcode.
    fn infer_target(&mut self, file: &InputFile) {
        if self.target.is_some() {
            return;
        }
        self.target = match file.format() {
            FileFormat::Elf(ident) | FileFormat::GccLtoIr(ident) => {
                ident.architecture().map(|arch| {
                    let mut target = Target::X86_64_LINUX;
                    target.arch = arch;
                    target.endian = ident.endian;
                    target.pointer_width = ident.class;
                    target
                })
            }
            FileFormat::LlvmBitcode(_) => super::target::of_bitcode(file.data()),
            _ => None,
        };
    }

    /// Rejects an ELF object built for another machine, class or byte order
    /// than the target, as GNU `ld` does, instead of linking its
    /// relocations as the target's (an x32 object read as i386).
    fn check_machine(&self, file: &InputFile) -> Result<()> {
        let (FileFormat::Elf(ident) | FileFormat::GccLtoIr(ident), Some(target)) =
            (file.format(), self.target)
        else {
            return Ok(());
        };
        if ident.machine == crate::elf::read::consts::EM_NONE {
            // Machine-neutral: `-b binary` inputs (`binary_input`).
            return Ok(());
        }
        let found = ident.architecture();
        if found == Some(target.arch) && ident.endian == target.endian {
            return Ok(());
        }
        let found = found.map_or_else(
            || format!("machine {:#x}", ident.machine),
            |arch| format!("{arch:?}"),
        );
        Err(Error::Option(format!(
            "{}: {found} ({:?}-endian) architecture of input file is incompatible with {:?} ({:?}-endian) output",
            file.path().display(),
            ident.endian,
            target.arch,
            target.endian,
        )))
    }

    /// Adds input `id`. `prepared` holds its members if it is an archive
    /// whose members were found before the walk ([`prepare_members`]).
    fn add(
        &mut self,
        id: FileId,
        attrs: InputAttrs,
        what: &str,
        found_as: &[u8],
        prepared: Option<Result<Vec<PreparedMember<'a>>>>,
    ) -> Result<()> {
        let Some(file) = self.table.get(id) else {
            return Err(Error::Internal("loaded file missing from table".into()));
        };
        match file.format() {
            FileFormat::Elf(ident) if ident.file_type == ET_REL => {
                self.infer_target(file);
                self.check_machine(file)?;
                let input_number = self.next_position()?;
                let mut input = self.input(InputPosition::new(input_number, 0), InputRole::Object);
                input.file = Some(file);
                if attrs.lazy {
                    let (names, gcc_lto) = defined_names::<F>(file)?;
                    if gcc_lto && self.lto == LtoMode::Claim {
                        // IR names come from the plugin: link it eagerly.
                        input.live_at_start = true;
                    } else {
                        input.lazy_names = names;
                    }
                } else {
                    input.live_at_start = true;
                }
                self.files.push(input);
                Ok(())
            }
            FileFormat::Elf(ident) if ident.file_type == ET_DYN => {
                self.add_shared(file, attrs, found_as)
            }
            FileFormat::Elf(_) => Err(Error::malformed(
                file.path(),
                16,
                "ELF file type (expected a relocatable object)",
            )),
            FileFormat::Archive | FileFormat::ThinArchive => {
                self.add_archive(id, file, attrs, prepared)
            }
            FileFormat::Text(_) => self.add_script(file, attrs, what),
            FileFormat::Empty => Ok(()),
            format if format.is_ir() => {
                if self.lto != LtoMode::Claim {
                    return Err(lto::ir_error(
                        &file.path().display().to_string(),
                        IrKind::LlvmBitcode,
                        self.lto,
                    ));
                }
                self.infer_target(file);
                self.check_machine(file)?;
                // Claimed when resolution loads it. `--start-lib` IR is
                // linked eagerly: only the plugin knows what it defines.
                let input_number = self.next_position()?;
                let mut input = self.input(InputPosition::new(input_number, 0), InputRole::Object);
                input.file = Some(file);
                input.live_at_start = true;
                self.files.push(input);
                Ok(())
            }
            _ => Err(Error::malformed(
                file.path(),
                0,
                "file format not recognized",
            )),
        }
    }

    fn add_shared(
        &mut self,
        file: &'a InputFile,
        attrs: InputAttrs,
        found_as: &[u8],
    ) -> Result<()> {
        if attrs.static_only || self.static_output {
            return Err(Error::Option(format!(
                "attempted static link of dynamic object {}",
                file.path().display()
            )));
        }
        self.infer_target(file);
        let found_as = if found_as.is_empty() {
            file.path().as_os_str().as_encoded_bytes()
        } else {
            found_as
        };
        let shared = SharedInput::parse(
            file.data(),
            ElfSource::new(file.path()),
            found_as,
            attrs.as_needed,
        )?;
        if self.sonames.contains(&shared.needed_name) {
            return Ok(());
        }
        self.sonames.push(shared.needed_name.clone());
        let input_number = self.next_position()?;
        let mut input = self.input(InputPosition::new(input_number, 0), InputRole::Shared);
        input.file = Some(file);
        input.live_at_start = true;
        input.shared = Some(shared);
        self.files.push(input);
        Ok(())
    }

    fn add_archive(
        &mut self,
        id: FileId,
        file: &'a InputFile,
        attrs: InputAttrs,
        prepared: Option<Result<Vec<PreparedMember<'a>>>>,
    ) -> Result<()> {
        let archive = file.archive()?;
        let input_number = self.next_position()?;
        let first = self.files.len();
        // Header offset -> index into `files`, for the symbol index.
        let mut by_offset: Vec<(u64, usize)> = Vec::new();
        let mut add = |walker: &mut Self, ordinal: usize, member: Member<'a>, entry| {
            let ordinal = u32::try_from(ordinal)
                .map_err(|_| Error::Limit("too many archive members".into()))?;
            let mut input =
                walker.input(InputPosition::new(input_number, ordinal), InputRole::Member);
            match entry {
                None => {
                    input.thin = Some(ThinMember {
                        archive: id,
                        member,
                    });
                }
                Some(entry) => {
                    let member_id = walker.table.push_member(entry)?;
                    let member_file = walker.table.get(member_id);
                    if let Some(member_file) = member_file {
                        walker.infer_target(member_file);
                        walker.check_machine(member_file)?;
                    }
                    input.file = member_file;
                }
            }
            input.live_at_start = attrs.whole_archive;
            by_offset.push((member.header_offset, walker.files.len()));
            walker.files.push(input);
            Ok::<(), Error>(())
        };
        match prepared {
            Some(members) => {
                for (ordinal, prepared) in members?.into_iter().enumerate() {
                    add(self, ordinal, prepared.member, Some(prepared.entry))?;
                }
            }
            None => {
                for (ordinal, member) in archive.members().enumerate() {
                    let member = member?;
                    let entry = if archive.is_thin() {
                        None
                    } else {
                        Some(self.table.member_entry(id, &member)?)
                    };
                    add(self, ordinal, member, entry)?;
                }
            }
        }
        if attrs.whole_archive {
            return Ok(());
        }
        match archive.symbol_index() {
            Some(_) => {
                by_offset.sort_unstable();
                self.deferred.push(DeferredIndex {
                    file,
                    first,
                    by_offset,
                });
                Ok(())
            }
            None => {
                // No index: learn what each member defines by reading it.
                // IR members are claimed before resolution instead.
                let claim = self.lto == LtoMode::Claim;
                let members = self.files.get_mut(first..).unwrap_or_default();
                members.par_iter_mut().try_for_each(|input| -> Result<()> {
                    let Some(member_file) = input.file else {
                        return Ok(());
                    };
                    match member_file.format() {
                        FileFormat::Elf(i) if i.is_relocatable() => {
                            let (names, gcc_lto) = defined_names::<F>(member_file)?;
                            if gcc_lto && claim {
                                input.needs_claim = true;
                            } else {
                                input.lazy_names = names;
                            }
                        }
                        format if format.is_ir() => input.needs_claim = claim,
                        _ => {}
                    }
                    Ok(())
                })?;
                Ok(())
            }
        }
    }

    fn add_script(&mut self, file: &'a InputFile, attrs: InputAttrs, what: &str) -> Result<()> {
        self.depth = self.depth.saturating_add(1);
        if self.depth > 16 {
            return Err(Error::Limit(format!(
                "linker scripts nested too deeply at {}",
                file.path().display()
            )));
        }
        let mut reader = script::NoIncludes;
        let parsed = script::parse_script(file.data(), file.path(), &mut reader)?;
        let mut entries: Vec<(Source, InputAttrs, Vec<u8>)> = Vec::new();
        for command in &parsed.commands {
            let (list, lazy) = match &command.kind {
                CommandKind::Input(list) | CommandKind::Group(list) => (list, false),
                CommandKind::Lib(list) => (list, true),
                CommandKind::OutputFormat { .. }
                | CommandKind::OutputArch(_)
                | CommandKind::SearchDir(_)
                | CommandKind::Target(_) => continue,
                _ => {
                    return Err(Error::Unimplemented(format!(
                        "linker script {}{} beyond INPUT/GROUP (roadmap M3: linker scripts)",
                        file.path().display(),
                        if what.is_empty() {
                            String::new()
                        } else {
                            format!(" ({what})")
                        }
                    )));
                }
            };
            for entry in list {
                let mut entry_attrs = attrs;
                entry_attrs.lazy |= lazy;
                entry_attrs.as_needed |= entry.as_needed;
                let source = self.resolve_script_input(&entry.name, file.path(), entry_attrs)?;
                let found_as = match (&entry.name, &source) {
                    (InputName::Library(_), Source::Path(path)) => base_name_of(path),
                    (InputName::Path(path), _) => path.clone(),
                    _ => Vec::new(),
                };
                entries.push((source, entry_attrs, found_as));
            }
        }
        for (source, entry_attrs, found_as) in entries {
            let id = self.table.load(&source)?;
            self.add(id, entry_attrs, "", &found_as, None)?;
        }
        self.depth = self.depth.saturating_sub(1);
        Ok(())
    }

    fn resolve_script_input(
        &self,
        name: &InputName,
        script: &Path,
        attrs: InputAttrs,
    ) -> Result<Source> {
        match name {
            InputName::Library(lib) => {
                let lib = String::from_utf8_lossy(lib).into_owned();
                self.search
                    .find_library(&lib, attrs.static_only)
                    .map(Source::Path)
                    .ok_or_else(|| Error::NotFound(format!("cannot find -l{lib}")))
            }
            InputName::Path(path) => {
                let path = PathBuf::from(String::from_utf8_lossy(path).into_owned());
                let fs = self.table;
                // GNU ld: an absolute path in a script that itself lies in
                // the sysroot is looked up under the sysroot. Without this a
                // cross link picks up the host's `/lib64/libc.so.6`.
                if path.is_absolute()
                    && let Some(sysroot) = self.search.sysroot
                    && script.starts_with(sysroot)
                {
                    let mut inside = sysroot.to_path_buf();
                    inside.extend(path.components().skip(1));
                    if crate::input::FileSystem::is_file(&fs, &inside) {
                        return Ok(Source::Path(inside));
                    }
                }
                let resolved = crate::input::search::apply_sysroot(&path, self.search.sysroot);
                if crate::input::FileSystem::is_file(&fs, &resolved) {
                    return Ok(Source::Path(resolved));
                }
                if path.is_relative() {
                    if let Some(dir) = script.parent() {
                        let beside = dir.join(&path);
                        if crate::input::FileSystem::is_file(&fs, &beside) {
                            return Ok(Source::Path(beside));
                        }
                    }
                    if let Some(found) = self.search.find_exact(&path.to_string_lossy()) {
                        return Ok(Source::Path(found));
                    }
                }
                Err(Error::NotFound(format!(
                    "cannot find {} (named in {})",
                    path.display(),
                    script.display()
                )))
            }
        }
    }
}

/// The global symbols an object defines, for lazy objects and archives
/// without an index, and whether the object carries GCC LTO IR (only
/// checked when it defines GCC's slim-object marker or nothing).
fn defined_names<F: ElfFormat>(file: &InputFile) -> Result<(Vec<SymbolName<'_>>, bool)> {
    let source = match file.member() {
        Some(member) => ElfSource::member(file.path(), member),
        None => ElfSource::new(file.path()),
    };
    let object = ObjectFile::<F>::parse(file.data(), source)?;
    let symbols = object.symbols();
    let mut names = Vec::new();
    for symbol in symbols.globals() {
        let symbol = symbol?;
        if symbol.is_local() || matches!(symbol.section, SectionIndex::Undefined) {
            continue;
        }
        names.push(SymbolName::new(symbol.name));
    }
    // Fat LTO objects define their real symbols and are linkable natively,
    // but the IR is what a plugin wants: look for it in every object.
    let gcc_lto = object.has_gcc_lto_ir().unwrap_or(false);
    Ok((names, gcc_lto))
}

/// Adds the inputs an LTO plugin asked for after code generation: `objects`
/// (already in the file table) that are not relocatable objects, which the
/// caller places itself, and the `-l` `libraries`, searched in
/// `library_paths` and then the `-L` paths. They must be for `target`,
/// the target the first walk found ([`Inputs::target`]).
/// Libraries already in the link, and libraries not found, are skipped, and
/// relocatable output takes no libraries. The new inputs come after every
/// existing one, with the `-Bstatic`/`--as-needed` state of the last
/// command-line input.
///
/// # Errors
///
/// Errors loading the new inputs.
pub fn add_after_lto<'a, F: crate::elf::read::ElfFormat>(
    files: &mut Vec<ElfInput<'a, F>>,
    options: &LinkOptions,
    target: Option<Target>,
    objects: &[FileId],
    libraries: &[std::ffi::OsString],
    library_paths: &[PathBuf],
) -> Result<()> {
    let Some(template) = files.first() else {
        return Err(Error::Internal("no internal input file".into()));
    };
    let (table, config) = (template.table, template.config);
    let search_paths: Vec<PathBuf> = library_paths
        .iter()
        .chain(&options.search_paths)
        .cloned()
        .collect();
    let fs = table;
    let static_output = matches!(
        options.kind,
        crate::args::OutputKind::StaticExecutable | crate::args::OutputKind::StaticPie
    );
    let mut attrs = options
        .inputs
        .last()
        .map(|spec| spec.attrs)
        .unwrap_or_default();
    attrs.whole_archive = false;
    attrs.lazy = false;
    attrs.static_only |= static_output;
    let ordinal = files
        .iter()
        .map(|file| file.position.input())
        .max()
        .unwrap_or(0);
    let mut walker = Walker {
        table,
        search: SearchContext {
            search_paths: &search_paths,
            sysroot: options.sysroot.as_deref(),
            naming: LibraryNaming::Elf,
            fs: &fs,
        },
        config,
        sonames: files
            .iter()
            .filter_map(|file| Some(file.shared.as_ref()?.needed_name.clone()))
            .collect(),
        files: std::mem::take(files),
        ordinal,
        // What the first walk found; if nothing named a target, the
        // generated objects and libraries do.
        target,
        depth: 0,
        static_output,
        lto: LtoMode::AfterLto,
        deferred: Vec::new(),
    };
    let result = (|| -> Result<()> {
        for &id in objects {
            let found_as = table.get(id).map(|f| base_name_of(f.path()));
            walker.add(id, attrs, "", &found_as.unwrap_or_default(), None)?;
        }
        if options.kind == crate::args::OutputKind::Relocatable {
            // Undefined symbols stay undefined in relocatable output.
            return Ok(());
        }
        for library in libraries {
            let name = library.to_string_lossy();
            // GCC's plugin hands back every library the driver named
            // (`-pass-through=-lgcc_s` even for -static); one that is not
            // found cannot have been needed by the original link either.
            let Some(path) = walker.search.find_library(&name, attrs.static_only) else {
                continue;
            };
            let present = walker.files.iter().any(|file| {
                file.file
                    .is_some_and(|f| f.parent().is_none() && f.path() == path)
            });
            if present {
                continue;
            }
            let id = table.load_path(&path)?;
            walker.add(id, attrs, "", &base_name_of(&path), None)?;
        }
        Ok(())
    })();
    // The archives added before a failure still report their errors first.
    let result = walker.finish().and(result);
    *files = walker.files;
    result
}

/// Reads one deferred archive symbol index into its `members` (the
/// archive's entries of `files`, in member order); see [`Walker::finish`].
fn read_symbol_index<'a, F: crate::elf::read::ElfFormat>(
    archive: &DeferredIndex<'a>,
    members: &mut [ElfInput<'a, F>],
    lto: LtoMode,
) -> Result<()> {
    let file = archive.file;
    let parsed = file.archive()?;
    let Some(index) = parsed.symbol_index() else {
        return Ok(());
    };
    let by_offset = &archive.by_offset;
    for symbol in index.iter() {
        let symbol = symbol?;
        let found = by_offset
            .binary_search_by_key(&symbol.member_offset, |&(offset, _)| offset)
            .ok()
            .and_then(|at| by_offset.get(at))
            .and_then(|&(_, slot)| members.get_mut(slot.checked_sub(archive.first)?));
        match found {
            Some(input) => input.lazy_names.push(SymbolName::new(symbol.name)),
            None => {
                return Err(Error::malformed(
                    file.path(),
                    symbol.member_offset,
                    "archive symbol index (member offset)",
                ));
            }
        }
    }
    if lto == LtoMode::Claim {
        // IR members the index does not describe (an archive built
        // without the plugin lists at most GCC's marker symbol) are
        // claimed before resolution to learn their symbols.
        members.par_iter_mut().for_each(|input| {
            let Some(member_file) = input.file else {
                return;
            };
            let undescribed = input.lazy_names.iter().all(|name| {
                name.bytes() == super::object::GCC_LTO_SLIM_MARKER
                    || name.bytes() == b"__gnu_lto_v1"
            });
            if !undescribed {
                return;
            }
            let ir = match member_file.format() {
                FileFormat::Elf(i) if i.is_relocatable() => {
                    defined_names::<F>(member_file).is_ok_and(|(_, gcc_lto)| gcc_lto)
                }
                format => format.is_ir(),
            };
            if ir {
                input.needs_claim = true;
                input.lazy_names = Vec::new();
            }
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_and_defsym_expressions() {
        assert_eq!(parse_number("0x1234"), Some(0x1234));
        assert_eq!(parse_number("42"), Some(42));
        assert_eq!(parse_number("010"), Some(8));
        assert_eq!(parse_number("0"), Some(0));
        assert_eq!(parse_number("foo"), None);
        assert_eq!(parse_defsym("0x10"), Some(DefsymExpr::Absolute(16)));
        assert_eq!(
            parse_defsym("table+8"),
            Some(DefsymExpr::Symbol("table".into(), 8))
        );
        assert_eq!(
            parse_defsym("real_function"),
            Some(DefsymExpr::Symbol("real_function".into(), 0))
        );
        assert_eq!(
            parse_defsym("x - 0x4"),
            Some(DefsymExpr::Symbol("x".into(), -4))
        );
        assert_eq!(parse_defsym("a * 2"), None);
    }
}
