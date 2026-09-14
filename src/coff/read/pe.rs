//! PE images (DLLs and executables), read so that qld can link directly
//! against a `.dll`, as GNU ld does.

use super::consts::{
    IMAGE_DIRECTORY_ENTRY_EXPORT, IMAGE_FILE_DLL, IMAGE_NT_OPTIONAL_HDR32_MAGIC,
    IMAGE_NT_OPTIONAL_HDR64_MAGIC, IMAGE_NT_SIGNATURE, IMAGE_SCN_CNT_CODE, IMAGE_SCN_MEM_EXECUTE,
    machine_architecture,
};
use super::header::FileHeader;
use super::object::Section;
use super::section::{SECTION_HEADER_SIZE, SectionHeader, SectionTable};
use super::source::{Source, array, c_string, subslice, to_u64, u8_at, u16_at, u32_at, u64_at};
use super::strtab::StringTable;
use super::symbol::SymbolTable;
use crate::error::Result;
use crate::target::Architecture;

/// Offset of `e_lfanew` in the DOS header.
pub const E_LFANEW_OFFSET: usize = 0x3c;
/// Size of `IMAGE_EXPORT_DIRECTORY`.
pub const EXPORT_DIRECTORY_SIZE: usize = 40;

/// One data directory entry.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct DataDirectory {
    /// RVA of the table (a file offset for the certificate table).
    pub virtual_address: u32,
    /// Size of the table.
    pub size: u32,
}

impl DataDirectory {
    /// Whether `rva` lies inside the table.
    #[must_use]
    pub fn contains(&self, rva: u32) -> bool {
        rva.checked_sub(self.virtual_address)
            .is_some_and(|delta| delta < self.size)
    }
}

/// The optional header, normalized over PE32 and PE32+.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct OptionalHeader {
    /// `Magic`: `0x10b` (PE32) or `0x20b` (PE32+).
    pub magic: u16,
    /// `MajorLinkerVersion`.
    pub major_linker_version: u8,
    /// `MinorLinkerVersion`.
    pub minor_linker_version: u8,
    /// `SizeOfCode`.
    pub size_of_code: u32,
    /// `SizeOfInitializedData`.
    pub size_of_initialized_data: u32,
    /// `SizeOfUninitializedData`.
    pub size_of_uninitialized_data: u32,
    /// `AddressOfEntryPoint` (RVA).
    pub address_of_entry_point: u32,
    /// `BaseOfCode` (RVA).
    pub base_of_code: u32,
    /// `BaseOfData` (RVA); PE32 only.
    pub base_of_data: Option<u32>,
    /// `ImageBase`.
    pub image_base: u64,
    /// `SectionAlignment`.
    pub section_alignment: u32,
    /// `FileAlignment`.
    pub file_alignment: u32,
    /// `MajorOperatingSystemVersion`.
    pub major_operating_system_version: u16,
    /// `MinorOperatingSystemVersion`.
    pub minor_operating_system_version: u16,
    /// `MajorImageVersion`.
    pub major_image_version: u16,
    /// `MinorImageVersion`.
    pub minor_image_version: u16,
    /// `MajorSubsystemVersion`.
    pub major_subsystem_version: u16,
    /// `MinorSubsystemVersion`.
    pub minor_subsystem_version: u16,
    /// `Win32VersionValue` (reserved, 0).
    pub win32_version_value: u32,
    /// `SizeOfImage`.
    pub size_of_image: u32,
    /// `SizeOfHeaders`.
    pub size_of_headers: u32,
    /// `CheckSum`.
    pub check_sum: u32,
    /// `Subsystem` (`IMAGE_SUBSYSTEM_*`).
    pub subsystem: u16,
    /// `DllCharacteristics` (`IMAGE_DLLCHARACTERISTICS_*`).
    pub dll_characteristics: u16,
    /// `SizeOfStackReserve`.
    pub size_of_stack_reserve: u64,
    /// `SizeOfStackCommit`.
    pub size_of_stack_commit: u64,
    /// `SizeOfHeapReserve`.
    pub size_of_heap_reserve: u64,
    /// `SizeOfHeapCommit`.
    pub size_of_heap_commit: u64,
    /// `LoaderFlags` (reserved, 0).
    pub loader_flags: u32,
    /// `NumberOfRvaAndSizes`, as written.
    pub number_of_rva_and_sizes: u32,
}

