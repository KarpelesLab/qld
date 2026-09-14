//! COFF relocatable objects, regular and `/bigobj`.

use super::addrsig::{ADDRSIG_SECTION, AddrsigIter};
use super::consts::{
    FEAT00_GUARD_CF, FEAT00_GUARD_EHCONT, FEAT00_KERNEL, FEAT00_SAFESEH, machine_architecture,
};
use super::directives::{DRECTVE_SECTION, Directives, parse_directives};
use super::header::{FileHeader, is_bigobj, is_import_object};
use super::reloc::{RELOCATION_SIZE, Relocation, Relocations};
use super::section::{SECTION_HEADER_SIZE, SectionHeader, SectionTable};
use super::source::{Source, subslice, to_u64};
use super::symbol::{Symbol, SymbolTable};
use crate::error::Result;
use crate::target::Architecture;

/// Name of the symbol whose value holds the object's feature flags.
pub const FEAT00_SYMBOL: &[u8] = b"@feat.00";

/// A section with its number and resolved name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Section<'a> {
    /// The 1-based section number symbols refer to.
    pub number: u32,
    /// The decoded header.
    pub header: SectionHeader,
    /// The resolved name.
    pub name: &'a [u8],
}

impl Section<'_> {
    /// Whether this is a resource section from `windres` or `cvtres`:
    /// `.rsrc`, or `.rsrc$<suffix>` (`.rsrc$01` holds the directory tree,
    /// `.rsrc$02` the data).
    #[must_use]
    pub fn is_resource(&self) -> bool {
        is_resource_section_name(self.name)
    }

    /// Whether the section name is `.tls` or starts with `.tls$`.
    #[must_use]
    pub fn is_tls(&self) -> bool {
        self.name == b".tls" || self.name.starts_with(b".tls$")
    }
}

/// Whether `name` is `.rsrc` or starts with `.rsrc$`.
#[must_use]
pub fn is_resource_section_name(name: &[u8]) -> bool {
    name == b".rsrc" || name.starts_with(b".rsrc$")
}

/// The `@feat.00` feature flags.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Feat00(pub u32);

impl Feat00 {
    /// `FEAT00_SAFESEH`: the object is SafeSEH compatible.
    #[must_use]
    pub fn safe_seh(self) -> bool {
        self.0 & FEAT00_SAFESEH != 0
    }

    /// `FEAT00_GUARD_CF`: compiled with Control Flow Guard.
    #[must_use]
    pub fn guard_cf(self) -> bool {
        self.0 & FEAT00_GUARD_CF != 0
    }

    /// `FEAT00_GUARD_EHCONT`: compiled with EH continuation metadata.
    #[must_use]
    pub fn guard_ehcont(self) -> bool {
        self.0 & FEAT00_GUARD_EHCONT != 0
    }

    /// `FEAT00_KERNEL`: compiled for kernel mode.
    #[must_use]
    pub fn kernel(self) -> bool {
        self.0 & FEAT00_KERNEL != 0
    }
}

/// A parsed COFF object file.
///
/// Parsing validates the file header, and that the section table and the
/// symbol and string tables are inside the file, and stops. Sections,
/// symbols and relocations are decoded on access; nothing is allocated.
#[derive(Clone, Copy, Debug)]
pub struct CoffObject<'a> {
    data: &'a [u8],
    source: Source<'a>,
    header: FileHeader,
    sections: SectionTable<'a>,
    symbols: SymbolTable<'a>,
}

