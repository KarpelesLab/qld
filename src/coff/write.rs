//! Writing the PE image: the DOS stub, the COFF and optional headers, the
//! section table, the section contents with relocations applied, the COFF
//! symbol table and the image checksum.

#![deny(clippy::arithmetic_side_effects)]

use crate::diag::{Diagnostic, DiagnosticSink};
use crate::error::{Error, Result};
use crate::output::{FileMode, OutputFile, OutputOptions};

use super::inputs::CoffInput;
use super::layout::{Layout, Piece, align_up64};
use super::machine::Machine;
use super::options::PeOptions;
use super::read::consts::{
    IMAGE_DIRECTORY_ENTRY_BASERELOC, IMAGE_DIRECTORY_ENTRY_EXCEPTION, IMAGE_DIRECTORY_ENTRY_EXPORT,
    IMAGE_DIRECTORY_ENTRY_IAT, IMAGE_DIRECTORY_ENTRY_IMPORT, IMAGE_DIRECTORY_ENTRY_LOAD_CONFIG,
    IMAGE_DIRECTORY_ENTRY_RESOURCE, IMAGE_DIRECTORY_ENTRY_TLS,
    IMAGE_DLLCHARACTERISTICS_DYNAMIC_BASE, IMAGE_DLLCHARACTERISTICS_FORCE_INTEGRITY,
    IMAGE_DLLCHARACTERISTICS_HIGH_ENTROPY_VA, IMAGE_DLLCHARACTERISTICS_NO_BIND,
    IMAGE_DLLCHARACTERISTICS_NO_ISOLATION, IMAGE_DLLCHARACTERISTICS_NO_SEH,
    IMAGE_DLLCHARACTERISTICS_NX_COMPAT, IMAGE_DLLCHARACTERISTICS_TERMINAL_SERVER_AWARE,
    IMAGE_DLLCHARACTERISTICS_WDM_DRIVER, IMAGE_FILE_32BIT_MACHINE, IMAGE_FILE_DEBUG_STRIPPED,
    IMAGE_FILE_DLL, IMAGE_FILE_EXECUTABLE_IMAGE, IMAGE_FILE_LARGE_ADDRESS_AWARE,
    IMAGE_FILE_LINE_NUMS_STRIPPED, IMAGE_FILE_LOCAL_SYMS_STRIPPED, IMAGE_NT_OPTIONAL_HDR32_MAGIC,
    IMAGE_NT_OPTIONAL_HDR64_MAGIC, IMAGE_NT_SIGNATURE,
};
use super::reloc::{self, Addresses, Applied};

/// The MS-DOS stub GNU `ld` and `link.exe` write: a 64-byte header followed
/// by the program that prints "This program cannot be run in DOS mode".
pub const DOS_STUB: [u8; 128] = [
    0x4d, 0x5a, 0x90, 0x00, 0x03, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0xff, 0xff, 0x00, 0x00,
    0xb8, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x80, 0x00, 0x00, 0x00,
    0x0e, 0x1f, 0xba, 0x0e, 0x00, 0xb4, 0x09, 0xcd, 0x21, 0xb8, 0x01, 0x4c, 0xcd, 0x21, 0x54, 0x68,
    0x69, 0x73, 0x20, 0x70, 0x72, 0x6f, 0x67, 0x72, 0x61, 0x6d, 0x20, 0x63, 0x61, 0x6e, 0x6e, 0x6f,
    0x74, 0x20, 0x62, 0x65, 0x20, 0x72, 0x75, 0x6e, 0x20, 0x69, 0x6e, 0x20, 0x44, 0x4f, 0x53, 0x20,
    0x6d, 0x6f, 0x64, 0x65, 0x2e, 0x0d, 0x0d, 0x0a, 0x24, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

/// Size of the PE32+ optional header with 16 data directories.
pub const OPTIONAL_HEADER_SIZE_64: usize = 240;
/// Size of the PE32 optional header with 16 data directories.
pub const OPTIONAL_HEADER_SIZE_32: usize = 224;
/// Number of data directories in the optional header.
pub const DATA_DIRECTORIES: usize = 16;
/// `IMAGE_SCN_CNT_CODE`.
const IMAGE_SCN_CNT_CODE_FLAG: u32 = 0x20;
/// Size of one section header.
const SECTION_HEADER_SIZE: usize = 40;
/// Size of the `PE\0\0` signature plus `IMAGE_FILE_HEADER`.
const NT_HEADER_SIZE: usize = 24;

/// The unrounded size of everything before the first section.
#[must_use]
pub fn header_size(sections: usize, machine: Machine) -> usize {
    DOS_STUB
        .len()
        .saturating_add(NT_HEADER_SIZE)
        .saturating_add(machine.optional_header_size())
        .saturating_add(sections.saturating_mul(SECTION_HEADER_SIZE))
}

/// One data directory entry.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Directory {
    /// RVA of the table.
    pub rva: u32,
    /// Size of the table.
    pub size: u32,
}

