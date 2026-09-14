//! File format identification by magic number.
//!
//! [`identify`] looks only at a file's headers: it never walks section tables
//! or load commands. It is cheap enough to run on every input and every
//! archive member.
//!
//! GCC LTO IR cannot be recognized from headers: it is an ordinary ELF
//! relocatable file whose sections are named `.gnu.lto_*`. Detecting it needs
//! the ELF section reader, which this module does not depend on. Callers that
//! have one pass a [`GccLtoProbe`] to [`identify_with`].

use crate::target::{Architecture, Endianness, PointerWidth};

use super::read;

/// Identity of an input file, as far as its headers tell.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileFormat {
    /// An ELF file (`\x7fELF`).
    Elf(ElfIdent),
    /// An ELF relocatable file carrying GCC LTO IR (`.gnu.lto_*` sections).
    /// Only produced by [`identify_with`], when the probe says so.
    GccLtoIr(ElfIdent),
    /// A COFF object file.
    Coff(CoffIdent),
    /// A COFF short import library member (`IMPORT_OBJECT_HEADER`).
    CoffImport(CoffImportIdent),
    /// A PE image: an EXE or DLL (`MZ` … `PE\0\0`).
    Pe(PeIdent),
    /// A Mach-O file (object, dylib, executable, …).
    MachO(MachOIdent),
    /// A universal (fat) Mach-O container.
    Fat(FatIdent),
    /// A regular `ar` archive (`!<arch>\n`).
    Archive,
    /// A thin `ar` archive (`!<thin>\n`).
    ThinArchive,
    /// LLVM bitcode, raw (`BC\xC0\xDE`) or inside a bitcode wrapper.
    LlvmBitcode(BitcodeIdent),
    /// Text: a linker script, a module-definition file or a `.tbd` stub.
    Text(TextKind),
    /// A zero-length file. GNU ld treats it as an empty linker script.
    Empty,
    /// Nothing recognized.
    Unknown,
}

impl FileFormat {
    /// Whether this is an `ar` archive, thin or not.
    #[must_use]
    pub fn is_archive(self) -> bool {
        matches!(self, Self::Archive | Self::ThinArchive)
    }

    /// Whether this carries compiler IR that needs LTO.
    #[must_use]
    pub fn is_ir(self) -> bool {
        matches!(self, Self::GccLtoIr(_) | Self::LlvmBitcode(_))
    }

    /// The processor architecture the headers name, when they name one that
    /// qld knows.
    #[must_use]
    pub fn architecture(self) -> Option<Architecture> {
        match self {
            Self::Elf(ident) | Self::GccLtoIr(ident) => ident.architecture(),
            Self::Coff(ident) => coff_architecture(ident.machine),
            Self::CoffImport(ident) => coff_architecture(ident.machine),
            Self::Pe(ident) => coff_architecture(ident.machine),
            Self::MachO(ident) => ident.architecture(),
            _ => None,
        }
    }
}

/// Header fields of an ELF file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ElfIdent {
    /// `EI_CLASS`: 32- or 64-bit.
    pub class: PointerWidth,
    /// `EI_DATA`: byte order.
    pub endian: Endianness,
    /// `EI_OSABI`.
    pub os_abi: u8,
    /// `e_type` (`ET_REL` = 1, `ET_EXEC` = 2, `ET_DYN` = 3).
    pub file_type: u16,
    /// `e_machine`.
    pub machine: u16,
}

/// ELF `e_type` values.
pub mod elf_type {
    /// Relocatable object.
    pub const REL: u16 = 1;
    /// Executable.
    pub const EXEC: u16 = 2;
    /// Shared object or PIE.
    pub const DYN: u16 = 3;
}

impl ElfIdent {
    /// Whether this is a relocatable object (`ET_REL`).
    #[must_use]
    pub fn is_relocatable(&self) -> bool {
        self.file_type == elf_type::REL
    }

    /// Whether this is a shared object (`ET_DYN`).
    #[must_use]
    pub fn is_shared(&self) -> bool {
        self.file_type == elf_type::DYN
    }