impl<'a> CoffObject<'a> {
    /// Parses a COFF object (regular or `/bigobj`).
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` if the header is truncated, the file is a
    /// short import object or a PE image, or the section, symbol or string
    /// table is out of bounds.
    pub fn parse(data: &'a [u8], source: Source<'a>) -> Result<Self> {
        let header = if is_bigobj(data) {
            FileHeader::parse_bigobj(data)
        } else if is_import_object(data) {
            return Err(source.malformed(0, "COFF object (a short import object)"));
        } else if data.starts_with(&[0, 0, 0xff, 0xff]) {
            return Err(
                source.malformed(0, "COFF object (an anonymous object, such as MSVC /GL IR)")
            );
        } else if data.starts_with(b"MZ") {
            return Err(source.malformed(0, "COFF object (a PE image)"));
        } else {
            FileHeader::parse_regular(data)
        }
        .ok_or_else(|| source.malformed(0, "COFF file header (truncated)"))?;

        let table_offset = header
            .header_size()
            .checked_add(usize::from(header.size_of_optional_header))
            .ok_or_else(|| source.malformed(16, "optional header size"))?;
        let table_size =
            u64::from(header.number_of_sections).saturating_mul(to_u64(SECTION_HEADER_SIZE));
        let table = subslice(data, to_u64(table_offset), table_size).ok_or_else(|| {
            source.malformed(to_u64(table_offset), "section table (out of bounds)")
        })?;
        let sections = SectionTable::from_bytes(table, to_u64(table_offset));
        let symbols = SymbolTable::parse(
            data,
            header.pointer_to_symbol_table,
            header.number_of_symbols,
            header.bigobj,
            source,
        )?;
        Ok(Self {
            data,
            source,
            header,
            sections,
            symbols,
        })
    }

    /// The whole file.
    #[must_use]
    pub fn data(&self) -> &'a [u8] {
        self.data
    }

    /// The file identity used in error messages.
    #[must_use]
    pub fn source(&self) -> Source<'a> {
        self.source
    }

    /// The file header.
    #[must_use]
    pub fn header(&self) -> &FileHeader {
        &self.header
    }

    /// `Machine` (`IMAGE_FILE_MACHINE_*`).
    #[must_use]
    pub fn machine(&self) -> u16 {
        self.header.machine
    }

    /// The target architecture, if qld knows the machine.
    #[must_use]
    pub fn architecture(&self) -> Option<Architecture> {
        machine_architecture(self.header.machine)
    }

    /// Whether the object uses the `/bigobj` layout.
    #[must_use]
    pub fn is_bigobj(&self) -> bool {
        self.header.bigobj
    }

    /// Number of sections.
    #[must_use]
    pub fn section_count(&self) -> u32 {
        self.header.number_of_sections
    }

    /// The section header table.
    #[must_use]
    pub fn section_table(&self) -> SectionTable<'a> {
        self.sections
    }

    /// The symbol table.
    #[must_use]
    pub fn symbols(&self) -> SymbolTable<'a> {
        self.symbols
    }

    /// Decodes symbol record `index`; see [`SymbolTable::get`].
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` if the record cannot be decoded.
    pub fn symbol(&self, index: u32) -> Result<Symbol<'a>> {
        self.symbols.get(index)
    }

    /// Decodes section `number` (1-based) and resolves its name.
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` if `number` is out of range or the name
    /// refers outside the string table.
    pub fn section(&self, number: u32) -> Result<Section<'a>> {
        let header = self.sections.get(number).ok_or_else(|| {
            self.source.malformed(
                self.sections.file_offset(),
                format!("section number {number} (out of range)"),
            )
        })?;
        let name = self
            .sections
            .name(number, &self.symbols.strings())
            .ok_or_else(|| {
                self.source.malformed(
                    self.sections.header_offset(number),
                    format!("section name (section {number})"),
                )
            })?;
        Ok(Section {
            number,
            header,
            name,
        })
    }

    /// Iterates over the sections in table order.
    pub fn sections(&self) -> impl Iterator<Item = Result<Section<'a>>> + '_ {
        (1..=self.header.number_of_sections).map(|number| self.section(number))
    }

    /// The sections named `name`, in table order.
    pub fn sections_named<'n>(
        &self,
        name: &'n [u8],
    ) -> impl Iterator<Item = Result<Section<'a>>> + use<'_, 'a, 'n> {
        self.sections()
            .filter(move |section| section.as_ref().map_or(true, |s| s.name == name))
    }

    /// The contents of a section.
    ///
    /// Sections without contents (`PointerToRawData` of 0, as for `.bss`)
    /// return an empty slice.
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` if the contents are out of bounds.
    pub fn section_data(&self, header: &SectionHeader) -> Result<&'a [u8]> {
        if header.pointer_to_raw_data == 0 {
            return Ok(&[]);
        }
        let offset = u64::from(header.pointer_to_raw_data);
        subslice(self.data, offset, u64::from(header.size_of_raw_data)).ok_or_else(|| {
            self.source
                .malformed(offset, "section contents (out of bounds)")
        })
    }

    /// The relocations of a section, with the `IMAGE_SCN_LNK_NRELOC_OVFL`
    /// extended count applied.
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` if the relocations are out of bounds or an
    /// extended count is invalid.
    pub fn relocations(&self, header: &SectionHeader) -> Result<Relocations<'a>> {
        let mut offset = u64::from(header.pointer_to_relocations);
        let mut count = u64::from(header.number_of_relocations);
        if count == 0 {
            return Ok(Relocations::default());
        }
        if header.has_extended_relocations() {
            let first = subslice(self.data, offset, to_u64(RELOCATION_SIZE))
                .and_then(|b| b.first_chunk::<RELOCATION_SIZE>())
                .ok_or_else(|| self.source.malformed(offset, "relocations (out of bounds)"))?;
            let total = Relocation::decode(first).virtual_address;
            count = u64::from(total).checked_sub(1).ok_or_else(|| {
                self.source
                    .malformed(offset, "extended relocation count (zero)")
            })?;
            offset = offset.saturating_add(to_u64(RELOCATION_SIZE));
        }
        let bytes = subslice(
            self.data,
            offset,
            count.saturating_mul(to_u64(RELOCATION_SIZE)),
        )
        .ok_or_else(|| self.source.malformed(offset, "relocations (out of bounds)"))?;
        let (records, _) = bytes.as_chunks::<RELOCATION_SIZE>();
        Ok(Relocations::new(records, offset))
    }

    /// The linker directives of every `.drectve` section, in section order
    /// (objects normally have at most one).
    pub fn directives(&self) -> impl Iterator<Item = Result<Directives<'a>>> + '_ {
        self.sections_named(DRECTVE_SECTION).map(|section| {
            let section = section?;
            let data = self.section_data(&section.header)?;
            Ok(parse_directives(
                data,
                u64::from(section.header.pointer_to_raw_data),
                self.source,
            ))
        })
    }

    /// The address-significance table (`.llvm_addrsig`), if present.
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` if a section cannot be decoded.
    pub fn addrsig(&self) -> Result<Option<AddrsigIter<'a>>> {
        match self.sections_named(ADDRSIG_SECTION).next() {
            Some(section) => Ok(Some(AddrsigIter::new(self.section_data(&section?.header)?))),
            None => Ok(None),
        }
    }

    /// The `@feat.00` flags: the value of the absolute symbol of that name,
    /// if present.
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` if the symbol table cannot be decoded.
    pub fn feat00(&self) -> Result<Option<Feat00>> {
        for symbol in self.symbols.iter() {
            let symbol = symbol?;
            if symbol.name == FEAT00_SYMBOL && symbol.is_absolute() {
                return Ok(Some(Feat00(symbol.value)));
            }
        }
        Ok(None)
    }

    /// The resource sections (`.rsrc`, `.rsrc$01`, `.rsrc$02`), in table
    /// order.
    pub fn resource_sections(&self) -> impl Iterator<Item = Result<Section<'a>>> + '_ {
        self.sections()
            .filter(|section| section.as_ref().map_or(true, Section::is_resource))
    }
}
