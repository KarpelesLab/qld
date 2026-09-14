//! The ELF file header.

use super::consts::{
    ELFCLASS32, ELFCLASS64, EM_386, EM_AARCH64, EM_ARM, EM_IAMCU, EM_LOONGARCH, EM_PPC64, EM_RISCV,
    EM_S390, EM_X86_64,
};
use crate::target::Architecture;

/// A decoded ELF header (`Elf32_Ehdr` / `Elf64_Ehdr`), with address-sized
/// fields widened to 64 bits.
///
/// `e_shnum`, `e_shstrndx` and `e_phnum` are the raw header values; the
/// counts after extended-numbering resolution are on
/// [`ElfFile`](super::ElfFile).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FileHeader {
    /// `e_ident[EI_CLASS]`.
    pub class: u8,
    /// `e_ident[EI_DATA]`.
    pub data: u8,
    /// `e_ident[EI_VERSION]`.
    pub ident_version: u8,
    /// `e_ident[EI_OSABI]`.
    pub os_abi: u8,
    /// `e_ident[EI_ABIVERSION]`.
    pub abi_version: u8,
    /// Object file type (`ET_*`).
    pub e_type: u16,
    /// Machine (`EM_*`).
    pub e_machine: u16,
    /// Object file version.
    pub e_version: u32,
    /// Entry point address.
    pub e_entry: u64,
    /// Program header table offset.
    pub e_phoff: u64,
    /// Section header table offset.
    pub e_shoff: u64,
    /// Processor-specific flags.
    pub e_flags: u32,
    /// ELF header size.
    pub e_ehsize: u16,
    /// Program header entry size.
    pub e_phentsize: u16,
    /// Number of program headers (raw; may be `PN_XNUM`).
    pub e_phnum: u16,
    /// Section header entry size.
    pub e_shentsize: u16,
    /// Number of section headers (raw; may be 0 with extended numbering).
    pub e_shnum: u16,
    /// Section name string table index (raw; may be `SHN_XINDEX`).
    pub e_shstrndx: u16,
}

impl FileHeader {
    /// Maps `e_machine` and the class to a qld [`Architecture`].
    ///
    /// Returns `None` for machines qld does not target. x86-64 objects of
    /// class 32 are x32.
    #[must_use]
    pub fn architecture(&self) -> Option<Architecture> {
        architecture(self.e_machine, self.class)
    }
}

/// Maps an `e_machine` value and an ELF class to an [`Architecture`].
#[must_use]
pub fn architecture(e_machine: u16, class: u8) -> Option<Architecture> {
    match (e_machine, class) {
        (EM_X86_64, ELFCLASS64) => Some(Architecture::X86_64),
        (EM_X86_64, ELFCLASS32) => Some(Architecture::X86_64X32),
        (EM_386 | EM_IAMCU, ELFCLASS32) => Some(Architecture::X86),
        (EM_AARCH64, ELFCLASS64) => Some(Architecture::Aarch64),
        (EM_ARM, ELFCLASS32) => Some(Architecture::Arm),
        (EM_RISCV, ELFCLASS64) => Some(Architecture::Riscv64),
        (EM_RISCV, ELFCLASS32) => Some(Architecture::Riscv32),
        (EM_PPC64, ELFCLASS64) => Some(Architecture::PowerPc64),
        (EM_LOONGARCH, ELFCLASS64) => Some(Architecture::LoongArch64),
        (EM_S390, ELFCLASS64) => Some(Architecture::S390x),
        _ => None,
    }
}
