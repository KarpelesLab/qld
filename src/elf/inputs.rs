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
use crate::elf::read::{Elf64Le, ObjectFile, SectionIndex, Source as ElfSource};
use crate::error::{Error, Result};
use crate::ids::FileId;
use crate::input::archive::Member;
use crate::input::identify::FileFormat;
use crate::input::{FileTable, InputFile, LibraryNaming, RealFileSystem, SearchContext, Source};
use crate::script::{self, CommandKind, InputName};
use crate::symbols::{DefinitionKind, InputPosition, ResolveFile, SymbolName, SymbolUse};
use crate::target::{Architecture, Target};

use super::object::{ObjectInput, ParseConfig};

/// What kind of link input an [`ElfInput`] is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputRole {
    /// The linker's internal file (entry point, `-u`, `--defsym`).
    Internal,
    /// An object named on the command line (or in a script).
    Object,
    /// An archive member.
    Member,
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

/// A member of a thin archive, loaded from disk when it is extracted.
#[derive(Debug)]
struct ThinMember<'a> {
    archive: FileId,
    member: Member<'a>,
}

/// One input as symbol resolution sees it.
#[derive(Debug)]
pub struct ElfInput<'a> {
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
    pub object: Option<ObjectInput<'a>>,
    /// For the internal file, its symbols.
    pub internal: InternalSymbols<'a>,
    thin: Option<ThinMember<'a>>,
    table: &'a FileTable,
    config: ParseConfig<'a>,
}

impl<'a> ElfInput<'a> {
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
}

impl<'a> ResolveFile<'a> for ElfInput<'a> {
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
                return Err(Error::Unimplemented(format!(
                    "LTO input {} (roadmap M6: link-time optimization)",
                    self.display()
                )));
            }
            _ => {
                return Err(source.malformed(0, "archive member (not an ELF relocatable object)"));
            }
        }
        let object = ObjectInput::parse(file.data(), source, &self.config)?;
        self.object = Some(object);
        Ok(())
    }

    fn symbol_names(&self) -> &[SymbolName<'a>] {
        match &self.object {
            Some(object) => &object.names,
            None => &self.internal.names,
        }
    }

    fn symbol_use(&self, index: usize) -> SymbolUse {
        let uses = match &self.object {
            Some(object) => &object.uses,
            None => &self.internal.uses,
        };
        uses.get(index).copied().unwrap_or(SymbolUse::Ignore)
    }
}

/// Everything collected from the command line.
#[derive(Debug)]
pub struct Inputs<'a> {
    /// All inputs, file 0 being the internal file.
    pub files: Vec<ElfInput<'a>>,
    /// The target: from `-m`, or inferred from the first object.
    pub target: Target,
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
            None => names.push((b"_start".to_vec(), reference)),
        }
        for name in options.undefined.iter().chain(&options.require_defined) {
            names.push((name.as_bytes().to_vec(), reference));
        }
        for (name, expr) in &options.defsym {
            if let Some(target) = defsym_target(expr) {
                names.push((target.as_bytes().to_vec(), reference));
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

fn defsym_target(expr: &str) -> Option<String> {
    match parse_defsym(expr)? {
        DefsymExpr::Symbol(symbol, _) => Some(symbol),
        DefsymExpr::Absolute(_) => None,
    }
}

/// One entry of the expanded input list, before loading.
struct Pending {
    source: Source,
    attrs: InputAttrs,
    what: String,
}

/// Resolves, loads and expands every input.
///
/// # Errors
///
/// Returns [`Error::NotFound`] for missing libraries, [`Error::Io`] for files
/// that cannot be read, [`Error::Unimplemented`] for inputs that need a later
/// milestone, and parse errors for malformed archives and scripts.
pub fn collect<'a>(
    options: &LinkOptions,
    table: &'a FileTable,
    internal: &'a InternalNames,
    config: ParseConfig<'a>,
) -> Result<Inputs<'a>> {
    let fs = RealFileSystem;
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
                });
            }
            _ => pending.push(Pending {
                source: search.resolve(spec)?,
                attrs: spec.attrs,
                what: String::new(),
            }),
        }
    }
    let sources: Vec<Source> = pending.iter().map(|p| p.source.clone()).collect();
    let loaded = table.load_all(&sources);

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
        thin: None,
        table,
        config,
    });

    let mut walker = Walker {
        table,
        search,
        config,
        files,
        ordinal: 0,
        target: options.target,
        depth: 0,
    };
    for (entry, id) in pending.iter().zip(loaded) {
        let id = id?;
        walker.add(id, entry.attrs, &entry.what)?;
    }
    let target = walker.target.unwrap_or(Target::X86_64_LINUX);
    if target.arch != Architecture::X86_64 {
        return Err(Error::Unimplemented(format!(
            "linking for {:?} (roadmap M4: more ELF architectures)",
            target.arch
        )));
    }
    Ok(Inputs {
        files: walker.files,
        target,
    })
}

struct Walker<'a, 's> {
    table: &'a FileTable,
    search: SearchContext<'s>,
    config: ParseConfig<'a>,
    files: Vec<ElfInput<'a>>,
    ordinal: u32,
    target: Option<Target>,
    depth: u32,
}

