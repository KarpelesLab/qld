//! Relocatable objects (`ET_REL`).

use super::consts::{
    ELF_NOTE_GNU, EM_X86_64, ET_REL, NT_GNU_PROPERTY_TYPE_0, SHT_GROUP, SHT_NOTE, SHT_REL,
    SHT_RELA, SHT_SYMTAB, SHT_SYMTAB_SHNDX, SHT_X86_64_UNWIND,
};
use super::eh_frame::{EhFrameEntry, split_eh_frame};
use super::file::ElfFile;
use super::format::{ElfFormat, RawRecord};
use super::group::Group;
use super::note::{GnuProperties, NoteIter};
use super::reloc::{RelaSlice, RelocationSection, Relocations};
use super::section::{CompressionHeader, SectionHeader};
use super::source::{Source, to_u64};
use super::symbol::{SectionIndex, SymbolTable};
use crate::error::Result;
use crate::target::Architecture;

/// Prefix of the sections holding GCC LTO IR.
pub const GCC_LTO_SECTION_PREFIX: &[u8] = b".gnu.lto_";

/// A parsed relocatable object file.
///
/// Parsing validates the headers, locates the symbol table with its string
/// table and extended index table, and stops: it reads O(number of sections)
/// fixed-size headers and allocates nothing. Section, symbol and relocation
/// counts are known immediately, so later stages can preallocate.
#[derive(Debug)]
pub struct ObjectFile<'a, F: ElfFormat> {
    elf: ElfFile<'a, F>,
    symbols: SymbolTable<'a, F>,
}

impl<F: ElfFormat> Clone for ObjectFile<'_, F> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<F: ElfFormat> Copy for ObjectFile<'_, F> {}

impl<'a, F: ElfFormat> ObjectFile<'a, F> {
    /// Parses a relocatable object.
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` if the file is not a valid `ET_REL` ELF file
    /// of format `F`, has more than one `SHT_SYMTAB`, or its symbol table,
    /// string table or extended index table is out of bounds or badly sized.
    pub fn parse(data: &'a [u8], source: Source<'a>) -> Result<Self> {
        let elf = ElfFile::<F>::parse(data, source)?;
        if elf.header().e_type != ET_REL {
            return Err(source.malformed(16, "ELF file type (expected a relocatable object)"));
        }

        let mut symtab: Option<(u32, SectionHeader)> = None;
        let mut shndx: Option<(u32, SectionHeader)> = None;
        for (index, hdr) in elf.enumerate_sections() {
            match hdr.sh_type {
                SHT_SYMTAB => {
                    if symtab.is_some() {
                        return Err(source.malformed(
                            elf.section_header_offset(index),
                            "symbol table (more than one SHT_SYMTAB)",
                        ));
                    }
                    symtab = Some((index, hdr));
                }
                SHT_SYMTAB_SHNDX if shndx.is_none() => shndx = Some((index, hdr)),
                _ => {}
            }
        }

        let symbols = match symtab {
            None => SymbolTable::empty(source),
            Some((index, hdr)) => {
                // Usually the only SHT_SYMTAB_SHNDX; otherwise search for
                // the one linked to this symbol table.
                let shndx = match shndx {
                    Some((_, s)) if s.sh_link == index => Some((0, s)),
                    Some(_) => elf
                        .enumerate_sections()
                        .find(|(_, s)| s.sh_type == SHT_SYMTAB_SHNDX && s.sh_link == index),
                    None => None,
                };
                elf.symbol_table_with_shndx(index, &hdr, shndx.map(|(_, h)| h))?
            }
        };
        Ok(Self { elf, symbols })
    }

    /// The underlying ELF file view.
    #[inline]
    #[must_use]
    pub fn elf(&self) -> &ElfFile<'a, F> {
        &self.elf
    }

    /// The whole file.
    #[inline]
    #[must_use]
    pub fn data(&self) -> &'a [u8] {
        self.elf.data()
    }

    /// The file identity used in error messages.
    #[inline]
    #[must_use]
    pub fn source(&self) -> Source<'a> {
        self.elf.source()
    }

    /// The target architecture, if qld knows `e_machine`.
    #[inline]
    #[must_use]
    pub fn architecture(&self) -> Option<Architecture> {
        self.elf.architecture()
    }

    /// Number of sections, including section 0.
    #[inline]
    #[must_use]
    pub fn section_count(&self) -> usize {
        self.elf.section_count()
    }

    /// Decodes section header `index`.
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` if `index` is out of range.
    #[inline]
    pub fn section_header(&self, index: u32) -> Result<SectionHeader> {
        self.elf.section_header(index)
    }