    /// The architecture named by `e_machine` and the class.
    #[must_use]
    pub fn architecture(&self) -> Option<Architecture> {
        let is64 = self.class == PointerWidth::Bits64;
        Some(match (self.machine, is64) {
            (62, true) => Architecture::X86_64,
            (62, false) => Architecture::X86_64X32,
            (3, false) => Architecture::X86,
            (183, true) => Architecture::Aarch64,
            (40, false) => Architecture::Arm,
            (243, true) => Architecture::Riscv64,
            (243, false) => Architecture::Riscv32,
            (21, true) => Architecture::PowerPc64,
            (258, true) => Architecture::LoongArch64,
            (22, true) => Architecture::S390x,
            _ => return None,
        })
    }
}

/// Header fields of a COFF object file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CoffIdent {
    /// `Machine` (`IMAGE_FILE_MACHINE_*`).
    pub machine: u16,
    /// Whether this is a `/bigobj` object (`ANON_OBJECT_HEADER_BIGOBJ`).
    pub bigobj: bool,
}

/// Header fields of a COFF short import library member.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CoffImportIdent {
    /// `Machine` (`IMAGE_FILE_MACHINE_*`).
    pub machine: u16,
    /// `SizeOfData`: bytes of symbol and DLL name that follow the header.
    pub size_of_data: u32,
}

/// Header fields of a PE image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PeIdent {
    /// `Machine` from the COFF file header.
    pub machine: u16,
    /// `Characteristics` from the COFF file header.
    pub characteristics: u16,
    /// Whether the optional header is PE32+ (`0x20b`) rather than PE32.
    pub pe32_plus: bool,
}

impl PeIdent {
    /// Whether the image is a DLL (`IMAGE_FILE_DLL`).
    #[must_use]
    pub fn is_dll(&self) -> bool {
        self.characteristics & 0x2000 != 0
    }
}

/// `IMAGE_FILE_MACHINE_*` values recognized for COFF objects.
pub mod coff_machine {
    /// Intel 386.
    pub const I386: u16 = 0x014c;
    /// x86-64.
    pub const AMD64: u16 = 0x8664;
    /// ARM64.
    pub const ARM64: u16 = 0xaa64;
    /// ARM64EC.
    pub const ARM64EC: u16 = 0xa641;
    /// ARM64X.
    pub const ARM64X: u16 = 0xa64e;
    /// ARM little-endian.
    pub const ARM: u16 = 0x01c0;
    /// ARM Thumb-2 (ARMNT).
    pub const ARMNT: u16 = 0x01c4;
    /// Itanium.
    pub const IA64: u16 = 0x0200;
    /// RISC-V 32-bit.
    pub const RISCV32: u16 = 0x5032;
    /// RISC-V 64-bit.
    pub const RISCV64: u16 = 0x5064;
    /// LoongArch 64-bit.
    pub const LOONGARCH64: u16 = 0x6264;

    /// Every value [`super::identify`] accepts as a COFF object machine.
    pub const KNOWN: &[u16] = &[
        I386,
        AMD64,
        ARM64,
        ARM64EC,
        ARM64X,
        ARM,
        ARMNT,
        IA64,
        RISCV32,
        RISCV64,
        LOONGARCH64,
    ];
}

fn coff_architecture(machine: u16) -> Option<Architecture> {
    Some(match machine {
        coff_machine::I386 => Architecture::X86,
        coff_machine::AMD64 => Architecture::X86_64,
        coff_machine::ARM64 | coff_machine::ARM64EC | coff_machine::ARM64X => Architecture::Aarch64,
        coff_machine::ARM | coff_machine::ARMNT => Architecture::Arm,
        coff_machine::RISCV32 => Architecture::Riscv32,
        coff_machine::RISCV64 => Architecture::Riscv64,
        coff_machine::LOONGARCH64 => Architecture::LoongArch64,
        _ => return None,
    })
}

/// Header fields of a Mach-O file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MachOIdent {
    /// 32-bit (`MH_MAGIC`) or 64-bit (`MH_MAGIC_64`) header.
    pub width: PointerWidth,
    /// Byte order of the header and the rest of the file.
    pub endian: Endianness,
    /// `cputype`.
    pub cpu_type: u32,
    /// `cpusubtype`.
    pub cpu_subtype: u32,
    /// `filetype` (`MH_OBJECT` = 1, `MH_EXECUTE` = 2, `MH_DYLIB` = 6, …).
    pub file_type: u32,
}

impl MachOIdent {
    /// The architecture named by `cputype`.
    #[must_use]
    pub fn architecture(&self) -> Option<Architecture> {
        Some(match self.cpu_type {
            0x0100_0007 => Architecture::X86_64,
            7 => Architecture::X86,
            0x0100_000c => Architecture::Aarch64,
            12 => Architecture::Arm,
            0x0100_0012 => Architecture::PowerPc64,
            _ => return None,
        })
    }
}