/// Everything the writer needs beyond the layout.
#[derive(Debug)]
pub struct WriteInput<'i, 'a> {
    /// Symbol addresses.
    pub addresses: &'i Addresses<'i, 'a>,
    /// The PE options.
    pub options: &'i PeOptions,
    /// The output path.
    pub path: &'i std::path::Path,
    /// RVA of the entry point.
    pub entry: u32,
    /// The subsystem.
    pub subsystem: u16,
    /// The data directories.
    pub directories: [Directory; DATA_DIRECTORIES],
    /// Bytes generated for linker-made output sections, by section name.
    pub generated: &'i [(Vec<u8>, Vec<u8>)],
    /// Whether `.reloc` is filled in from the relocation pass.
    pub emit_base_relocs: bool,
    /// The image's COFF symbol table, appended after the sections.
    pub symbols: &'i super::symtab::SymbolTable,
    /// How the output file is created: an in-memory buffer and a
    /// cancellation token come from the link's options.
    pub output: OutputOptions,
}

/// The result of writing: the base relocations the pass found, so the caller
/// can check that `.reloc` was sized correctly.
#[derive(Debug, Default)]
pub struct Written {
    /// Diagnostics the relocation pass produced.
    pub errors: Vec<Diagnostic>,
    /// Encoded `.reloc` contents.
    pub base_relocs: Vec<u8>,
}

/// Renders every output section's bytes, with relocations applied.
///
/// Returns the section contents in layout order, and the collected base
/// relocations.
#[must_use]
pub fn render(
    addresses: &Addresses<'_, '_>,
    generated: &[(Vec<u8>, Vec<u8>)],
) -> (Vec<Vec<u8>>, Applied) {
    let layout = addresses.layout;
    let mut applied = Applied::default();
    let mut contents = Vec::with_capacity(layout.sections.len());
    for section in &layout.sections {
        if section.is_bss() {
            contents.push(Vec::new());
            continue;
        }
        // GNU ld pads x86 code with `nop` between input sections.
        let fill = if section.characteristics & IMAGE_SCN_CNT_CODE_FLAG != 0
            && layout.machine != Machine::Arm64
        {
            0x90
        } else {
            0
        };
        let mut bytes = vec![fill; section.virtual_size as usize];
        if let Some((_, made)) = generated
            .iter()
            .find(|(name, _)| name.as_slice() == section.name.as_slice())
        {
            let end = made.len().min(bytes.len());
            if let Some(slot) = bytes.get_mut(..end) {
                slot.copy_from_slice(made.get(..end).unwrap_or_default());
            }
        }
        for chunk in &section.chunks {
            let start = chunk.offset as usize;
            let Some(end) = start.checked_add(chunk.size as usize) else {
                continue;
            };
            match &chunk.piece {
                Piece::Zero => {}
                &Piece::Thunks {
                    file,
                    section: number,
                } => {
                    let targets: Vec<u32> = layout
                        .thunks
                        .blocks
                        .get(&(file, number))
                        .map_or(&[][..], Vec::as_slice)
                        .iter()
                        .map(|target| {
                            let base = addresses.record_value(file as usize, target.record).map_or(
                                0,
                                |value| match value {
                                    reloc::Value::Address { rva, .. } => u64::from(rva),
                                    reloc::Value::Absolute(number) => number,
                                },
                            );
                            base.wrapping_add(target.addend as u64) as u32
                        })
                        .collect();
                    let rva = section.rva.wrapping_add(chunk.offset);
                    if let Some(slot) = bytes.get_mut(start..end)
                        && let Err(problem) = super::arm64::render_block(slot, rva, &targets)
                    {
                        applied.errors.push(Diagnostic::error(problem));
                    }
                }
                Piece::Fill(fill) => {
                    let len = fill.len().min(chunk.size as usize);
                    if let Some(slot) = bytes.get_mut(start..start.saturating_add(len)) {
                        slot.copy_from_slice(fill.get(..len).unwrap_or_default());
                    }
                }
                &Piece::Input {
                    file,
                    section: number,
                } => {
                    let file = file as usize;
                    let Some(input) = addresses
                        .files
                        .get(file)
                        .and_then(CoffInput::object)
                        .and_then(|parsed| parsed.section(number))
                    else {
                        continue;
                    };
                    let Some(slot) = bytes.get_mut(start..end) else {
                        continue;
                    };
                    let len = input.data.len().min(slot.len());
                    if let Some(target) = slot.get_mut(..len) {
                        target.copy_from_slice(input.data.get(..len).unwrap_or_default());
                    }
                    reloc::apply(
                        addresses,
                        file,
                        number,
                        section.rva.wrapping_add(chunk.offset),
                        slot,
                        &mut applied,
                    );
                }
            }
        }
        if section.name == b".pdata" {
            sort_pdata(&mut bytes, layout.machine);
        }
        contents.push(bytes);
    }
    (contents, applied)
}

