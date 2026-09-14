//! Input resolution for a PE link: from [`InputSpec`]s to the file list
//! symbol resolution works on.
//!
//! The walk mirrors [`crate::elf::inputs`]: command-line files are mapped in
//! parallel, then a sequential pass in command-line order turns each one into
//! [`CoffInput`]s. A COFF object becomes one live input; an archive becomes
//! one lazy input per member, with lazy names from the archive symbol index;
//! `--whole-archive` members are live from the start. File 0 is the linker's
//! own internal file, holding the entry point and `-u` references.
//!
//! MinGW import libraries (`libfoo.dll.a`, `libfoo.a` from `dlltool`) are
//! ordinary archives of ordinary COFF objects whose `.idata$N` sections build
//! the import directory, so they need no special handling here: grouped
//! section ordering in [`layout`](super::layout) does the rest, exactly as
//! GNU `ld`'s default script does.

#![deny(clippy::arithmetic_side_effects)]

use rayon::prelude::*;

use crate::args::{InputAttrs, InputKind, LinkOptions};
use crate::error::{Error, Result};
use crate::ids::FileId;
use crate::input::identify::FileFormat;
use crate::input::{FileTable, InputFile, LibraryNaming, RealFileSystem, SearchContext, Source};
use crate::symbols::{InputPosition, ResolveFile, SymbolName, SymbolUse};

use super::object::ParsedObject;
use super::read::{CoffObject, Source as CoffSource};

/// What kind of input a [`CoffInput`] is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputRole {
    /// The linker's internal file: the entry point and `-u` references.
    Internal,
    /// An object named on the command line.
    Object,
    /// An archive member.
    Member,
}

/// Symbols the linker itself references.
#[derive(Debug, Default)]
pub struct InternalSymbols<'a> {
    /// Names, in order.
    pub names: Vec<SymbolName<'a>>,
    /// How each is used.
    pub uses: Vec<SymbolUse>,
}

/// The names the linker itself references, owned for the link's duration.
#[derive(Debug, Default)]
pub struct InternalNames {
    /// Name and use of each linker-created symbol.
    pub names: Vec<(Vec<u8>, SymbolUse)>,
}

impl InternalNames {
    /// The references a PE link makes on its own: the entry point, `-u` and
    /// `--require-defined`.
    #[must_use]
    pub fn new(options: &LinkOptions, entry: Option<&[u8]>) -> Self {
        let reference = SymbolUse::Reference { weak: false };
        let mut names = Vec::new();
        if let Some(entry) = entry {
            names.push((entry.to_vec(), reference));
        }
        for name in options.undefined.iter().chain(&options.require_defined) {
            names.push((name.as_bytes().to_vec(), reference));
        }
        Self { names }
    }

    /// Adds a name the link must keep, such as a `.drectve` `-include:`.
    pub fn push(&mut self, name: &[u8]) {
        if !self.names.iter().any(|(existing, _)| existing == name) {
            self.names
                .push((name.to_vec(), SymbolUse::Reference { weak: false }));
        }
    }
}

/// One input as symbol resolution sees it.
#[derive(Debug)]
pub struct CoffInput<'a> {
    /// Where the input sits on the command line.
    pub position: InputPosition,
    /// What kind of input this is.
    pub role: InputRole,
    /// The mapped file.
    pub file: Option<&'a InputFile>,
    /// Live from the start (objects, `--whole-archive` members).
    pub live_at_start: bool,
    /// Names a lazy member would define.
    pub lazy_names: Vec<SymbolName<'a>>,
    /// The parsed object, once loaded.
    pub parsed: Option<Box<ParsedObject<'a>>>,
    /// For the internal file, its symbols.
    pub internal: InternalSymbols<'a>,
    /// Whether this input's symbols are excluded from `--out-implib`.
    pub exclude_from_implib: bool,
}

impl<'a> CoffInput<'a> {
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