/// Header fields of a universal (fat) binary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FatIdent {
    /// `FAT_MAGIC_64` (64-bit offsets) rather than `FAT_MAGIC`.
    pub is64: bool,
    /// `nfat_arch`: the number of slices.
    pub arch_count: u32,
}

/// Details of an LLVM bitcode file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BitcodeIdent {
    /// The wrapper header, when the bitcode is wrapped (as Darwin toolchains
    /// emit it). `None` for raw bitcode.
    pub wrapper: Option<BitcodeWrapper>,
}

/// The fields of a bitcode wrapper header (magic `0x0B17C0DE`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BitcodeWrapper {
    /// Offset of the raw bitcode within the file.
    pub offset: u32,
    /// Size of the raw bitcode.
    pub size: u32,
    /// Mach-O `cputype` of the target.
    pub cpu_type: u32,
}

/// What kind of text file this looks like.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextKind {
    /// An Apple text-based stub (`.tbd`): YAML starting `--- !tapi-tbd`, or
    /// JSON with a `tapi_tbd_version` key near the start.
    Tbd,
    /// Any other text: a linker script or a module-definition file. Telling
    /// those apart takes the file name or a parser.
    Other,
}

/// Decides whether an ELF file carries GCC LTO IR, usually by looking for
/// `.gnu.lto_*` section names. Called only for ELF files.
///
/// A plain function pointer, so that it is `Copy`, `Send` and `Sync` and can
/// be stored in a [`super::FileTable`] shared by parallel loaders.
pub type GccLtoProbe = fn(data: &[u8], ident: &ElfIdent) -> bool;

/// Identifies `data` from its magic number and headers.
///
/// Never returns [`FileFormat::GccLtoIr`]; use [`identify_with`] for that.
#[must_use]
pub fn identify(data: &[u8]) -> FileFormat {
    identify_with(data, None)
}

/// Identifies `data`, asking `gcc_lto` whether an ELF file is GCC LTO IR.
#[must_use]
pub fn identify_with(data: &[u8], gcc_lto: Option<GccLtoProbe>) -> FileFormat {
    if data.is_empty() {
        return FileFormat::Empty;
    }
    if let Some(ident) = elf(data) {
        if let Some(probe) = gcc_lto
            && probe(data, &ident)
        {
            return FileFormat::GccLtoIr(ident);
        }
        return FileFormat::Elf(ident);
    }
    if data.starts_with(b"!<arch>\n") {
        return FileFormat::Archive;
    }
    if data.starts_with(b"!<thin>\n") {
        return FileFormat::ThinArchive;
    }
    if let Some(format) = macho_or_fat(data) {
        return format;
    }
    if data.starts_with(b"BC\xc0\xde") {
        return FileFormat::LlvmBitcode(BitcodeIdent { wrapper: None });
    }
    if let Some(wrapper) = bitcode_wrapper(data) {
        return FileFormat::LlvmBitcode(BitcodeIdent {
            wrapper: Some(wrapper),
        });
    }
    if let Some(format) = coff_import_or_anon(data) {
        return format;
    }
    if let Some(ident) = pe(data) {
        return FileFormat::Pe(ident);
    }
    if let Some(ident) = coff(data) {
        return FileFormat::Coff(ident);
    }
    if let Some(kind) = text(data) {
        return FileFormat::Text(kind);
    }
    FileFormat::Unknown
}

fn elf(data: &[u8]) -> Option<ElfIdent> {
    if !data.starts_with(b"\x7fELF") {
        return None;
    }
    let class = match *data.get(4)? {
        1 => PointerWidth::Bits32,
        2 => PointerWidth::Bits64,
        _ => return None,
    };
    let endian = match *data.get(5)? {
        1 => Endianness::Little,
        2 => Endianness::Big,
        _ => return None,
    };
    let os_abi = *data.get(7)?;
    let (file_type, machine) = match endian {
        Endianness::Little => (read::u16_le(data, 16)?, read::u16_le(data, 18)?),
        Endianness::Big => (read::u16_be(data, 16)?, read::u16_be(data, 18)?),
    };
    Some(ElfIdent {
        class,
        endian,
        os_abi,
        file_type,
        machine,
    })
}