/// Sorts the `.pdata` table by `BeginAddress`.
///
/// The Windows unwinder binary-searches the exception table, so its entries
/// must be in ascending address order. Concatenating each object's `.pdata`
/// happens to produce that order when the linker keeps the objects' `.text`
/// order, but qld does not promise that order, so it sorts. An x86-64
/// `RUNTIME_FUNCTION` is 12 bytes; an ARM64 one is 8, its second word
/// holding either the `.xdata` RVA or packed unwind data.
fn sort_pdata(bytes: &mut [u8], machine: Machine) {
    fn by_begin<const N: usize>(bytes: &mut [u8]) {
        let (records, _) = bytes.as_chunks_mut::<N>();
        records.sort_by_key(|record| {
            record
                .first_chunk::<4>()
                .map_or(0, |begin| u32::from_le_bytes(*begin))
        });
    }
    match machine.pdata_entry_size() {
        12 => by_begin::<12>(bytes),
        8 => by_begin::<8>(bytes),
        _ => {}
    }
}

/// Writes the image described by `input`, with `contents` as the section
/// bytes produced by [`render`].
///
/// # Errors
///
/// Returns [`Error::Io`] if the output cannot be created or written, and
/// [`Error::Limit`] if the image is too large for PE.
pub fn write(input: &WriteInput<'_, '_>, contents: &[Vec<u8>]) -> Result<()> {
    let layout = input.addresses.layout;
    let symbol_table = u64::try_from(input.symbols.bytes.len()).unwrap_or(0);
    let size = align_up64(layout.file_size, u64::from(input.options.file_alignment))
        .saturating_add(symbol_table);
    let mut output = OutputFile::create(
        input.path,
        size,
        &OutputOptions {
            mode: FileMode::Executable,
            ..input.output.clone()
        },
    )?;
    {
        let bytes = output.as_mut_slice()?;
        write_headers(input, bytes)?;
        for (section, data) in layout.sections.iter().zip(contents) {
            if section.is_bss() || data.is_empty() {
                continue;
            }
            let start = section.file_offset as usize;
            let Some(end) = start.checked_add(data.len()) else {
                return Err(Error::Limit("section past the end of the output".into()));
            };
            let Some(slot) = bytes.get_mut(start..end) else {
                return Err(Error::Limit("section past the end of the output".into()));
            };
            slot.copy_from_slice(data);
        }
        if !input.symbols.is_empty() {
            let start = usize::try_from(layout.file_size).unwrap_or(0);
            let end = start.saturating_add(input.symbols.bytes.len());
            match bytes.get_mut(start..end) {
                Some(slot) => slot.copy_from_slice(&input.symbols.bytes),
                None => return Err(Error::Limit("symbol table past the output".into())),
            }
        }
        let checksum = compute_checksum(bytes, checksum_offset());
        if let Some(slot) = bytes
            .get_mut(checksum_offset()..)
            .and_then(<[u8]>::first_chunk_mut::<4>)
        {
            *slot = checksum.to_le_bytes();
        }
    }
    output.finish()?;
    Ok(())
}