impl OptionalHeader {
    /// Whether the header is PE32+.
    #[must_use]
    pub fn is_pe32_plus(&self) -> bool {
        self.magic == IMAGE_NT_OPTIONAL_HDR64_MAGIC
    }

    /// Decodes an optional header. Returns the header and the offset of the
    /// data directories within `data`, or `None` if the magic is unknown or
    /// the fixed part is truncated.
    fn decode(data: &[u8]) -> Option<(Self, usize)> {
        let magic = u16_at(data, 0)?;
        let plus = match magic {
            IMAGE_NT_OPTIONAL_HDR32_MAGIC => false,
            IMAGE_NT_OPTIONAL_HDR64_MAGIC => true,
            _ => return None,
        };
        let word = |offset: usize| -> Option<u64> {
            if plus {
                u64_at(data, offset)
            } else {
                u32_at(data, offset).map(u64::from)
            }
        };
        let (image_base, base_of_data) = if plus {
            (u64_at(data, 24)?, None)
        } else {
            (u64::from(u32_at(data, 28)?), Some(u32_at(data, 24)?))
        };
        // Offsets of the stack and heap sizes, `LoaderFlags` and the data
        // directories, which move with the word size.
        let (sizes, loader, count, directories): ([usize; 4], usize, usize, usize) = if plus {
            ([72, 80, 88, 96], 104, 108, 112)
        } else {
            ([72, 76, 80, 84], 88, 92, 96)
        };
        let header = Self {
            magic,
            major_linker_version: u8_at(data, 2)?,
            minor_linker_version: u8_at(data, 3)?,
            size_of_code: u32_at(data, 4)?,
            size_of_initialized_data: u32_at(data, 8)?,
            size_of_uninitialized_data: u32_at(data, 12)?,
            address_of_entry_point: u32_at(data, 16)?,
            base_of_code: u32_at(data, 20)?,
            base_of_data,
            image_base,
            section_alignment: u32_at(data, 32)?,
            file_alignment: u32_at(data, 36)?,
            major_operating_system_version: u16_at(data, 40)?,
            minor_operating_system_version: u16_at(data, 42)?,
            major_image_version: u16_at(data, 44)?,
            minor_image_version: u16_at(data, 46)?,
            major_subsystem_version: u16_at(data, 48)?,
            minor_subsystem_version: u16_at(data, 50)?,
            win32_version_value: u32_at(data, 52)?,
            size_of_image: u32_at(data, 56)?,
            size_of_headers: u32_at(data, 60)?,
            check_sum: u32_at(data, 64)?,
            subsystem: u16_at(data, 68)?,
            dll_characteristics: u16_at(data, 70)?,
            size_of_stack_reserve: word(sizes[0])?,
            size_of_stack_commit: word(sizes[1])?,
            size_of_heap_reserve: word(sizes[2])?,
            size_of_heap_commit: word(sizes[3])?,
            loader_flags: u32_at(data, loader)?,
            number_of_rva_and_sizes: u32_at(data, count)?,
        };
        Some((header, directories))
    }
}

/// A parsed PE image.
///
/// Parsing validates the DOS stub pointer, the PE signature, the COFF and
/// optional headers and the section table. RVAs are translated through the
/// section table on demand.
#[derive(Clone, Copy, Debug)]
pub struct PeImage<'a> {
    data: &'a [u8],
    source: Source<'a>,
    nt_offset: u32,
    file_header: FileHeader,
    optional: OptionalHeader,
    directories: &'a [[u8; 8]],
    sections: SectionTable<'a>,
    strings: StringTable<'a>,
}

