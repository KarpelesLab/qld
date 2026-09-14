//! Relocatable objects (`MH_OBJECT`).

use super::bytes::{Source, subslice};
use super::commands::{
    BuildVersion, DataInCodeEntry, DysymtabCommand, LinkerOptionHint, OptimizationHintIter,
    data_in_code_entries, linker_option_hint,
};
use super::consts::{
    LC_DATA_IN_CODE, LC_DYSYMTAB, LC_LINKER_OPTIMIZATION_HINT, LC_LINKER_OPTION, LC_SEGMENT,
    LC_SEGMENT_64, LC_SYMTAB, MH_OBJECT,
};
use super::file::{MachHeader, MachOFile};
use super::reloc::{PairedRelocationIter, RelocationTable};
use super::section::Section;
use super::symbol::SymbolTable;
use crate::error::Result;

/// A parsed relocatable object.
///
/// Parsing walks the load commands once, decodes the section headers into a
/// vector (one allocation per object), and locates the symbol and string
/// tables. Section contents, symbols and relocations stay in the input
/// bytes and are decoded on demand.
#[derive(Clone, Debug)]
pub struct ObjectFile<'a> {
    file: MachOFile<'a>,
    sections: Vec<Section<'a>>,
    symbols: SymbolTable<'a>,
    dysymtab: Option<DysymtabCommand>,
    build_version: Option<BuildVersion<'a>>,
    data_in_code: &'a [u8],
    data_in_code_offset: u64,
    optimization_hints: &'a [u8],
    optimization_hints_offset: u64,
}