/// File offset of the optional header's `CheckSum` field.
fn checksum_offset() -> usize {
    DOS_STUB
        .len()
        .saturating_add(NT_HEADER_SIZE)
        .saturating_add(64)
}

/// Writes the DOS stub, the COFF header, the optional header and the section
/// table into `bytes`.
fn write_headers(input: &WriteInput<'_, '_>, bytes: &mut [u8]) -> Result<()> {
    let layout = input.addresses.layout;
    let options = input.options;
    let machine = options.target();
    let mut w = Cursor::new(bytes);
    w.put(&DOS_STUB)?;
    w.put(&IMAGE_NT_SIGNATURE)?;

    // IMAGE_FILE_HEADER.
    let count = u16::try_from(layout.sections.len())
        .map_err(|_| Error::Limit("too many output sections".into()))?;
    let mut characteristics =
        IMAGE_FILE_EXECUTABLE_IMAGE | IMAGE_FILE_LINE_NUMS_STRIPPED | IMAGE_FILE_DEBUG_STRIPPED;
    if input.symbols.is_empty() {
        characteristics |= IMAGE_FILE_LOCAL_SYMS_STRIPPED;
    }
    if options.large_address_aware {
        characteristics |= IMAGE_FILE_LARGE_ADDRESS_AWARE;
    }
    if options.dll {
        characteristics |= IMAGE_FILE_DLL;
    }
    if machine.is_pe32() {
        characteristics |= IMAGE_FILE_32BIT_MACHINE;
    }
    w.u16(options.machine)?;
    w.u16(count)?;
    w.u32(0)?; // TimeDateStamp; deterministic unless --insert-timestamp.
    if input.symbols.is_empty() {
        w.u32(0)?; // PointerToSymbolTable
        w.u32(0)?; // NumberOfSymbols
    } else {
        w.u32(
            u32::try_from(layout.file_size)
                .map_err(|_| Error::Limit("output file too large for a symbol table".into()))?,
        )?;
        w.u32(input.symbols.count)?;
    }
    w.u16(u16::try_from(machine.optional_header_size()).unwrap_or(0))?;
    w.u16(characteristics)?;

    // IMAGE_OPTIONAL_HEADER64, or IMAGE_OPTIONAL_HEADER32 which adds
    // `BaseOfData` and has 32-bit image base, stack and heap fields.
    let sum = |pick: fn(&super::layout::OutSection) -> bool| -> u32 {
        layout
            .sections
            .iter()
            .filter(|section| pick(section))
            .fold(0u32, |total, section| {
                total.saturating_add(section.raw_size.max(section.virtual_size))
            })
    };
    let code = sum(|section| section.characteristics & 0x20 != 0);
    // As GNU ld counts: every section flagged as initialized data, code
    // included, and `.bss` rounded up to the file alignment.
    let initialized = sum(|section| section.characteristics & 0x40 != 0);
    let uninitialized = layout
        .sections
        .iter()
        .filter(|section| section.is_bss())
        .fold(0u32, |total, section| {
            let rounded = u32::try_from(align_up64(
                u64::from(section.virtual_size),
                u64::from(options.file_alignment),
            ))
            .unwrap_or(u32::MAX);
            total.saturating_add(rounded)
        });
    let base_of_code = layout.by_name(b".text").map_or(0, |section| section.rva);
    // GNU ld's `BaseOfData` is the first section after the code.
    let base_of_data = layout
        .sections
        .iter()
        .find(|section| section.characteristics & 0x20 == 0)
        .map_or(0, |section| section.rva);
    let pe32 = machine.is_pe32();
    // A PE32 field that PE32+ widens to 64 bits.
    let wide = |w: &mut Cursor<'_>, value: u64| -> Result<()> {
        if pe32 {
            w.u32(u32::try_from(value).map_err(|_| {
                Error::Limit(format!("{value:#x} does not fit a PE32 header field"))
            })?)
        } else {
            w.u64(value)
        }
    };
    w.u16(if pe32 {
        IMAGE_NT_OPTIONAL_HDR32_MAGIC
    } else {
        IMAGE_NT_OPTIONAL_HDR64_MAGIC
    })?;
    w.u8(2)?; // MajorLinkerVersion, as GNU ld reports.
    w.u8(44)?;
    w.u32(code)?;
    w.u32(initialized)?;
    w.u32(uninitialized)?;
    w.u32(input.entry)?;
    w.u32(base_of_code)?;
    if pe32 {
        w.u32(base_of_data)?;
    }
    wide(&mut w, options.effective_image_base())?;
    w.u32(options.section_alignment)?;
    w.u32(options.file_alignment)?;
    w.u16(options.os_version.major)?;
    w.u16(options.os_version.minor)?;
    w.u16(options.image_version.major)?;
    w.u16(options.image_version.minor)?;
    w.u16(options.subsystem_version.major)?;
    w.u16(options.subsystem_version.minor)?;
    w.u32(0)?; // Win32VersionValue
    w.u32(layout.size_of_image)?;
    w.u32(layout.size_of_headers)?;
    w.u32(0)?; // CheckSum, filled in after the image is complete.
    w.u16(input.subsystem)?;
    w.u16(dll_characteristics(options))?;
    wide(&mut w, options.stack.0)?;
    wide(&mut w, options.stack.1)?;
    wide(&mut w, options.heap.0)?;
    wide(&mut w, options.heap.1)?;
    w.u32(0)?; // LoaderFlags
    w.u32(u32::try_from(DATA_DIRECTORIES).unwrap_or(0))?;
    for directory in &input.directories {
        w.u32(directory.rva)?;
        w.u32(directory.size)?;
    }

    // The section table. Long names need the string table the symbol table
    // carries, so `-s` truncates them as GNU ld does.
    for (index, section) in layout.sections.iter().enumerate() {
        let name = match input.symbols.section_names.get(index) {
            Some(name) => *name,
            None => {
                let mut name = [0u8; 8];
                let len = section.name.len().min(8);
                if let (Some(slot), Some(source)) = (name.get_mut(..len), section.name.get(..len)) {
                    slot.copy_from_slice(source);
                }
                name
            }
        };
        w.put(&name)?;
        w.u32(section.virtual_size)?;
        w.u32(section.rva)?;
        w.u32(section.raw_size)?;
        w.u32(section.file_offset)?;
        w.u32(0)?; // PointerToRelocations
        w.u32(0)?; // PointerToLinenumbers
        w.u16(0)?; // NumberOfRelocations
        w.u16(0)?; // NumberOfLinenumbers
        w.u32(section.characteristics)?;
    }
    Ok(())
}

