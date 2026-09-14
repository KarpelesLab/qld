//! The parts common to every ELF file: header, section and program header
//! tables, and section names.

use super::consts::{
    EI_VERSION, ELFMAG, EV_CURRENT, PN_XNUM, SHN_UNDEF, SHN_XINDEX, SHT_REL, SHT_RELA, SHT_RELR,
    SHT_SYMTAB_SHNDX,
};
use super::format::{ElfFormat, Endian, RawRecord};
use super::header::FileHeader;
use super::reloc::{RelSlice, RelaSlice, RelocationSection, Relocations, RelrSlice};
use super::section::{SectionHeader, SectionTable};
use super::segment::{ProgramHeader, ProgramHeaderTable};
use super::source::{Source, entry_offset, subslice, to_u64};
use super::strtab::StringTable;
use super::symbol::SymbolTable;
use crate::error::Result;
use crate::target::Architecture;

/// Offsets of header fields, for error messages.
const E_VERSION_OFFSET: u64 = 20;
const E_SHENTSIZE_OFFSET_64: u64 = 58;
const E_SHENTSIZE_OFFSET_32: u64 = 46;

/// A validated ELF file of format `F`: the header, the section header table
/// (with extended numbering resolved), the program header table and the
/// section name string table.
///
/// Construction reads a fixed amount of data regardless of file size; nothing
/// is copied.
#[derive(Debug)]
pub struct ElfFile<'a, F: ElfFormat> {
    data: &'a [u8],
    source: Source<'a>,
    header: FileHeader,
    sections: SectionTable<'a, F>,
    segments: ProgramHeaderTable<'a, F>,
    shstrtab: StringTable<'a>,
    shstrndx: u32,
}

impl<F: ElfFormat> Clone for ElfFile<'_, F> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<F: ElfFormat> Copy for ElfFile<'_, F> {}

impl<'a, F: ElfFormat> ElfFile<'a, F> {
    /// Parses and validates the ELF header and header tables.
    ///
    /// Any file type is accepted; see [`ObjectFile`](super::ObjectFile) and
    /// [`SharedObject`](super::SharedObject) for type-specific readers.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Malformed`](crate::Error::Malformed) if the magic,
    /// class, byte order or version do not match `F`, if an entry size is
    /// wrong, or if a table or the section name string table lies outside
    /// the file.
    pub fn parse(data: &'a [u8], source: Source<'a>) -> Result<Self> {
        let (ehdrs, _) = F::Ehdr::slice_from(data);
        let Some(raw_header) = ehdrs.first() else {
            return Err(source.malformed(0, "ELF header (file too small)"));
        };
        if data.first_chunk::<4>() != Some(&ELFMAG) {
            return Err(source.malformed(0, "ELF magic"));
        }
        let header = F::decode_ehdr(raw_header);
        if header.class != F::CLASS {
            return Err(source.malformed(4, "ELF class"));
        }
        if header.data != F::Endian::ELF_DATA {
            return Err(source.malformed(5, "ELF data encoding"));
        }
        if header.ident_version != EV_CURRENT {
            return Err(source.malformed(to_u64(EI_VERSION), "ELF identification version"));
        }
        if header.e_version != u32::from(EV_CURRENT) {
            return Err(source.malformed(E_VERSION_OFFSET, "ELF version"));
        }

        let mut shstrndx = u32::from(header.e_shstrndx);
        let mut phnum = u64::from(header.e_phnum);
        let mut sections = SectionTable::default();

        if header.e_shoff != 0 {
            if usize::from(header.e_shentsize) != F::Shdr::SIZE {
                let at = if F::Shdr::SIZE == 64 {
                    E_SHENTSIZE_OFFSET_64
                } else {
                    E_SHENTSIZE_OFFSET_32
                };
                return Err(source.malformed(at, "section header entry size"));
            }
            let shdr_size = to_u64(F::Shdr::SIZE);
            let first = subslice(data, header.e_shoff, shdr_size)
                .and_then(|b| F::Shdr::slice_from(b).0.first())
                .ok_or_else(|| {
                    source.malformed(header.e_shoff, "section header table (out of bounds)")
                })?;
            let section0 = F::decode_shdr(first);
            let shnum = if header.e_shnum == 0 {
                section0.sh_size
            } else {
                u64::from(header.e_shnum)
            };
            if header.e_shstrndx == SHN_XINDEX {
                shstrndx = section0.sh_link;
            }
            if header.e_phnum == PN_XNUM {
                phnum = u64::from(section0.sh_info);
            }
            if shnum > u64::from(u32::MAX) {
                return Err(source.malformed(header.e_shoff, "section count"));
            }
            let bytes = shnum
                .checked_mul(shdr_size)
                .and_then(|size| subslice(data, header.e_shoff, size))
                .ok_or_else(|| {
                    source.malformed(header.e_shoff, "section header table (out of bounds)")
                })?;
            sections = SectionTable::new(F::Shdr::slice_from(bytes).0, header.e_shoff);
        } else if header.e_phnum == PN_XNUM {
            return Err(source.malformed(0, "program header count (PN_XNUM without sections)"));
        }

        let mut segments = ProgramHeaderTable::default();
        if phnum != 0 {
            if usize::from(header.e_phentsize) != F::Phdr::SIZE {
                return Err(source.malformed(0, "program header entry size"));
            }
            let bytes = phnum
                .checked_mul(to_u64(F::Phdr::SIZE))
                .and_then(|size| subslice(data, header.e_phoff, size))
                .ok_or_else(|| {
                    source.malformed(header.e_phoff, "program header table (out of bounds)")
                })?;
            segments = ProgramHeaderTable::new(F::Phdr::slice_from(bytes).0);
        }

        let mut file = Self {
            data,
            source,
            header,
            sections,
            segments,
            shstrtab: StringTable::default(),
            shstrndx,
        };

        if shstrndx != u32::from(SHN_UNDEF) && !sections.is_empty() {
            let hdr = file
                .section_header(shstrndx)
                .map_err(|_| source.malformed(header.e_shoff, "section name string table index"))?;
            let bytes = file.section_data(&hdr)?;
            file.shstrtab = StringTable::new(bytes, hdr.sh_offset);
        }
        Ok(file)
    }