impl<'a> PeImage<'a> {
    /// Parses a PE image.
    ///
    /// Data directories beyond the end of the optional header are ignored,
    /// as the Windows loader does. A COFF symbol table (present in images
    /// GNU ld writes with debug information) is only used for long section
    /// names; if it is invalid, long names do not resolve.
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` if the DOS header, PE signature, COFF
    /// header, optional header or section table is missing or truncated.
    pub fn parse(data: &'a [u8], source: Source<'a>) -> Result<Self> {
        if !data.starts_with(b"MZ") {
            return Err(source.malformed(0, "DOS header (no MZ signature)"));
        }
        let nt_offset = u32_at(data, E_LFANEW_OFFSET)
            .ok_or_else(|| source.malformed(0, "DOS header (truncated)"))?;
        let nt = usize::try_from(nt_offset).unwrap_or(usize::MAX);
        if array::<4>(data, nt) != Some(IMAGE_NT_SIGNATURE) {
            return Err(source.malformed(u64::from(nt_offset), "PE signature"));
        }
        let coff_offset = nt.saturating_add(4);
        let file_header = data
            .get(coff_offset..)
            .and_then(FileHeader::parse_regular)
            .ok_or_else(|| source.malformed(to_u64(coff_offset), "COFF file header (truncated)"))?;
        let optional_offset = coff_offset.saturating_add(20);
        let optional_size = usize::from(file_header.size_of_optional_header);
        let optional_bytes = subslice(data, to_u64(optional_offset), to_u64(optional_size))
            .ok_or_else(|| {
                source.malformed(to_u64(optional_offset), "optional header (out of bounds)")
            })?;
        let (optional, directories_offset) =
            OptionalHeader::decode(optional_bytes).ok_or_else(|| {
                source.malformed(
                    to_u64(optional_offset),
                    "optional header (bad magic or size)",
                )
            })?;
        let (all_directories, _) = optional_bytes
            .get(directories_offset..)
            .unwrap_or_default()
            .as_chunks::<8>();
        let wanted = usize::try_from(optional.number_of_rva_and_sizes).unwrap_or(usize::MAX);
        let directories = all_directories
            .get(..wanted.min(all_directories.len()))
            .unwrap_or_default();

        let table_offset = optional_offset.saturating_add(optional_size);
        let table_size =
            u64::from(file_header.number_of_sections).saturating_mul(to_u64(SECTION_HEADER_SIZE));
        let table = subslice(data, to_u64(table_offset), table_size).ok_or_else(|| {
            source.malformed(to_u64(table_offset), "section table (out of bounds)")
        })?;
        let sections = SectionTable::from_bytes(table, to_u64(table_offset));
        let strings = SymbolTable::parse(
            data,
            file_header.pointer_to_symbol_table,
            file_header.number_of_symbols,
            false,
            source,
        )
        .map(|symbols| symbols.strings())
        .unwrap_or_default();
        Ok(Self {
            data,
            source,
            nt_offset,
            file_header,
            optional,
            directories,
            sections,
            strings,
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

    /// `e_lfanew`: file offset of the PE signature.
    #[must_use]
    pub fn nt_headers_offset(&self) -> u32 {
        self.nt_offset
    }

    /// The COFF file header.
    #[must_use]
    pub fn file_header(&self) -> &FileHeader {
        &self.file_header
    }

    /// The optional header.
    #[must_use]
    pub fn optional_header(&self) -> &OptionalHeader {
        &self.optional
    }

    /// `Machine` (`IMAGE_FILE_MACHINE_*`).
    #[must_use]
    pub fn machine(&self) -> u16 {
        self.file_header.machine
    }

    /// The target architecture, if qld knows the machine.
    #[must_use]
    pub fn architecture(&self) -> Option<Architecture> {
        machine_architecture(self.file_header.machine)
    }

    /// Whether the optional header is PE32+.
    #[must_use]
    pub fn is_pe32_plus(&self) -> bool {
        self.optional.is_pe32_plus()
    }

    /// `Characteristics` of the COFF header (`IMAGE_FILE_*`).
    #[must_use]
    pub fn characteristics(&self) -> u16 {
        self.file_header.characteristics
    }

    /// Whether the image is a DLL (`IMAGE_FILE_DLL`).
    #[must_use]
    pub fn is_dll(&self) -> bool {
        self.file_header.characteristics & IMAGE_FILE_DLL != 0
    }

    /// `ImageBase`.
    #[must_use]
    pub fn image_base(&self) -> u64 {
        self.optional.image_base
    }

    /// `Subsystem` (`IMAGE_SUBSYSTEM_*`).
    #[must_use]
    pub fn subsystem(&self) -> u16 {
        self.optional.subsystem
    }

    /// `DllCharacteristics` (`IMAGE_DLLCHARACTERISTICS_*`).
    #[must_use]
    pub fn dll_characteristics(&self) -> u16 {
        self.optional.dll_characteristics
    }

    /// Number of data directories present (the smaller of
    /// `NumberOfRvaAndSizes` and what fits in the optional header).
    #[must_use]
    pub fn data_directory_count(&self) -> usize {
        self.directories.len()
    }

    /// Data directory `index` (`IMAGE_DIRECTORY_ENTRY_*`), if present and
    /// nonempty.
    #[must_use]
    pub fn data_directory(&self, index: usize) -> Option<DataDirectory> {
        let raw = self.directories.get(index)?;
        let directory = DataDirectory {
            virtual_address: u32_at(raw, 0)?,
            size: u32_at(raw, 4)?,
        };
        (directory.virtual_address != 0 || directory.size != 0).then_some(directory)
    }

    /// Number of sections.
    #[must_use]
    pub fn section_count(&self) -> u32 {
        self.file_header.number_of_sections
    }

    /// The section header table.
    #[must_use]
    pub fn section_table(&self) -> SectionTable<'a> {
        self.sections
    }

    /// Decodes section `number` (1-based) and resolves its name.
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` if `number` is out of range or the name
    /// cannot be resolved.
    pub fn section(&self, number: u32) -> Result<Section<'a>> {
        let header = self.sections.get(number).ok_or_else(|| {
            self.source.malformed(
                self.sections.file_offset(),
                format!("section number {number} (out of range)"),
            )
        })?;
        let name = self.sections.name(number, &self.strings).ok_or_else(|| {
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
        (1..=self.file_header.number_of_sections).map(|number| self.section(number))
    }

    /// The file contents of a section: `SizeOfRawData` bytes, cut to
    /// `VirtualSize` when that is smaller (the rest is file alignment
    /// padding).
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` if the contents are out of bounds.
    pub fn section_data(&self, header: &SectionHeader) -> Result<&'a [u8]> {
        if header.pointer_to_raw_data == 0 {
            return Ok(&[]);
        }
        let size = if header.virtual_size != 0 {
            header.size_of_raw_data.min(header.virtual_size)
        } else {
            header.size_of_raw_data
        };
        let offset = u64::from(header.pointer_to_raw_data);
        subslice(self.data, offset, u64::from(size)).ok_or_else(|| {
            self.source
                .malformed(offset, "section contents (out of bounds)")
        })
    }

    /// The header of the section whose address range contains `rva`
    /// (`VirtualSize`, or `SizeOfRawData` when that is 0), with its number.
    #[must_use]
    pub fn section_at_rva(&self, rva: u32) -> Option<(u32, SectionHeader)> {
        self.sections
            .iter()
            .zip(1u32..)
            .find_map(|(header, number)| {
                let size = if header.virtual_size != 0 {
                    header.virtual_size
                } else {
                    header.size_of_raw_data
                };
                let delta = rva.checked_sub(header.virtual_address)?;
                (delta < size).then_some((number, header))
            })
    }

    /// Translates an RVA to a file offset, and returns how many bytes of
    /// file data follow it within its section (or the headers).
    ///
    /// Returns `None` if no section contains `rva`, or if it falls in the
    /// part of a section that has no file data (zero-initialized).
    #[must_use]
    pub fn rva_to_file_offset(&self, rva: u32) -> Option<(u64, u64)> {
        if let Some((_, header)) = self.section_at_rva(rva) {
            let delta = rva.checked_sub(header.virtual_address)?;
            let available = header.size_of_raw_data.checked_sub(delta)?;
            if available == 0 || header.pointer_to_raw_data == 0 {
                return None;
            }
            let offset = u64::from(header.pointer_to_raw_data).checked_add(u64::from(delta))?;
            return Some((offset, u64::from(available)));
        }
        // Headers are mapped at RVA 0 unchanged.
        let headers = self.optional.size_of_headers;
        let available = headers.checked_sub(rva).filter(|&n| n != 0)?;
        Some((u64::from(rva), u64::from(available)))
    }

    /// `size` bytes of file data at `rva`, all within one section.
    #[must_use]
    pub fn data_at_rva(&self, rva: u32, size: u32) -> Option<&'a [u8]> {
        let (offset, available) = self.rva_to_file_offset(rva)?;
        if u64::from(size) > available {
            return None;
        }
        subslice(self.data, offset, u64::from(size))
    }

    /// The NUL-terminated string at `rva` (without the terminator), which
    /// must end within the section's file data.
    #[must_use]
    pub fn c_string_at_rva(&self, rva: u32) -> Option<&'a [u8]> {
        let (offset, available) = self.rva_to_file_offset(rva)?;
        c_string(subslice(self.data, offset, available)?)
    }

    /// Whether `rva` lies in a section that is not code (neither
    /// `IMAGE_SCN_CNT_CODE` nor `IMAGE_SCN_MEM_EXECUTE`). Linkers use this
    /// to tell data exports, which get no thunk, from functions.
    #[must_use]
    pub fn is_data_rva(&self, rva: u32) -> bool {
        self.section_at_rva(rva).is_some_and(|(_, header)| {
            header.characteristics & (IMAGE_SCN_CNT_CODE | IMAGE_SCN_MEM_EXECUTE) == 0
        })
    }

    /// The export directory, if the image has one.
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` if the directory, its tables or the DLL
    /// name are not backed by file data.
    pub fn exports(&self) -> Result<Option<ExportDirectory<'a>>> {
        let Some(directory) = self.data_directory(IMAGE_DIRECTORY_ENTRY_EXPORT) else {
            return Ok(None);
        };
        ExportDirectory::parse(*self, directory).map(Some)
    }