/// The `DllCharacteristics` bits the options ask for.
fn dll_characteristics(options: &PeOptions) -> u16 {
    let mut bits = 0u16;
    // 64-bit ASLR means nothing to a 32-bit image.
    if options.high_entropy_va && !options.target().is_pe32() {
        bits |= IMAGE_DLLCHARACTERISTICS_HIGH_ENTROPY_VA;
    }
    if options.dynamicbase {
        bits |= IMAGE_DLLCHARACTERISTICS_DYNAMIC_BASE;
    }
    if options.forceinteg {
        bits |= IMAGE_DLLCHARACTERISTICS_FORCE_INTEGRITY;
    }
    if options.nxcompat {
        bits |= IMAGE_DLLCHARACTERISTICS_NX_COMPAT;
    }
    if options.no_isolation {
        bits |= IMAGE_DLLCHARACTERISTICS_NO_ISOLATION;
    }
    if options.no_seh {
        bits |= IMAGE_DLLCHARACTERISTICS_NO_SEH;
    }
    if options.no_bind {
        bits |= IMAGE_DLLCHARACTERISTICS_NO_BIND;
    }
    if options.wdmdriver {
        bits |= IMAGE_DLLCHARACTERISTICS_WDM_DRIVER;
    }
    if options.tsaware {
        bits |= IMAGE_DLLCHARACTERISTICS_TERMINAL_SERVER_AWARE;
    }
    bits
}