impl<'a> Walker<'a, '_> {
    fn next_position(&mut self) -> Result<u32> {
        self.ordinal = self
            .ordinal
            .checked_add(1)
            .ok_or_else(|| Error::Limit("too many inputs".into()))?;
        Ok(self.ordinal)
    }

    fn input(&self, position: InputPosition, role: InputRole) -> ElfInput<'a> {
        ElfInput {
            position,
            role,
            file: None,
            live_at_start: false,
            lazy_names: Vec::new(),
            object: None,
            internal: InternalSymbols::default(),
            thin: None,
            table: self.table,
            config: self.config,
        }
    }

    fn infer_target(&mut self, file: &InputFile) {
        if self.target.is_some() {
            return;
        }
        if let FileFormat::Elf(ident) = file.format()
            && let Some(arch) = ident.architecture()
        {
            let mut target = Target::X86_64_LINUX;
            target.arch = arch;
            target.endian = ident.endian;
            target.pointer_width = ident.class;
            self.target = Some(target);
        }
    }

    fn add(&mut self, id: FileId, attrs: InputAttrs, what: &str) -> Result<()> {
        let Some(file) = self.table.get(id) else {
            return Err(Error::Internal("loaded file missing from table".into()));
        };
        match file.format() {
            FileFormat::Elf(ident) if ident.file_type == ET_REL => {
                self.infer_target(file);
                let input_number = self.next_position()?;
                let mut input = self.input(InputPosition::new(input_number, 0), InputRole::Object);
                input.file = Some(file);
                if attrs.lazy {
                    input.lazy_names = defined_names(file, self.config)?;
                } else {
                    input.live_at_start = true;
                }
                self.files.push(input);
                Ok(())
            }
            FileFormat::Elf(ident) if ident.file_type == ET_DYN => {
                Err(Error::Unimplemented(format!(
                    "linking against shared object {} (roadmap M2: dynamic ELF)",
                    file.path().display()
                )))
            }
            FileFormat::Elf(_) => Err(Error::malformed(
                file.path(),
                16,
                "ELF file type (expected a relocatable object)",
            )),
            FileFormat::Archive | FileFormat::ThinArchive => self.add_archive(id, file, attrs),
            FileFormat::Text(_) => self.add_script(file, attrs, what),
            FileFormat::Empty => Ok(()),
            format if format.is_ir() => Err(Error::Unimplemented(format!(
                "LTO input {} (roadmap M6: link-time optimization)",
                file.path().display()
            ))),
            _ => Err(Error::malformed(
                file.path(),
                0,
                "file format not recognized",
            )),
        }
    }

    fn add_archive(&mut self, id: FileId, file: &'a InputFile, attrs: InputAttrs) -> Result<()> {
        let archive = file.archive()?;
        let input_number = self.next_position()?;
        let first = self.files.len();
        // Header offset -> index into `files`, for the symbol index.
        let mut by_offset: Vec<(u64, usize)> = Vec::new();
        for (ordinal, member) in archive.members().enumerate() {
            let member = member?;
            let ordinal = u32::try_from(ordinal)
                .map_err(|_| Error::Limit("too many archive members".into()))?;
            let mut input =
                self.input(InputPosition::new(input_number, ordinal), InputRole::Member);
            if archive.is_thin() {
                input.thin = Some(ThinMember {
                    archive: id,
                    member,
                });
            } else {
                let member_id = self.table.add_member(id, &member)?;
                let member_file = self.table.get(member_id);
                if let Some(member_file) = member_file {
                    self.infer_target(member_file);
                }
                input.file = member_file;
            }
            input.live_at_start = attrs.whole_archive;
            by_offset.push((member.header_offset, self.files.len()));
            self.files.push(input);
        }
        if attrs.whole_archive {
            return Ok(());
        }
        match archive.symbol_index() {
            Some(index) => {
                by_offset.sort_unstable();
                for symbol in index.iter() {
                    let symbol = symbol?;
                    let found = by_offset
                        .binary_search_by_key(&symbol.member_offset, |&(offset, _)| offset)
                        .ok()
                        .and_then(|at| by_offset.get(at))
                        .and_then(|&(_, slot)| self.files.get_mut(slot));
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
            }
            None => {
                // No index: learn what each member defines by reading it.
                let config = self.config;
                let members = self.files.get_mut(first..).unwrap_or_default();
                members.par_iter_mut().try_for_each(|input| -> Result<()> {
                    if let Some(member_file) = input.file
                        && matches!(member_file.format(), FileFormat::Elf(i) if i.is_relocatable())
                    {
                        input.lazy_names = defined_names(member_file, config)?;
                    }
                    Ok(())
                })?;
            }
        }
        Ok(())
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
        let mut entries: Vec<(Source, InputAttrs)> = Vec::new();
        for command in &parsed.commands {
            let (list, lazy) = match &command.kind {
                CommandKind::Input(list) | CommandKind::Group(list) => (list, false),
                CommandKind::Lib(list) => (list, true),
                CommandKind::OutputFormat { .. }
                | CommandKind::OutputArch(_)
                | CommandKind::SearchDir(_)
                | CommandKind::Target(_) => continue,
                other => {
                    let _ = other;
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
                entries.push((source, entry_attrs));
            }
        }
        for (source, entry_attrs) in entries {
            let id = self.table.load(&source)?;
            self.add(id, entry_attrs, "")?;
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
                let fs = RealFileSystem;
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
/// without an index.
fn defined_names<'a>(file: &'a InputFile, config: ParseConfig<'a>) -> Result<Vec<SymbolName<'a>>> {
    let source = match file.member() {
        Some(member) => ElfSource::member(file.path(), member),
        None => ElfSource::new(file.path()),
    };
    let object = ObjectFile::<Elf64Le>::parse(file.data(), source)?;
    let _ = config;
    let symbols = object.symbols();
    let mut names = Vec::new();
    for symbol in symbols.globals() {
        let symbol = symbol?;
        if symbol.is_local() || matches!(symbol.section, SectionIndex::Undefined) {
            continue;
        }
        names.push(SymbolName::new(symbol.name));
    }
    Ok(names)
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