/// Java class files share `0xCAFEBABE` with `FAT_MAGIC`. In a class file the
/// next four bytes are the minor and major version, and every major version
/// is at least 45, so a fat header is one whose `nfat_arch` is smaller.
const JAVA_MIN_MAJOR: u32 = 45;

fn macho_or_fat(data: &[u8]) -> Option<FileFormat> {
    let magic = read::u32_be(data, 0)?;
    let (width, endian) = match magic {
        0xcafe_babe | 0xcafe_babf => {
            let arch_count = read::u32_be(data, 4)?;
            if arch_count >= JAVA_MIN_MAJOR {
                return None;
            }
            return Some(FileFormat::Fat(FatIdent {
                is64: magic == 0xcafe_babf,
                arch_count,
            }));
        }
        0xfeed_face => (PointerWidth::Bits32, Endianness::Big),
        0xfeed_facf => (PointerWidth::Bits64, Endianness::Big),
        0xcefa_edfe => (PointerWidth::Bits32, Endianness::Little),
        0xcffa_edfe => (PointerWidth::Bits64, Endianness::Little),
        _ => return None,
    };
    let field = |offset| match endian {
        Endianness::Little => read::u32_le(data, offset),
        Endianness::Big => read::u32_be(data, offset),
    };
    Some(FileFormat::MachO(MachOIdent {
        width,
        endian,
        cpu_type: field(4)?,
        cpu_subtype: field(8)?,
        file_type: field(12)?,
    }))
}

fn bitcode_wrapper(data: &[u8]) -> Option<BitcodeWrapper> {
    if read::u32_le(data, 0)? != 0x0b17_c0de {
        return None;
    }
    Some(BitcodeWrapper {
        offset: read::u32_le(data, 8)?,
        size: read::u32_le(data, 12)?,
        cpu_type: read::u32_le(data, 16)?,
    })
}

/// `ClassID` of `ANON_OBJECT_HEADER_BIGOBJ`:
/// `{D1BAA1C7-BAEE-4BA9-AF20-FAF66AA4DCB8}` as stored on disk.
const BIGOBJ_CLASS_ID: [u8; 16] = [
    0xc7, 0xa1, 0xba, 0xd1, 0xee, 0xba, 0xa9, 0x4b, 0xaf, 0x20, 0xfa, 0xf6, 0x6a, 0xa4, 0xdc, 0xb8,
];

/// Short import members and anonymous objects both start with
/// `Sig1 = IMAGE_FILE_MACHINE_UNKNOWN (0)`, `Sig2 = 0xFFFF`.
fn coff_import_or_anon(data: &[u8]) -> Option<FileFormat> {
    if read::u16_le(data, 0)? != 0 || read::u16_le(data, 2)? != 0xffff {
        return None;
    }
    let version = read::u16_le(data, 4)?;
    let machine = read::u16_le(data, 6)?;
    if version == 0 {
        // IMPORT_OBJECT_HEADER is 20 bytes.
        let size_of_data = read::u32_le(data, 12)?;
        read::bytes(data, 0, 20)?;
        return Some(FileFormat::CoffImport(CoffImportIdent {
            machine,
            size_of_data,
        }));
    }
    if version >= 2 && read::array::<16>(data, 12)? == BIGOBJ_CLASS_ID {
        return Some(FileFormat::Coff(CoffIdent {
            machine,
            bigobj: true,
        }));
    }
    None
}

fn pe(data: &[u8]) -> Option<PeIdent> {
    if !data.starts_with(b"MZ") {
        return None;
    }
    let header = read::to_usize(u64::from(read::u32_le(data, 0x3c)?))?;
    if read::bytes(data, header, 4)? != b"PE\0\0" {
        return None;
    }
    let coff = header.checked_add(4)?;
    let machine = read::u16_le(data, coff)?;
    let characteristics = read::u16_le(data, coff.checked_add(18)?)?;
    let optional_magic = read::u16_le(data, coff.checked_add(20)?);
    Some(PeIdent {
        machine,
        characteristics,
        pe32_plus: optional_magic == Some(0x20b),
    })
}