/// The PE image checksum: a ones-complement sum of 16-bit words with the
/// `CheckSum` field read as zero, plus the file size.
#[must_use]
pub fn compute_checksum(bytes: &[u8], skip: usize) -> u32 {
    let mut sum: u32 = 0;
    let mut index = 0usize;
    while index < bytes.len() {
        let word = if index.saturating_add(1) < bytes.len() {
            u32::from(u16::from_le_bytes([
                bytes[index],
                bytes[index.saturating_add(1)],
            ]))
        } else {
            u32::from(bytes[index])
        };
        // The checksum field itself counts as zero.
        let word = if index >= skip && index < skip.saturating_add(4) {
            0
        } else {
            word
        };
        sum = sum.wrapping_add(word);
        sum = (sum & 0xffff).wrapping_add(sum >> 16);
        index = index.saturating_add(2);
    }
    sum = (sum & 0xffff).wrapping_add(sum >> 16);
    sum = (sum & 0xffff).wrapping_add(sum >> 16);
    (sum & 0xffff).wrapping_add(u32::try_from(bytes.len()).unwrap_or(0))
}

/// Fills the data directory entries that come straight from an output
/// section.
#[must_use]
pub fn section_directories(layout: &Layout) -> [Directory; DATA_DIRECTORIES] {
    let mut directories = [Directory::default(); DATA_DIRECTORIES];
    let mut set = |index: usize, name: &[u8]| {
        if let Some(section) = layout.by_name(name)
            && let Some(slot) = directories.get_mut(index)
        {
            *slot = Directory {
                rva: section.rva,
                size: section.virtual_size,
            };
        }
    };
    set(IMAGE_DIRECTORY_ENTRY_EXPORT, b".edata");
    set(IMAGE_DIRECTORY_ENTRY_IMPORT, b".idata");
    set(IMAGE_DIRECTORY_ENTRY_RESOURCE, b".rsrc");
    set(IMAGE_DIRECTORY_ENTRY_EXCEPTION, b".pdata");
    set(IMAGE_DIRECTORY_ENTRY_BASERELOC, b".reloc");
    directories
}

/// Fills the directories that point at a symbol: the TLS directory
/// (`_tls_used`), the load configuration (`_load_config_used`) and the import
/// address table (`__IAT_start__` … `__IAT_end__`).
pub fn symbol_directories(
    addresses: &Addresses<'_, '_>,
    machine: Machine,
    directories: &mut [Directory; DATA_DIRECTORIES],
) {
    let mut set = |index: usize, name: &[u8], size: u32| {
        if let Some(rva) = addresses.by_name(name).and_then(super::reloc::Value::rva)
            && let Some(slot) = directories.get_mut(index)
        {
            *slot = Directory { rva, size };
        }
    };
    // The C runtime defines both structures under C names, which i386
    // decorates. IMAGE_TLS_DIRECTORY is six fields of pointer size.
    set(
        IMAGE_DIRECTORY_ENTRY_TLS,
        &machine.decorate(b"_tls_used"),
        machine.tls_directory_size(),
    );
    // The load configuration's size is its first field; `load_config_size`
    // fills it in once the contents are known.
    set(
        IMAGE_DIRECTORY_ENTRY_LOAD_CONFIG,
        &machine.decorate(b"_load_config_used"),
        0,
    );
    // The import address table is `.idata$5`, whose bounds the layout
    // markers record: nothing has to reference `__IAT_start__` for the data
    // directory to be right.
    let start = addresses.layout.marker(super::layout::Marker::IatStart);
    let end = addresses.layout.marker(super::layout::Marker::IatEnd);
    if let (Some(start), Some(end)) = (start, end)
        && end > start
        && let Some(slot) = directories.get_mut(IMAGE_DIRECTORY_ENTRY_IAT)
    {
        *slot = Directory {
            rva: start,
            size: end.wrapping_sub(start),
        };
    }
}