    /// The archive member name, or the file name, used to order grouped
    /// sections the way GNU `ld`'s `SORT(*)` does.
    #[must_use]
    pub fn sort_name(&self) -> &[u8] {
        match self.file {
            None => b"",
            Some(file) => match file.member() {
                Some(member) => member.as_bytes(),
                None => file
                    .path()
                    .file_name()
                    .map_or(b"".as_slice(), |name| name.as_encoded_bytes()),
            },
        }
    }

    /// The parsed object, if this input is a loaded object.
    #[must_use]
    pub fn object(&self) -> Option<&ParsedObject<'a>> {
        self.parsed.as_deref()
    }
}

impl<'a> ResolveFile<'a> for CoffInput<'a> {
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
        if self.role == InputRole::Internal || self.parsed.is_some() {
            return Ok(());
        }
        let Some(file) = self.file else {
            return Ok(());
        };
        let object = CoffObject::parse(file.data(), source_of(file))?;
        self.parsed = Some(Box::new(ParsedObject::parse(object)?));
        Ok(())
    }

    fn symbol_names(&self) -> &[SymbolName<'a>] {
        match &self.parsed {
            Some(parsed) => &parsed.names,
            None => &self.internal.names,
        }
    }

    fn symbol_use(&self, index: usize) -> SymbolUse {
        let uses = match &self.parsed {
            Some(parsed) => &parsed.uses,
            None => &self.internal.uses,
        };
        uses.get(index).copied().unwrap_or(SymbolUse::Ignore)
    }
}

/// The reader's file identity for `file`.
#[must_use]
pub fn source_of(file: &InputFile) -> CoffSource<'_> {
    match file.member() {
        Some(member) => CoffSource::member(file.path(), member),
        None => CoffSource::new(file.path()),
    }
}

/// The collected inputs of a link.
#[derive(Debug)]
pub struct Inputs<'a> {
    /// Every input, file 0 being the linker's internal file.
    pub files: Vec<CoffInput<'a>>,
}

/// One entry of the expanded input list, before loading.
struct Pending {
    source: Source,
    attrs: InputAttrs,
}

/// Resolves, loads and expands every input.
///
/// # Errors
///
/// Returns [`Error::NotFound`] for missing libraries, [`Error::Io`] for files
/// that cannot be read, [`Error::Unimplemented`] for inputs a later milestone
/// covers, and parse errors for malformed archives.
pub fn collect<'a>(
    options: &LinkOptions,
    table: &'a FileTable,
    internal: &'a InternalNames,
) -> Result<Inputs<'a>> {
    let fs = RealFileSystem;
    let search = SearchContext {
        search_paths: &options.search_paths,
        sysroot: options.sysroot.as_deref(),
        naming: LibraryNaming::MinGw,
        fs: &fs,
    };

    let mut pending = Vec::with_capacity(options.inputs.len());
    for spec in &options.inputs {
        match &spec.kind {
            InputKind::JustSymbols(path) => {
                return Err(Error::Unimplemented(format!(
                    "--just-symbols={} for PE/COFF",
                    path.display()
                )));
            }
            InputKind::Script(path) => {
                return Err(Error::Unimplemented(format!(
                    "linker script {} for PE/COFF (roadmap M7)",
                    path.display()
                )));
            }
            _ => pending.push(Pending {
                source: search.resolve(spec)?,
                attrs: spec.attrs,
            }),
        }
    }
    let sources: Vec<Source> = pending.iter().map(|entry| entry.source.clone()).collect();
    let loaded = table.load_all(&sources);

    let mut internal_symbols = InternalSymbols::default();
    for (name, use_) in &internal.names {
        internal_symbols.names.push(SymbolName::new(name));
        internal_symbols.uses.push(*use_);
    }
    let mut walker = Walker {
        table,
        files: vec![CoffInput {
            position: InputPosition::new(0, 0),
            role: InputRole::Internal,
            file: None,
            live_at_start: true,
            lazy_names: Vec::new(),
            parsed: None,
            internal: internal_symbols,
            exclude_from_implib: true,
        }],
        ordinal: 0,
    };
    for (entry, id) in pending.iter().zip(loaded) {
        walker.add(id?, entry.attrs)?;
    }
    Ok(Inputs {
        files: walker.files,
    })
}