/// A COFF object has no magic number, only a machine field. To avoid
/// claiming arbitrary data, also require an empty optional header, a sane
/// section count, and a section table and symbol table inside the file.
fn coff(data: &[u8]) -> Option<CoffIdent> {
    let machine = read::u16_le(data, 0)?;
    if !coff_machine::KNOWN.contains(&machine) {
        return None;
    }
    let sections = usize::from(read::u16_le(data, 2)?);
    let symbol_table = read::to_usize(u64::from(read::u32_le(data, 8)?))?;
    let symbol_count = read::to_usize(u64::from(read::u32_le(data, 12)?))?;
    let optional_header_size = read::u16_le(data, 16)?;
    if optional_header_size != 0 {
        return None;
    }
    let section_table_end = sections.checked_mul(40)?.checked_add(20)?;
    if section_table_end > data.len() {
        return None;
    }
    if symbol_table != 0 || symbol_count != 0 {
        let symbols_end = symbol_count.checked_mul(18)?.checked_add(symbol_table)?;
        if symbol_table < section_table_end || symbols_end > data.len() {
            return None;
        }
    }
    Some(CoffIdent {
        machine,
        bigobj: false,
    })
}

/// How many leading bytes the text heuristic inspects.
const TEXT_PROBE_LEN: usize = 1024;

fn text(data: &[u8]) -> Option<TextKind> {
    let probe = data.get(..TEXT_PROBE_LEN).unwrap_or(data);
    // A multi-byte UTF-8 sequence may be cut at the end of the probe window.
    let valid = match std::str::from_utf8(probe) {
        Ok(text) => text,
        Err(error) if error.error_len().is_none() => {
            std::str::from_utf8(probe.get(..error.valid_up_to())?).ok()?
        }
        Err(_) => return None,
    };
    let is_text = valid
        .chars()
        .all(|c| !c.is_control() || matches!(c, '\t' | '\n' | '\r' | '\x0c'));
    if !is_text {
        return None;
    }
    let trimmed = valid.trim_start_matches('\u{feff}').trim_start();
    if trimmed.starts_with("--- !tapi-tbd") {
        return Some(TextKind::Tbd);
    }
    if trimmed.starts_with('{') && trimmed.contains("\"tapi_tbd_version\"") {
        return Some(TextKind::Tbd);
    }
    Some(TextKind::Other)
}

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)] // Test code builds fixtures, not parses input.
mod tests {
    use super::*;

    fn elf_header(class: u8, data: u8, e_type: u16, machine: u16) -> Vec<u8> {
        let mut bytes = vec![0u8; 64];
        bytes[..4].copy_from_slice(b"\x7fELF");
        bytes[4] = class;
        bytes[5] = data;
        bytes[6] = 1;
        let (t, m) = if data == 1 {
            (e_type.to_le_bytes(), machine.to_le_bytes())
        } else {
            (e_type.to_be_bytes(), machine.to_be_bytes())
        };
        bytes[16..18].copy_from_slice(&t);
        bytes[18..20].copy_from_slice(&m);
        bytes
    }

    #[test]
    fn elf_class_endianness_and_machine() {
        let FileFormat::Elf(ident) = identify(&elf_header(2, 1, 1, 62)) else {
            panic!("not ELF");
        };
        assert_eq!(ident.class, PointerWidth::Bits64);
        assert_eq!(ident.endian, Endianness::Little);
        assert!(ident.is_relocatable());
        assert_eq!(ident.architecture(), Some(Architecture::X86_64));

        let FileFormat::Elf(ident) = identify(&elf_header(2, 2, 3, 22)) else {
            panic!("not ELF");
        };
        assert_eq!(ident.endian, Endianness::Big);
        assert!(ident.is_shared());
        assert_eq!(ident.architecture(), Some(Architecture::S390x));

        let format = identify(&elf_header(1, 1, 1, 62));
        assert_eq!(format.architecture(), Some(Architecture::X86_64X32));

        // Invalid class or truncated header.
        assert_eq!(identify(&elf_header(3, 1, 1, 62)), FileFormat::Unknown);
        assert_eq!(identify(b"\x7fELF\x02\x01"), FileFormat::Unknown);
    }

    #[test]
    fn gcc_lto_probe_is_consulted_for_elf_only() {
        fn yes(_: &[u8], ident: &ElfIdent) -> bool {
            ident.is_relocatable()
        }
        let object = elf_header(2, 1, 1, 62);
        assert!(matches!(
            identify_with(&object, Some(yes)),
            FileFormat::GccLtoIr(_)
        ));
        assert!(identify_with(&object, Some(yes)).is_ir());
        assert!(matches!(
            identify_with(&elf_header(2, 1, 3, 62), Some(yes)),
            FileFormat::Elf(_)
        ));
        assert_eq!(identify_with(b"!<arch>\n", Some(yes)), FileFormat::Archive);
    }