/// Sets the load configuration directory's size from the structure's own
/// `Size` field, now that the section contents are known.
///
/// GNU `ld` does the same, except that it writes 64 for an i386 image whose
/// subsystem version is 5.01 or less: Windows XP and older accept only the
/// size of their own structure. `i386pe`'s default subsystem version, 4.0,
/// is such an image.
pub fn load_config_size(
    layout: &Layout,
    options: &PeOptions,
    subsystem: u16,
    contents: &[Vec<u8>],
    directories: &mut [Directory; DATA_DIRECTORIES],
) {
    let Some(directory) = directories.get_mut(IMAGE_DIRECTORY_ENTRY_LOAD_CONFIG) else {
        return;
    };
    if directory.rva == 0 {
        return;
    }
    let Some((index, section)) = layout.sections.iter().enumerate().find(|(_, section)| {
        directory.rva >= section.rva
            && directory.rva.wrapping_sub(section.rva) < section.virtual_size
    }) else {
        return;
    };
    let offset = directory.rva.wrapping_sub(section.rva) as usize;
    let Some(size) = contents
        .get(index)
        .and_then(|bytes| bytes.get(offset..))
        .and_then(<[u8]>::first_chunk::<4>)
        .map(|bytes| u32::from_le_bytes(*bytes))
    else {
        return;
    };
    let version = options.subsystem_version;
    let legacy = options.target().is_pe32()
        && (version.major, version.minor) <= (5, 1)
        && matches!(
            subsystem,
            super::read::consts::IMAGE_SUBSYSTEM_WINDOWS_CUI
                | super::read::consts::IMAGE_SUBSYSTEM_WINDOWS_GUI
        );
    directory.size = if legacy { 64 } else { size };
}

/// A bounds-checked little-endian writer over the output buffer.
struct Cursor<'b> {
    bytes: &'b mut [u8],
    at: usize,
}

impl<'b> Cursor<'b> {
    fn new(bytes: &'b mut [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    fn put(&mut self, data: &[u8]) -> Result<()> {
        let end = self
            .at
            .checked_add(data.len())
            .ok_or_else(|| Error::Limit("PE headers too large".into()))?;
        let slot = self
            .bytes
            .get_mut(self.at..end)
            .ok_or_else(|| Error::Internal("PE headers do not fit in the output".into()))?;
        slot.copy_from_slice(data);
        self.at = end;
        Ok(())
    }

    fn u8(&mut self, value: u8) -> Result<()> {
        self.put(&[value])
    }

    fn u16(&mut self, value: u16) -> Result<()> {
        self.put(&value.to_le_bytes())
    }

    fn u32(&mut self, value: u32) -> Result<()> {
        self.put(&value.to_le_bytes())
    }

    fn u64(&mut self, value: u64) -> Result<()> {
        self.put(&value.to_le_bytes())
    }
}

/// Reports the relocation errors of a write. Returns the number of errors.
#[must_use]
pub fn report(errors: &[Diagnostic], diagnostics: &dyn DiagnosticSink) -> usize {
    for error in errors {
        diagnostics.emit(error.clone());
    }
    errors.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dos_stub_points_at_the_pe_header() {
        assert_eq!(&DOS_STUB[0..2], b"MZ");
        assert_eq!(
            u32::from_le_bytes([DOS_STUB[60], DOS_STUB[61], DOS_STUB[62], DOS_STUB[63]]),
            128
        );
    }

    #[test]
    fn header_size_matches_gnu_ld() {
        // 128 + 4 + 20 + 240 + 9 * 40 = 752 for a nine-section image.
        assert_eq!(header_size(9, Machine::Amd64), 752);
        // PE32's optional header is 16 bytes shorter.
        assert_eq!(header_size(7, Machine::I386), 128 + 4 + 20 + 224 + 7 * 40);
    }

    #[test]
    fn checksum_ignores_its_own_field() {
        let mut bytes = vec![0u8; 64];
        bytes[10] = 0xff;
        let skip = 20;
        let first = compute_checksum(&bytes, skip);
        bytes[skip] = 0xab;
        bytes[skip + 3] = 0xcd;
        assert_eq!(compute_checksum(&bytes, skip), first);
    }
}