    /// The whole file.
    #[inline]
    #[must_use]
    pub fn data(&self) -> &'a [u8] {
        self.data
    }

    /// The file identity used in error messages.
    #[inline]
    #[must_use]
    pub fn source(&self) -> Source<'a> {
        self.source
    }

    /// The decoded ELF header.
    #[inline]
    #[must_use]
    pub fn header(&self) -> &FileHeader {
        &self.header
    }

    /// The target architecture, if qld knows `e_machine`.
    #[inline]
    #[must_use]
    pub fn architecture(&self) -> Option<Architecture> {
        self.header.architecture()
    }

    /// The section header table.
    #[inline]
    #[must_use]
    pub fn sections(&self) -> SectionTable<'a, F> {
        self.sections
    }

    /// Number of sections, after extended numbering (includes section 0).
    #[inline]
    #[must_use]
    pub fn section_count(&self) -> usize {
        self.sections.len()
    }

    /// Index of the section name string table, after extended numbering.
    #[inline]
    #[must_use]
    pub fn shstrndx(&self) -> u32 {
        self.shstrndx
    }

    /// The section name string table.
    #[inline]
    #[must_use]
    pub fn shstrtab(&self) -> StringTable<'a> {
        self.shstrtab
    }

    /// The program header table.
    #[inline]
    #[must_use]
    pub fn segments(&self) -> ProgramHeaderTable<'a, F> {
        self.segments
    }

    /// Iterates over `(index, header)` for every section, including 0.
    pub fn enumerate_sections(
        &self,
    ) -> impl ExactSizeIterator<Item = (u32, SectionHeader)> + use<'a, F> {
        // `parse` rejects more than `u32::MAX` sections, so the fallback is
        // unreachable.
        self.sections
            .iter()
            .enumerate()
            .map(|(i, hdr)| (u32::try_from(i).unwrap_or(u32::MAX), hdr))
    }

    /// Decodes section header `index`.
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` if `index` is out of range.
    #[inline]
    pub fn section_header(&self, index: u32) -> Result<SectionHeader> {
        self.sections.get(index).ok_or_else(|| {
            self.source.malformed(
                self.sections.file_offset(),
                format!("section index {index} (out of range)"),
            )
        })
    }

    /// Returns the name of a section.
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` if the name offset is outside the section
    /// name string table or not NUL-terminated.
    #[inline]
    pub fn section_name(&self, header: &SectionHeader) -> Result<&'a [u8]> {
        self.shstrtab.get(header.sh_name).ok_or_else(|| {
            self.source.malformed(
                self.shstrtab.file_offset(),
                format!("section name offset {:#x}", header.sh_name),
            )
        })
    }

    /// Returns the contents of a section. `SHT_NOBITS` sections are empty.
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` if the contents lie outside the file.
    #[inline]
    pub fn section_data(&self, header: &SectionHeader) -> Result<&'a [u8]> {
        if header.is_nobits() {
            return Ok(&[]);
        }
        subslice(self.data, header.sh_offset, header.sh_size).ok_or_else(|| {
            self.source
                .malformed(header.sh_offset, "section contents (out of bounds)")
        })
    }

    /// Returns the contents of a section as fixed-size records.
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` if the contents lie outside the file, or
    /// their size is not a multiple of the record size, or `sh_entsize` is
    /// set to something other than the record size.
    pub fn section_records<R: RawRecord>(
        &self,
        header: &SectionHeader,
        what: &str,
    ) -> Result<&'a [R]> {
        let bytes = self.section_data(header)?;
        if header.sh_entsize != 0 && header.sh_entsize != to_u64(R::SIZE) {
            return Err(self
                .source
                .malformed(header.sh_offset, format!("{what} (bad sh_entsize)")));
        }
        let (records, rest) = R::slice_from(bytes);
        if !rest.is_empty() {
            return Err(self.source.malformed(
                header.sh_offset,
                format!("{what} (size is not a multiple of the entry size)"),
            ));
        }
        Ok(records)
    }

    /// Finds the first section with the given name.
    ///
    /// Sections whose names cannot be read are skipped.
    #[must_use]
    pub fn section_by_name(&self, name: &[u8]) -> Option<(u32, SectionHeader)> {
        self.enumerate_sections()
            .find(|(_, hdr)| self.shstrtab.get(hdr.sh_name) == Some(name))
    }

    /// Returns the contents of a segment.
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` if the contents lie outside the file.
    pub fn segment_data(&self, header: &ProgramHeader) -> Result<&'a [u8]> {
        subslice(self.data, header.p_offset, header.p_filesz).ok_or_else(|| {
            self.source
                .malformed(header.p_offset, "segment contents (out of bounds)")
        })
    }

    /// Loads the symbol table in section `index` (`SHT_SYMTAB` or
    /// `SHT_DYNSYM`), with its string table and, if one links to it, its
    /// `SHT_SYMTAB_SHNDX` table.
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` if a table is out of bounds or badly sized,
    /// a link is out of range, or `sh_info` exceeds the symbol count.
    pub fn symbol_table(&self, index: u32) -> Result<SymbolTable<'a, F>> {
        let header = self.section_header(index)?;
        let shndx = self
            .enumerate_sections()
            .find(|(_, h)| h.sh_type == SHT_SYMTAB_SHNDX && h.sh_link == index)
            .map(|(_, h)| h);
        self.symbol_table_with_shndx(index, &header, shndx)
    }

    /// [`symbol_table`](Self::symbol_table) with the extended index table
    /// already located.
    pub(crate) fn symbol_table_with_shndx(
        &self,
        index: u32,
        header: &SectionHeader,
        shndx: Option<SectionHeader>,
    ) -> Result<SymbolTable<'a, F>> {
        let raw = self.section_records::<F::Sym>(header, "symbol table")?;
        let at = self.section_header_offset(index);
        let strtab_header = self
            .section_header(header.sh_link)
            .map_err(|_| self.source.malformed(at, "symbol table string table link"))?;
        let strtab = StringTable::new(self.section_data(&strtab_header)?, strtab_header.sh_offset);
        if usize::try_from(header.sh_info).map_or(true, |n| n > raw.len()) {
            return Err(self
                .source
                .malformed(at, "symbol table first global index (sh_info)"));
        }
        let shndx = match shndx {
            Some(h) => self.section_records::<[u8; 4]>(&h, "extended section index table")?,
            None => &[],
        };
        Ok(SymbolTable::new(
            raw,
            strtab,
            shndx,
            header.sh_info,
            index,
            header.sh_offset,
            self.source,
        ))
    }

    /// Reads the relocations of a `SHT_REL` or `SHT_RELA` section.
    ///
    /// Returns `Ok(None)` for other section types.
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` if the contents are out of bounds or not
    /// a whole number of entries.
    pub fn relocation_section(
        &self,
        index: u32,
        header: &SectionHeader,
    ) -> Result<Option<RelocationSection<'a, F>>> {
        let relocations = match header.sh_type {
            SHT_RELA => Relocations::Rela(RelaSlice::new(
                self.section_records::<F::Rela>(header, "relocation section")?,
            )),
            SHT_REL => Relocations::Rel(RelSlice::new(
                self.section_records::<F::Rel>(header, "relocation section")?,
            )),
            _ => return Ok(None),
        };
        Ok(Some(RelocationSection {
            index,
            header: *header,
            target: header.sh_info,
            symtab: header.sh_link,
            relocations,
        }))
    }

    /// Reads an `SHT_RELR` section.
    ///
    /// Returns `Ok(None)` for other section types.
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` if the contents are out of bounds or not
    /// a whole number of words.
    pub fn relr_section(&self, header: &SectionHeader) -> Result<Option<RelrSlice<'a, F>>> {
        if header.sh_type != SHT_RELR {
            return Ok(None);
        }
        let words = self.section_records::<F::Word>(header, "RELR relocation section")?;
        Ok(Some(RelrSlice::new(words)))
    }

    /// File offset of section header `index`, for error messages.
    #[must_use]
    pub fn section_header_offset(&self, index: u32) -> u64 {
        let index = usize::try_from(index).unwrap_or(usize::MAX);
        entry_offset(self.sections.file_offset(), index, F::Shdr::SIZE)
    }
}

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)]
mod tests {
    use super::*;
    use crate::elf::read::consts::{EM_X86_64, ET_REL, SHT_PROGBITS, SHT_STRTAB};
    use crate::elf::read::format::{Elf32Le, Elf64Be, Elf64Le};
    use std::path::Path;