    #[cold]
    fn rva_error(&self, rva: u32, what: &str) -> crate::Error {
        let offset = self.rva_to_file_offset(rva).map_or(0, |(offset, _)| offset);
        self.source
            .malformed(offset, format!("{what} (RVA {rva:#x} not in the file)"))
    }
}

/// Where an export points.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ExportTarget<'a> {
    /// The RVA of the exported function or data.
    Rva(u32),
    /// A forwarder: `DLL.function` or `DLL.#ordinal`.
    Forwarder(&'a [u8]),
}

/// One export.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Export<'a> {
    /// The ordinal, `Base` included.
    pub ordinal: u32,
    /// The exported name, if it has one.
    pub name: Option<&'a [u8]>,
    /// Where it points.
    pub target: ExportTarget<'a>,
}

/// The export directory (`IMAGE_EXPORT_DIRECTORY`) and its tables.
#[derive(Clone, Copy, Debug)]
pub struct ExportDirectory<'a> {
    image: PeImage<'a>,
    directory: DataDirectory,
    /// `Characteristics` (reserved, 0).
    pub export_flags: u32,
    /// `TimeDateStamp`.
    pub time_date_stamp: u32,
    /// `MajorVersion`.
    pub major_version: u16,
    /// `MinorVersion`.
    pub minor_version: u16,
    /// `Name`: RVA of the DLL name.
    pub name_rva: u32,
    /// `Base`: the ordinal of the first address table entry.
    pub ordinal_base: u32,
    /// The DLL name.
    pub dll_name: &'a [u8],
    addresses: &'a [[u8; 4]],
    names: &'a [[u8; 4]],
    ordinals: &'a [[u8; 2]],
}

