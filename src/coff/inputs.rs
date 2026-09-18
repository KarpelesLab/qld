//! Input resolution for a PE link: from [`InputSpec`](crate::args::InputSpec)s to the file list
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

use super::imports::{self, Groups};
use super::machine::Machine;
use super::object::ParsedObject;
use super::read::{CoffObject, PeImage, ShortImport, Source as CoffSource};

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

/// Resolves, loads and expands every input for an image of `machine`.
///
/// An object named on the command line must be for `machine` (or for no
/// machine in particular); archive members and import objects for another
/// machine are skipped, as GNU `ld` skips an incompatible library, so that
/// a multilib search path can list both 32- and 64-bit directories.
///
/// # Errors
///
/// Returns [`Error::NotFound`] for missing libraries, [`Error::Io`] for files
/// that cannot be read, [`Error::Unimplemented`] for inputs a later milestone
/// covers, [`Error::Option`] for an object of another machine, and parse
/// errors for malformed archives.
pub fn collect<'a>(
    options: &LinkOptions,
    table: &'a FileTable,
    internal: &'a InternalNames,
    machine: u16,
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
        groups: Groups::new(),
        machine,
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
    walker.add_generated_imports()?;
    Ok(Inputs {
        files: walker.files,
    })
}

struct Walker<'a> {
    table: &'a FileTable,
    files: Vec<CoffInput<'a>>,
    ordinal: u32,
    /// Imports collected from short import libraries and from DLLs named on
    /// the command line, turned into objects once the walk is over.
    groups: Groups,
    /// The machine of the image, which the inputs must be for.
    machine: u16,
}

impl<'a> Walker<'a> {
    /// Whether an input for `machine` can go into the image. Machine 0
    /// (`IMAGE_FILE_MACHINE_UNKNOWN`) is what machine-independent objects
    /// carry.
    fn compatible(&self, machine: u16) -> bool {
        machine == self.machine || machine == super::read::consts::IMAGE_FILE_MACHINE_UNKNOWN
    }

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
            FileFormat::Coff(ident) => {
                if !self.compatible(ident.machine) {
                    return Err(incompatible(file, ident.machine, self.machine));
                }
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
            FileFormat::CoffImport(ident) => {
                if !self.compatible(ident.machine) {
                    return Err(incompatible(file, ident.machine, self.machine));
                }
                let number = self.next_position()?;
                let import = ShortImport::parse(file.data(), source_of(file))?;
                self.groups.add_short_import(&import, number);
                Ok(())
            }
            FileFormat::Pe(ident) => {
                if !self.compatible(ident.machine) {
                    return Err(incompatible(file, ident.machine, self.machine));
                }
                let number = self.next_position()?;
                let image = PeImage::parse(file.data(), source_of(file))?;
                let fallback = file
                    .path()
                    .file_name()
                    .map_or(b"".as_slice(), |name| name.as_encoded_bytes());
                self.groups
                    .add_dll(&image, number, fallback, Machine::or_default(self.machine))
            }
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
        // Members that became imports rather than link inputs; the symbol
        // index still names them.
        let mut consumed: Vec<u64> = Vec::new();
        for (ordinal, member) in archive.members().enumerate() {
            let member = member?;
            let ordinal = u32::try_from(ordinal)
                .map_err(|_| Error::Limit("too many archive members".into()))?;
            let member_id = self.table.add_member(id, &member)?;
            let Some(member_file) = self.table.get(member_id) else {
                continue;
            };
            // A short import member becomes a generated `.idata$N` object,
            // not a link input of its own.
            if let FileFormat::CoffImport(ident) = member_file.format() {
                if !self.compatible(ident.machine) {
                    consumed.push(member.header_offset);
                    continue;
                }
                let import = ShortImport::parse(member_file.data(), source_of(member_file))?;
                self.groups.add_short_import(&import, number);
                consumed.push(member.header_offset);
                continue;
            }
            if let FileFormat::Coff(ident) = member_file.format() {
                if !self.compatible(ident.machine) {
                    consumed.push(member.header_offset);
                    continue;
                }
                // The helper objects an MSVC-style import library carries
                // (`__IMPORT_DESCRIPTOR_*`, `__NULL_IMPORT_DESCRIPTOR`,
                // `*_NULL_THUNK_DATA`) describe the same import directory
                // qld generates from the short import objects, so they are
                // dropped rather than linked twice.
                if is_import_helper(member_file)? {
                    consumed.push(member.header_offset);
                    continue;
                }
            }
            let mut input = self.input(InputPosition::new(number, ordinal), InputRole::Member);
            input.file = Some(member_file);
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
                        None if consumed.contains(&symbol.member_offset) => {}
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

impl<'a> Walker<'a> {
    /// Turns the collected import groups into lazy COFF objects.
    fn add_generated_imports(&mut self) -> Result<()> {
        if self.groups.is_empty() {
            return Ok(());
        }
        let machine = self.machine;
        imports::check_machine(machine)?;
        for generated in imports::generate(self.table, &self.groups, machine)? {
            let Some(file) = self.table.get(generated.id) else {
                continue;
            };
            let mut input = self.input(
                InputPosition::new(generated.position, generated.ordinal),
                InputRole::Member,
            );
            input.file = Some(file);
            input.exclude_from_implib = true;
            input.lazy_names = generated
                .defines
                .iter()
                .filter_map(|name| {
                    // The names live in the generated object's own string
                    // table, so they last as long as the link.
                    find_name(file.data(), name).map(SymbolName::new)
                })
                .collect();
            self.files.push(input);
        }
        Ok(())
    }
}

/// The error for an input built for another machine, worded as GNU `ld`
/// words it.
fn incompatible(file: &InputFile, found: u16, wanted: u16) -> Error {
    let name = |machine: u16| {
        super::read::consts::machine_name(machine).map_or_else(
            || format!("machine {machine:#06x}"),
            |name| {
                name.trim_start_matches("IMAGE_FILE_MACHINE_")
                    .to_ascii_lowercase()
            },
        )
    };
    Error::Option(format!(
        "{}: {} architecture of input file is incompatible with {} output",
        file.path().display(),
        name(found),
        name(wanted)
    ))
}

/// Whether an archive member is one of the helper objects an MSVC-style
/// short import library carries alongside its import objects.
///
/// They hold the null import descriptor and the null thunk terminators that
/// qld generates itself from the short import objects, so linking them as
/// well would produce the directory twice.
fn is_import_helper(file: &InputFile) -> Result<bool> {
    let object = CoffObject::parse(file.data(), source_of(file))?;
    let mut helper = false;
    for symbol in object.symbols().iter() {
        let symbol = symbol?;
        if !symbol.is_defined_external() {
            continue;
        }
        let name = symbol.name;
        if name.starts_with(b"__IMPORT_DESCRIPTOR_")
            || name == b"__NULL_IMPORT_DESCRIPTOR"
            || name.ends_with(b"_NULL_THUNK_DATA")
        {
            helper = true;
        } else {
            return Ok(false);
        }
    }
    Ok(helper)
}

/// Finds `name` inside `data`, returning the slice with the file's lifetime.
///
/// The generated objects hold every symbol name in their own string table,
/// so the lazy name list can borrow from the mapped bytes rather than from a
/// temporary.
fn find_name<'a>(data: &'a [u8], name: &[u8]) -> Option<&'a [u8]> {
    let end = name.len();
    data.windows(end.saturating_add(1))
        .position(|window| window.get(..end) == Some(name) && window.get(end) == Some(&0))
        .and_then(|at| data.get(at..at.saturating_add(end)))
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