    fn shdr(name: u32, sh_type: u32, offset: u64, size: u64, link: u32) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&name.to_le_bytes());
        v.extend_from_slice(&sh_type.to_le_bytes());
        v.extend_from_slice(&[0; 16]); // flags, addr
        v.extend_from_slice(&offset.to_le_bytes());
        v.extend_from_slice(&size.to_le_bytes());
        v.extend_from_slice(&link.to_le_bytes());
        v.extend_from_slice(&[0; 20]); // info, addralign, entsize
        v
    }

    /// A minimal ELF64LE object that uses extended section numbering.
    fn extended_object() -> Vec<u8> {
        let strtab = b"\0.text\0.shstrtab\0";
        let mut v = Vec::new();
        v.extend_from_slice(b"\x7fELF\x02\x01\x01\0\0\0\0\0\0\0\0\0");
        v.extend_from_slice(&ET_REL.to_le_bytes());
        v.extend_from_slice(&EM_X86_64.to_le_bytes());
        v.extend_from_slice(&1u32.to_le_bytes());
        v.extend_from_slice(&[0; 16]); // entry, phoff
        v.extend_from_slice(&64u64.to_le_bytes()); // shoff
        v.extend_from_slice(&0u32.to_le_bytes()); // flags
        v.extend_from_slice(&64u16.to_le_bytes()); // ehsize
        v.extend_from_slice(&[0; 4]); // phentsize, phnum
        v.extend_from_slice(&64u16.to_le_bytes()); // shentsize
        v.extend_from_slice(&0u16.to_le_bytes()); // shnum: see section 0
        v.extend_from_slice(&SHN_XINDEX.to_le_bytes()); // shstrndx: see section 0
        assert_eq!(v.len(), 64);
        v.extend(shdr(0, 0, 0, 3, 2));
        v.extend(shdr(1, SHT_PROGBITS, 256, 0, 0));
        v.extend(shdr(7, SHT_STRTAB, 256, strtab.len() as u64, 0));
        v.extend_from_slice(strtab);
        v
    }

    #[test]
    fn resolves_extended_numbering() {
        let data = extended_object();
        let file = ElfFile::<Elf64Le>::parse(&data, Source::new(Path::new("x.o"))).unwrap();
        assert_eq!(file.section_count(), 3);
        assert_eq!(file.shstrndx(), 2);
        let text = file.section_header(1).unwrap();
        assert_eq!(file.section_name(&text).unwrap(), b".text");
        assert_eq!(file.section_by_name(b".shstrtab").map(|s| s.0), Some(2));
        assert!(file.section_header(3).is_err());
        assert_eq!(
            file.architecture(),
            Some(crate::target::Architecture::X86_64)
        );
    }

    #[test]
    fn rejects_bad_headers() {
        let src = Source::new(Path::new("x.o"));
        let good = extended_object();
        let expect_err = |data: &[u8], what: &str| {
            let err = ElfFile::<Elf64Le>::parse(data, src).unwrap_err();
            assert!(err.to_string().contains(what), "{err} (expected {what})");
        };
        let mut bad = good.clone();
        bad[0] = 0;
        expect_err(&bad, "magic");
        let mut bad = good.clone();
        bad[6] = 2;
        expect_err(&bad, "identification version");
        let mut bad = good.clone();
        bad[58] = 40;
        expect_err(&bad, "section header entry size");
        let mut bad = good.clone();
        bad[40] = 0xf0;
        expect_err(&bad, "section header table");
        let mut bad = good.clone();
        bad[64 + 40] = 9; // section 0 sh_link: shstrndx out of range
        expect_err(&bad, "section name string table index");
        let mut bad = good.clone();
        bad[64 + 32] = 0xff; // section 0 sh_size: too many sections
        expect_err(&bad, "section header table");
        expect_err(&good[..63], "file too small");

        assert!(ElfFile::<Elf32Le>::parse(&good, src).is_err());
        assert!(ElfFile::<Elf64Be>::parse(&good, src).is_err());
        for len in 0..good.len() {
            let _ = ElfFile::<Elf64Le>::parse(&good[..len], src);
        }
    }
}