impl<'a> ObjectFile<'a> {
    /// Parses an `MH_OBJECT` file.
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` if the file is not a Mach-O object, a load
    /// command is malformed, there is more than one symbol table, or a table
    /// lies outside the file. Section contents and relocation ranges are
    /// checked too, so later accessors cannot fail on them.
    pub fn parse(data: &'a [u8], source: Source<'a>) -> Result<Self> {
        let file = MachOFile::parse(data, source)?;
        if file.header().file_type != MH_OBJECT {
            return Err(source.malformed(12, "Mach-O file type (expected MH_OBJECT)"));
        }
        let endian = file.endian();
        let is64 = file.is64();
        let mut sections = Vec::new();
        let mut symbols = None;
        let mut dysymtab = None;
        let mut build_version = None;
        let mut data_in_code = (&[][..], 0);
        let mut optimization_hints = (&[][..], 0);
        for command in file.load_commands() {
            let command = command?;
            match command.cmd {
                LC_SEGMENT | LC_SEGMENT_64 => {
                    let segment = command.segment()?;
                    sections.reserve(segment.sections.len());
                    for index in 0..segment.sections.len() {
                        let Some(section) = segment.sections.get(index) else {
                            break;
                        };
                        let what = |w: &str| {
                            source.malformed(
                                segment.sections.record_offset(index),
                                format!("section {} ({w})", section.display_name()),
                            )
                        };
                        if !section.is_zerofill()
                            && subslice(data, u64::from(section.offset), section.size).is_none()
                        {
                            return Err(what("contents extend past end of file"));
                        }
                        if subslice(
                            data,
                            u64::from(section.reloff),
                            u64::from(section.nreloc).saturating_mul(8),
                        )
                        .is_none()
                        {
                            return Err(what("relocations extend past end of file"));
                        }
                        if section.addr.checked_add(section.size).is_none() {
                            return Err(what("address range overflows"));
                        }
                        sections.push(section);
                    }
                }
                LC_SYMTAB => {
                    if symbols.is_some() {
                        return Err(source.malformed(command.offset, "LC_SYMTAB (duplicate)"));
                    }
                    let symtab = command.symtab()?;
                    let entry = if is64 { 16 } else { 12 };
                    let records = file.bytes(
                        u64::from(symtab.symoff),
                        u64::from(symtab.nsyms).saturating_mul(entry),
                        "symbol table",
                    )?;
                    let strtab = file.bytes(
                        u64::from(symtab.stroff),
                        u64::from(symtab.strsize),
                        "string table",
                    )?;
                    symbols = Some(SymbolTable::new(
                        records,
                        strtab,
                        u64::from(symtab.symoff),
                        endian,
                        is64,
                        source,
                    ));
                }
                LC_DYSYMTAB => dysymtab = Some(command.dysymtab()?),
                LC_DATA_IN_CODE => {
                    let info = command.linkedit_data()?;
                    data_in_code = (
                        file.bytes(
                            u64::from(info.dataoff),
                            u64::from(info.datasize),
                            "data-in-code table",
                        )?,
                        u64::from(info.dataoff),
                    );
                }
                LC_LINKER_OPTIMIZATION_HINT => {
                    let info = command.linkedit_data()?;
                    optimization_hints = (
                        file.bytes(
                            u64::from(info.dataoff),
                            u64::from(info.datasize),
                            "linker optimization hints",
                        )?,
                        u64::from(info.dataoff),
                    );
                }
                _ if command.is_version() && build_version.is_none() => {
                    build_version = Some(command.build_version()?);
                }
                _ => {}
            }
        }
        if sections.len() > 255 {
            return Err(source.malformed(0, "section count (more than 255)"));
        }
        Ok(Self {
            symbols: symbols.unwrap_or_else(|| SymbolTable::empty(endian, is64, source)),
            file,
            sections,
            dysymtab,
            build_version,
            data_in_code: data_in_code.0,
            data_in_code_offset: data_in_code.1,
            optimization_hints: optimization_hints.0,
            optimization_hints_offset: optimization_hints.1,
        })
    }

    /// The underlying file.
    #[must_use]
    pub fn file(&self) -> &MachOFile<'a> {
        &self.file
    }

    /// The header.
    #[must_use]
    pub fn header(&self) -> &MachHeader {
        self.file.header()
    }

    /// The error context.
    #[must_use]
    pub fn source(&self) -> Source<'a> {
        self.file.source()
    }

    /// Whether `MH_SUBSECTIONS_VIA_SYMBOLS` is set.
    #[must_use]
    pub fn subsections_via_symbols(&self) -> bool {
        self.header().subsections_via_symbols()
    }

    /// The sections, in order. Section ordinal `n` (as in `n_sect`) is
    /// `sections()[n - 1]`.
    #[must_use]
    pub fn sections(&self) -> &[Section<'a>] {
        &self.sections
    }

    /// The section with 1-based ordinal `ordinal`.
    #[must_use]
    pub fn section_by_ordinal(&self, ordinal: u32) -> Option<&Section<'a>> {
        self.sections
            .get(usize::try_from(ordinal).ok()?.checked_sub(1)?)
    }

    /// Finds a section by segment and section name.
    #[must_use]
    pub fn find_section(&self, segname: &[u8], sectname: &[u8]) -> Option<(usize, &Section<'a>)> {
        self.sections
            .iter()
            .enumerate()
            .find(|(_, s)| s.is(segname, sectname))
    }

    /// Contents of section `index` (0-based). Zero-fill sections are empty.
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` if `index` is out of range.
    pub fn section_data(&self, index: usize) -> Result<&'a [u8]> {
        let section = self.section(index)?;
        section.data(self.file.data(), self.source())
    }

    fn section(&self, index: usize) -> Result<&Section<'a>> {
        self.sections.get(index).ok_or_else(|| {
            self.source()
                .malformed(0, format!("section index {index} (out of range)"))
        })
    }

    /// The relocation table of section `index` (0-based).
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` if `index` is out of range.
    pub fn relocations(&self, index: usize) -> Result<RelocationTable<'a>> {
        let section = self.section(index)?;
        let data = section.relocation_bytes(self.file.data(), self.source())?;
        Ok(RelocationTable::new(
            data,
            u64::from(section.reloff),
            self.file.endian(),
            self.header().cpu_type,
        ))
    }

    /// The paired relocations of section `index` (0-based).
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` if `index` is out of range.
    pub fn paired_relocations(&self, index: usize) -> Result<PairedRelocationIter<'a>> {
        Ok(self.relocations(index)?.paired(self.source()))
    }

    /// The symbol table (empty when there is no `LC_SYMTAB`).
    #[must_use]
    pub fn symbols(&self) -> &SymbolTable<'a> {
        &self.symbols
    }

    /// `LC_DYSYMTAB`, when present.
    #[must_use]
    pub fn dysymtab(&self) -> Option<&DysymtabCommand> {
        self.dysymtab.as_ref()
    }

    /// The first `LC_BUILD_VERSION` or `LC_VERSION_MIN_*` command.
    #[must_use]
    pub fn build_version(&self) -> Option<&BuildVersion<'a>> {
        self.build_version.as_ref()
    }

    /// The `LC_LINKER_OPTION` string lists, in order.
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` for a malformed command.
    pub fn linker_options(&self) -> Result<Vec<Vec<&'a [u8]>>> {
        let mut options = Vec::new();
        for command in self.file.load_commands() {
            let command = command?;
            if command.cmd == LC_LINKER_OPTION {
                options.push(command.linker_option_strings()?);
            }
        }
        Ok(options)
    }

    /// The `-l` and `-framework` requests of `LC_LINKER_OPTION` commands.
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` for a malformed command.
    pub fn linker_option_hints(&self) -> Result<Vec<LinkerOptionHint<'a>>> {
        let mut hints = Vec::new();
        for command in self.file.load_commands() {
            let command = command?;
            if command.cmd != LC_LINKER_OPTION {
                continue;
            }
            let strings = command.linker_option_strings()?;
            hints.push(linker_option_hint(&strings));
        }
        Ok(hints)
    }

    /// The `LC_DATA_IN_CODE` entries.
    pub fn data_in_code(&self) -> impl ExactSizeIterator<Item = DataInCodeEntry> + '_ {
        data_in_code_entries(self.data_in_code, self.file.endian())
    }

    /// The raw `LC_LINKER_OPTIMIZATION_HINT` ULEB128 stream.
    #[must_use]
    pub fn optimization_hint_bytes(&self) -> &'a [u8] {
        self.optimization_hints
    }

    /// Decodes the `LC_LINKER_OPTIMIZATION_HINT` stream.
    #[must_use]
    pub fn optimization_hints(&self) -> OptimizationHintIter<'a> {
        OptimizationHintIter::new(
            self.optimization_hints,
            self.optimization_hints_offset,
            self.source(),
        )
    }

    /// File offset of the data-in-code table.
    #[must_use]
    pub fn data_in_code_offset(&self) -> u64 {
        self.data_in_code_offset
    }

    /// Pointer size of the object in bytes: 8 or 4.
    #[must_use]
    pub fn word_size(&self) -> u64 {
        if self.file.is64() { 8 } else { 4 }
    }

    /// The index (0-based) of the section containing object address `addr`,
    /// excluding end addresses.
    #[must_use]
    pub fn section_at_address(&self, addr: u64) -> Option<usize> {
        self.sections
            .iter()
            .position(|s| addr >= s.addr && addr.wrapping_sub(s.addr) < s.size)
    }

    /// File offset of `index`'s contents, for error messages.
    #[must_use]
    pub fn section_file_offset(&self, index: usize) -> u64 {
        self.sections.get(index).map_or(0, |s| u64::from(s.offset))
    }

    /// Total number of relocation entries, for preallocation.
    #[must_use]
    pub fn relocation_count(&self) -> u64 {
        self.sections
            .iter()
            .map(|s| u64::from(s.nreloc))
            .fold(0, u64::saturating_add)
    }
}