impl<'a> ExportDirectory<'a> {
    fn parse(image: PeImage<'a>, directory: DataDirectory) -> Result<Self> {
        let rva = directory.virtual_address;
        let size = u32::try_from(EXPORT_DIRECTORY_SIZE).unwrap_or(u32::MAX);
        let raw = image
            .data_at_rva(rva, size)
            .ok_or_else(|| image.rva_error(rva, "export directory"))?;
        let field = |offset| u32_at(raw, offset).unwrap_or(0);
        let name_rva = field(12);
        let address_count = field(20);
        let name_count = field(24);
        let table = |table_rva: u32, count: u32, entry: u32, what: &str| -> Result<&'a [u8]> {
            if count == 0 {
                return Ok(&[]);
            }
            let bytes = count
                .checked_mul(entry)
                .ok_or_else(|| image.rva_error(table_rva, what))?;
            image
                .data_at_rva(table_rva, bytes)
                .ok_or_else(|| image.rva_error(table_rva, what))
        };
        let addresses = table(field(28), address_count, 4, "export address table")?;
        let names = table(field(32), name_count, 4, "export name pointer table")?;
        let ordinals = table(field(36), name_count, 2, "export ordinal table")?;
        let dll_name = image
            .c_string_at_rva(name_rva)
            .ok_or_else(|| image.rva_error(name_rva, "export DLL name"))?;
        Ok(Self {
            image,
            directory,
            export_flags: field(0),
            time_date_stamp: field(4),
            major_version: u16_at(raw, 8).unwrap_or(0),
            minor_version: u16_at(raw, 10).unwrap_or(0),
            name_rva,
            ordinal_base: field(16),
            dll_name,
            addresses: addresses.as_chunks::<4>().0,
            names: names.as_chunks::<4>().0,
            ordinals: ordinals.as_chunks::<2>().0,
        })
    }

    /// The data directory entry (its range identifies forwarders).
    #[must_use]
    pub fn directory(&self) -> DataDirectory {
        self.directory
    }

    /// Number of export address table entries.
    #[must_use]
    pub fn address_count(&self) -> usize {
        self.addresses.len()
    }

    /// Number of named exports.
    #[must_use]
    pub fn name_count(&self) -> usize {
        self.names.len()
    }

    /// The target of export address table entry `index` (unbiased), or
    /// `None` for an out-of-range index or an unused (zero) entry.
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` if a forwarder string is not in the file.
    pub fn target(&self, index: usize) -> Result<Option<ExportTarget<'a>>> {
        let Some(rva) = self
            .addresses
            .get(index)
            .map(|raw| u32::from_le_bytes(*raw))
        else {
            return Ok(None);
        };
        if rva == 0 {
            return Ok(None);
        }
        if self.directory.contains(rva) {
            let forwarder = self
                .image
                .c_string_at_rva(rva)
                .ok_or_else(|| self.image.rva_error(rva, "export forwarder"))?;
            return Ok(Some(ExportTarget::Forwarder(forwarder)));
        }
        Ok(Some(ExportTarget::Rva(rva)))
    }

    /// Named export `index`: its name, and the unbiased address table index
    /// from the ordinal table.
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` if the name is not in the file.
    pub fn name(&self, index: usize) -> Result<Option<(&'a [u8], u16)>> {
        let (Some(name_rva), Some(ordinal)) = (self.names.get(index), self.ordinals.get(index))
        else {
            return Ok(None);
        };
        let name_rva = u32::from_le_bytes(*name_rva);
        let name = self
            .image
            .c_string_at_rva(name_rva)
            .ok_or_else(|| self.image.rva_error(name_rva, "export name"))?;
        Ok(Some((name, u16::from_le_bytes(*ordinal))))
    }

    /// Iterates over the named exports in name table order (sorted by name,
    /// as the loader's binary search requires). This is how GNU ld reads a
    /// DLL it links against.
    pub fn named(&self) -> impl Iterator<Item = Result<Export<'a>>> + '_ {
        (0..self.names.len()).filter_map(move |index| {
            let (name, table_index) = match self.name(index) {
                Ok(Some(entry)) => entry,
                Ok(None) => return None,
                Err(error) => return Some(Err(error)),
            };
            let target = match self.target(usize::from(table_index)) {
                Ok(Some(target)) => target,
                Ok(None) => {
                    return Some(Err(self.image.source.malformed(
                        0,
                        format!(
                            "export `{}` (ordinal index {table_index} has no address)",
                            String::from_utf8_lossy(name)
                        ),
                    )));
                }
                Err(error) => return Some(Err(error)),
            };
            Some(Ok(Export {
                ordinal: self.ordinal_base.saturating_add(u32::from(table_index)),
                name: Some(name),
                target,
            }))
        })
    }

    /// Every export in address table order, skipping unused entries, each
    /// with its first name (exports reachable only by ordinal have none).
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` if a name or forwarder is not in the file.
    pub fn all(&self) -> Result<Vec<Export<'a>>> {
        let mut names: Vec<Option<&'a [u8]>> = vec![None; self.addresses.len()];
        for index in 0..self.names.len() {
            if let Some((name, table_index)) = self.name(index)?
                && let Some(slot) = names.get_mut(usize::from(table_index))
                && slot.is_none()
            {
                *slot = Some(name);
            }
        }
        let mut exports = Vec::with_capacity(self.addresses.len());
        for (index, name) in names.into_iter().enumerate() {
            if let Some(target) = self.target(index)? {
                exports.push(Export {
                    ordinal: self
                        .ordinal_base
                        .saturating_add(u32::try_from(index).unwrap_or(u32::MAX)),
                    name,
                    target,
                });
            }
        }
        Ok(exports)
    }
}