    #[test]
    fn archives() {
        assert_eq!(identify(b"!<arch>\n"), FileFormat::Archive);
        assert_eq!(identify(b"!<thin>\n/ "), FileFormat::ThinArchive);
        assert!(identify(b"!<arch>\n").is_archive());
        // Truncated magic is just text.
        assert_eq!(identify(b"!<arch>"), FileFormat::Text(TextKind::Other));
    }

    #[test]
    fn macho_all_magics() {
        let mut header = vec![0u8; 28];
        header[..4].copy_from_slice(&0xfeed_facfu32.to_le_bytes());
        header[4..8].copy_from_slice(&0x0100_000cu32.to_le_bytes());
        header[12..16].copy_from_slice(&1u32.to_le_bytes());
        let FileFormat::MachO(ident) = identify(&header) else {
            panic!("not Mach-O");
        };
        assert_eq!(ident.width, PointerWidth::Bits64);
        assert_eq!(ident.endian, Endianness::Little);
        assert_eq!(ident.architecture(), Some(Architecture::Aarch64));
        assert_eq!(ident.file_type, 1);

        let mut header = vec![0u8; 28];
        header[..4].copy_from_slice(&0xfeed_faceu32.to_be_bytes());
        header[4..8].copy_from_slice(&18u32.to_be_bytes());
        let FileFormat::MachO(ident) = identify(&header) else {
            panic!("not Mach-O");
        };
        assert_eq!(ident.width, PointerWidth::Bits32);
        assert_eq!(ident.endian, Endianness::Big);
        assert_eq!(ident.cpu_type, 18);

        header[..4].copy_from_slice(&0xfeed_facfu32.to_be_bytes());
        assert!(
            matches!(identify(&header), FileFormat::MachO(m) if m.width == PointerWidth::Bits64 && m.endian == Endianness::Big)
        );
        header[..4].copy_from_slice(&0xfeed_faceu32.to_le_bytes());
        assert!(
            matches!(identify(&header), FileFormat::MachO(m) if m.width == PointerWidth::Bits32 && m.endian == Endianness::Little)
        );

        // Truncated header.
        assert_eq!(identify(&header[..10]), FileFormat::Unknown);
    }

    #[test]
    fn fat_versus_java_class() {
        let mut fat = 0xcafe_babeu32.to_be_bytes().to_vec();
        fat.extend_from_slice(&2u32.to_be_bytes());
        assert_eq!(
            identify(&fat),
            FileFormat::Fat(FatIdent {
                is64: false,
                arch_count: 2
            })
        );
        let mut fat64 = 0xcafe_babfu32.to_be_bytes().to_vec();
        fat64.extend_from_slice(&1u32.to_be_bytes());
        assert!(matches!(identify(&fat64), FileFormat::Fat(f) if f.is64));

        // Java 8 class file: minor 0, major 52.
        let mut class = 0xcafe_babeu32.to_be_bytes().to_vec();
        class.extend_from_slice(&[0, 0, 0, 52]);
        assert_eq!(identify(&class), FileFormat::Unknown);
        // Minor version 3, major 45 (JDK 1.1).
        let class = [0xca, 0xfe, 0xba, 0xbe, 0, 3, 0, 45];
        assert_eq!(identify(&class), FileFormat::Unknown);
    }

    #[test]
    fn llvm_bitcode_raw_and_wrapped() {
        assert_eq!(
            identify(b"BC\xc0\xde\x35\x14\x00\x00"),
            FileFormat::LlvmBitcode(BitcodeIdent { wrapper: None })
        );
        let mut wrapped = Vec::new();
        for field in [0x0b17_c0deu32, 0, 20, 8, 0x0100_0007] {
            wrapped.extend_from_slice(&field.to_le_bytes());
        }
        wrapped.extend_from_slice(b"BC\xc0\xde\0\0\0\0");
        let FileFormat::LlvmBitcode(BitcodeIdent {
            wrapper: Some(wrapper),
        }) = identify(&wrapped)
        else {
            panic!("not wrapped bitcode");
        };
        assert_eq!((wrapper.offset, wrapper.size), (20, 8));
        assert_eq!(identify(&wrapped[..12]), FileFormat::Unknown);
    }