    /// Returns the name of a section.
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` if the name cannot be read.
    #[inline]
    pub fn section_name(&self, header: &SectionHeader) -> Result<&'a [u8]> {
        self.elf.section_name(header)
    }

    /// Returns the contents of a section (empty for `SHT_NOBITS`).
    ///
    /// For `SHF_COMPRESSED` sections this is the compression header followed
    /// by the compressed stream; see [`compressed_data`](Self::compressed_data).
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` if the contents lie outside the file.
    #[inline]
    pub fn section_data(&self, header: &SectionHeader) -> Result<&'a [u8]> {
        self.elf.section_data(header)
    }

    /// The symbol table (empty if the object has none).
    #[inline]
    #[must_use]
    pub fn symbols(&self) -> &SymbolTable<'a, F> {
        &self.symbols
    }

    /// Number of symbols, including the null symbol.
    #[inline]
    #[must_use]
    pub fn symbol_count(&self) -> usize {
        self.symbols.len()
    }

    /// Reads the relocations of a `SHT_REL` or `SHT_RELA` section.
    ///
    /// Returns `Ok(None)` for other section types.
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` if the contents are out of bounds or not
    /// a whole number of entries.
    #[inline]
    pub fn relocation_section(
        &self,
        index: u32,
        header: &SectionHeader,
    ) -> Result<Option<RelocationSection<'a, F>>> {
        self.elf.relocation_section(index, header)
    }

    /// Iterates over the `SHT_REL` / `SHT_RELA` sections.
    pub fn relocation_sections(
        &self,
    ) -> impl Iterator<Item = Result<RelocationSection<'a, F>>> + use<'a, F> {
        let this = *self;
        self.elf
            .enumerate_sections()
            .filter(|(_, hdr)| matches!(hdr.sh_type, SHT_REL | SHT_RELA))
            .filter_map(move |(index, hdr)| this.relocation_section(index, &hdr).transpose())
    }

    /// Maps each section index to the index of the relocation section that
    /// targets it, or 0 when none does.
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` if a relocation section targets an
    /// out-of-range section or a section already targeted by another one.
    pub fn relocation_map(&self) -> Result<Vec<u32>> {
        let mut map = vec![0u32; self.section_count()];
        for (index, hdr) in self.elf.enumerate_sections() {
            if !matches!(hdr.sh_type, SHT_REL | SHT_RELA) {
                continue;
            }
            let offset = self.elf.section_header_offset(index);
            let slot = usize::try_from(hdr.sh_info)
                .ok()
                .and_then(|t| map.get_mut(t))
                .ok_or_else(|| {
                    self.source()
                        .malformed(offset, "relocation section target (out of range)")
                })?;
            if *slot != 0 {
                return Err(self
                    .source()
                    .malformed(offset, "relocation section target (duplicate)"));
            }
            *slot = index;
        }
        Ok(map)
    }

    /// Reads the `SHT_GROUP` section `index`.
    ///
    /// Returns `Ok(None)` if the section is not a group.
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` if the contents are out of bounds, empty,
    /// or not a whole number of 4-byte words.
    pub fn group(&self, index: u32, header: &SectionHeader) -> Result<Option<Group<'a, F>>> {
        if header.sh_type != SHT_GROUP {
            return Ok(None);
        }
        let words = self
            .elf
            .section_records::<[u8; 4]>(header, "section group")?;
        Group::new(index, *header, words).map(Some).ok_or_else(|| {
            self.source()
                .malformed(header.sh_offset, "section group (empty)")
        })
    }

    /// Iterates over the `SHT_GROUP` sections.
    pub fn groups(&self) -> impl Iterator<Item = Result<Group<'a, F>>> + use<'a, F> {
        let this = *self;
        self.elf
            .enumerate_sections()
            .filter(|(_, hdr)| hdr.sh_type == SHT_GROUP)
            .filter_map(move |(index, hdr)| this.group(index, &hdr).transpose())
    }

    /// Returns the signature of a group: the name of its signature symbol, or
    /// the name of the section for a section symbol with an empty name.
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` if the group does not link to the object's
    /// symbol table or the symbol cannot be read.
    pub fn group_signature(&self, group: &Group<'a, F>) -> Result<&'a [u8]> {
        if group.symtab != self.symbols.section_index() || self.symbols.is_empty() {
            return Err(self
                .source()
                .malformed(group.header.sh_offset, "section group symbol table link"));
        }
        let index = usize::try_from(group.signature_symbol).unwrap_or(usize::MAX);
        let symbol = self.symbols.get(index)?;
        if symbol.name.is_empty()
            && symbol.is_section()
            && let SectionIndex::Section(section) = symbol.section
        {
            let header = self.section_header(section)?;
            return self.section_name(&header);
        }
        Ok(symbol.name)
    }

    /// Splits a compressed section (`SHF_COMPRESSED`) into its compression
    /// header and the compressed stream.
    ///
    /// Returns `Ok(None)` if the section is not compressed.
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` if the contents are out of bounds or too
    /// small to hold a compression header.
    pub fn compressed_data(
        &self,
        header: &SectionHeader,
    ) -> Result<Option<(CompressionHeader, &'a [u8])>> {
        if !header.is_compressed() {
            return Ok(None);
        }
        let data = self.section_data(header)?;
        let (chdrs, _) = F::Chdr::slice_from(data);
        let chdr = chdrs.first().ok_or_else(|| {
            self.source()
                .malformed(header.sh_offset, "compression header (truncated)")
        })?;
        let rest = data.get(F::Chdr::SIZE..).unwrap_or_default();
        Ok(Some((F::decode_chdr(chdr), rest)))
    }

    /// Merges the GNU properties of every `SHT_NOTE` section named
    /// `.note.gnu.property`.
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` if a property note is malformed.
    pub fn gnu_properties(&self) -> Result<GnuProperties> {
        let mut props = GnuProperties::default();
        let machine = self.elf.header().e_machine;
        for (_, hdr) in self.elf.enumerate_sections() {
            if hdr.sh_type != SHT_NOTE || self.section_name(&hdr)? != b".note.gnu.property" {
                continue;
            }
            let data = self.section_data(&hdr)?;
            for note in
                NoteIter::<F::Endian>::new(data, hdr.sh_addralign, hdr.sh_offset, self.source())
            {
                let note = note?;
                if note.name == ELF_NOTE_GNU && note.n_type == NT_GNU_PROPERTY_TYPE_0 {
                    let at = hdr.sh_offset.saturating_add(to_u64(note.offset));
                    props.merge_note::<F::Endian>(
                        note.desc,
                        F::WORD_SIZE,
                        machine,
                        at,
                        self.source(),
                    )?;
                }
            }
        }
        Ok(props)
    }

    /// Whether a section is an `.eh_frame` section (by name, or by the
    /// x86-64 `SHT_X86_64_UNWIND` type).
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` if the section name cannot be read.
    pub fn is_eh_frame(&self, header: &SectionHeader) -> Result<bool> {
        if header.sh_type == SHT_X86_64_UNWIND && self.elf.header().e_machine == EM_X86_64 {
            return Ok(true);
        }
        Ok(self.section_name(header)? == b".eh_frame")
    }

    /// Splits `.eh_frame` section `index` into records, associating the
    /// relocations of `relocations` (the relocation section targeting it, if
    /// any) with each record.
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` for malformed records or relocations; see
    /// [`split_eh_frame`].
    pub fn eh_frame(
        &self,
        header: &SectionHeader,
        relocations: Option<&RelocationSection<'a, F>>,
    ) -> Result<Vec<EhFrameEntry<'a>>> {
        let data = self.section_data(header)?;
        let relocs = relocations.map_or(Relocations::Rela(RelaSlice::default()), |r| r.relocations);
        split_eh_frame::<F>(data, relocs, header.sh_offset, self.source())
    }

    /// Whether the object contains GCC LTO IR (sections named `.gnu.lto_*`).
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` if a section name cannot be read.
    pub fn has_gcc_lto_ir(&self) -> Result<bool> {
        for (_, hdr) in self.elf.enumerate_sections() {
            if self.section_name(&hdr)?.starts_with(GCC_LTO_SECTION_PREFIX) {
                return Ok(true);
            }
        }
        Ok(false)
    }
}