struct Walker<'a> {
    table: &'a FileTable,
    files: Vec<CoffInput<'a>>,
    ordinal: u32,
}

impl<'a> Walker<'a> {
    fn next_position(&mut self) -> Result<u32> {
        self.ordinal = self
            .ordinal
            .checked_add(1)
            .ok_or_else(|| Error::Limit("too many inputs".into()))?;
        Ok(self.ordinal)
    }

    fn input(&self, position: InputPosition, role: InputRole) -> CoffInput<'a> {
        CoffInput {
            position,
            role,
            file: None,
            live_at_start: false,
            lazy_names: Vec::new(),
            parsed: None,
            internal: InternalSymbols::default(),
            exclude_from_implib: false,
        }
    }

    fn add(&mut self, id: FileId, attrs: InputAttrs) -> Result<()> {
        let Some(file) = self.table.get(id) else {
            return Err(Error::Internal("loaded file missing from table".into()));
        };
        match file.format() {
            FileFormat::Coff(_) => {
                let number = self.next_position()?;
                let mut input = self.input(InputPosition::new(number, 0), InputRole::Object);
                input.file = Some(file);
                if attrs.lazy {
                    input.lazy_names = defined_names(file)?;
                } else {
                    input.live_at_start = true;
                }
                self.files.push(input);
                Ok(())
            }
            FileFormat::Archive | FileFormat::ThinArchive => self.add_archive(id, file, attrs),
            FileFormat::Empty => Ok(()),
            FileFormat::Pe(_) => Err(Error::Unimplemented(format!(
                "linking directly against the DLL {} (roadmap M7)",
                file.path().display()
            ))),
            _ => Err(Error::malformed(
                file.path(),
                0,
                "file format not recognized (expected a COFF object)",
            )),
        }
    }

    fn add_archive(&mut self, id: FileId, file: &'a InputFile, attrs: InputAttrs) -> Result<()> {
        let archive = file.archive()?;
        if archive.is_thin() {
            return Err(Error::Unimplemented(format!(
                "thin archive {} for PE/COFF",
                file.path().display()
            )));
        }
        let number = self.next_position()?;
        let first = self.files.len();
        let mut by_offset: Vec<(u64, usize)> = Vec::new();
        for (ordinal, member) in archive.members().enumerate() {
            let member = member?;
            let ordinal = u32::try_from(ordinal)
                .map_err(|_| Error::Limit("too many archive members".into()))?;
            let mut input = self.input(InputPosition::new(number, ordinal), InputRole::Member);
            let member_id = self.table.add_member(id, &member)?;
            input.file = self.table.get(member_id);
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
                let members = self.files.get_mut(first..).unwrap_or_default();
                members.par_iter_mut().try_for_each(|input| -> Result<()> {
                    if let Some(member) = input.file
                        && matches!(member.format(), FileFormat::Coff(_))
                    {
                        input.lazy_names = defined_names(member)?;
                    }
                    Ok(())
                })?;
            }
        }
        Ok(())
    }
}

/// The external symbols a COFF object defines, for an archive without a
/// symbol index.
fn defined_names(file: &InputFile) -> Result<Vec<SymbolName<'_>>> {
    let object = CoffObject::parse(file.data(), source_of(file))?;
    let mut names = Vec::new();
    for symbol in object.symbols().iter() {
        let symbol = symbol?;
        if symbol.is_defined_external() {
            names.push(SymbolName::new(symbol.name));
        }
    }
    Ok(names)
}