    #[test]
    fn coff_import_bigobj_pe_and_object() {
        let mut import = vec![0u8; 20];
        import[2..4].copy_from_slice(&0xffffu16.to_le_bytes());
        import[6..8].copy_from_slice(&coff_machine::AMD64.to_le_bytes());
        import[12..16].copy_from_slice(&12u32.to_le_bytes());
        assert_eq!(
            identify(&import),
            FileFormat::CoffImport(CoffImportIdent {
                machine: coff_machine::AMD64,
                size_of_data: 12
            })
        );

        let mut bigobj = vec![0u8; 56];
        bigobj[2..4].copy_from_slice(&0xffffu16.to_le_bytes());
        bigobj[4..6].copy_from_slice(&2u16.to_le_bytes());
        bigobj[6..8].copy_from_slice(&coff_machine::ARM64.to_le_bytes());
        bigobj[12..28].copy_from_slice(&BIGOBJ_CLASS_ID);
        let format = identify(&bigobj);
        assert_eq!(
            format,
            FileFormat::Coff(CoffIdent {
                machine: coff_machine::ARM64,
                bigobj: true
            })
        );
        assert_eq!(format.architecture(), Some(Architecture::Aarch64));

        let mut pe = vec![0u8; 0x100];
        pe[..2].copy_from_slice(b"MZ");
        pe[0x3c..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        pe[0x80..0x84].copy_from_slice(b"PE\0\0");
        pe[0x84..0x86].copy_from_slice(&coff_machine::AMD64.to_le_bytes());
        pe[0x96..0x98].copy_from_slice(&0x2022u16.to_le_bytes());
        pe[0x98..0x9a].copy_from_slice(&0x20bu16.to_le_bytes());
        let FileFormat::Pe(ident) = identify(&pe) else {
            panic!("not PE");
        };
        assert!(ident.is_dll());
        assert!(ident.pe32_plus);
        // e_lfanew out of range.
        pe[0x3c..0x40].copy_from_slice(&0xffff_fff0u32.to_le_bytes());
        assert_eq!(identify(&pe), FileFormat::Unknown);

        // A COFF object with one section and a symbol table after it.
        let mut object = vec![0u8; 20 + 40 + 18];
        object[..2].copy_from_slice(&coff_machine::I386.to_le_bytes());
        object[2..4].copy_from_slice(&1u16.to_le_bytes());
        object[8..12].copy_from_slice(&60u32.to_le_bytes());
        object[12..16].copy_from_slice(&1u32.to_le_bytes());
        assert_eq!(
            identify(&object),
            FileFormat::Coff(CoffIdent {
                machine: coff_machine::I386,
                bigobj: false
            })
        );
        // Symbol table running past the end.
        object[12..16].copy_from_slice(&2u32.to_le_bytes());
        assert_eq!(identify(&object), FileFormat::Unknown);
    }

    #[test]
    fn text_and_tbd() {
        assert_eq!(identify(b""), FileFormat::Empty);
        assert_eq!(
            identify(b"GROUP ( /lib/libc.so.6 )\n"),
            FileFormat::Text(TextKind::Other)
        );
        assert_eq!(
            identify(b"--- !tapi-tbd\ntbd-version: 4\n"),
            FileFormat::Text(TextKind::Tbd)
        );
        assert_eq!(
            identify(b"{\n  \"tapi_tbd_version\": 5,\n}"),
            FileFormat::Text(TextKind::Tbd)
        );
        assert_eq!(identify(b"abc\0def"), FileFormat::Unknown);
        assert_eq!(identify(b"\xff\xfe"), FileFormat::Unknown);
        // A UTF-8 sequence cut by the probe window is still text.
        let mut long = vec![b'a'; TEXT_PROBE_LEN - 1];
        long.extend_from_slice("é".as_bytes());
        assert_eq!(identify(&long), FileFormat::Text(TextKind::Other));
    }

    #[test]
    fn never_panics_on_prefixes() {
        let samples: Vec<Vec<u8>> = vec![
            elf_header(2, 1, 1, 62),
            b"MZ\0\0".to_vec(),
            vec![0, 0, 0xff, 0xff, 2, 0, 0x64, 0x86],
            0xcafe_babeu32.to_be_bytes().to_vec(),
            0x0b17_c0deu32.to_le_bytes().to_vec(),
            vec![0x4c, 0x01, 0xff, 0xff, 0, 0, 0, 0, 0xff, 0xff, 0xff, 0xff],
        ];
        for sample in samples {
            for len in 0..=sample.len() {
                let _ = identify(&sample[..len]);
            }
        }
    }
}
