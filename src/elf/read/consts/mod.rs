//! ELF ABI constants.
//!
//! Values come from the System V gABI, the GNU extensions documented in
//! glibc's `elf.h` and binutils' `include/elf/common.h`, and the processor
//! supplements. Constants are typed by the width of the field they are
//! compared against once decoded (for example `sh_type` is a `u32`,
//! `sh_flags` a `u64` after widening, `d_tag` an `i64`).
//!
//! Relocation types live in per-architecture submodules, each with a
//! `reloc_name` function for diagnostics.

/// Declares relocation type constants and a name lookup function.
macro_rules! relocation_types {
    ($(#[$fn_doc:meta])* $fn_name:ident; $($name:ident = $value:expr,)*) => {
        $(
            #[doc = concat!("Relocation type `", stringify!($name), "`.")]
            pub const $name: u32 = $value;
        )*

        $(#[$fn_doc])*
        #[must_use]
        pub fn $fn_name(r_type: u32) -> Option<&'static str> {
            match r_type {
                $($name => Some(stringify!($name)),)*
                _ => None,
            }
        }
    };
}

pub mod aarch64;
pub mod i386;
pub mod ppc64;
pub mod riscv;
pub mod x86_64;

/// Returns the name of relocation type `r_type` for machine `e_machine`, when
/// the machine is one qld has a relocation table for and the type is known.
#[must_use]
pub fn reloc_name(e_machine: u16, r_type: u32) -> Option<&'static str> {
    match e_machine {
        EM_X86_64 => x86_64::reloc_name(r_type),
        EM_AARCH64 => aarch64::reloc_name(r_type),
        EM_RISCV => riscv::reloc_name(r_type),
        EM_386 | EM_IAMCU => i386::reloc_name(r_type),
        EM_PPC64 => ppc64::reloc_name(r_type),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// e_ident
// ---------------------------------------------------------------------------

/// The four magic bytes at the start of every ELF file.
pub const ELFMAG: [u8; 4] = *b"\x7fELF";
/// Size of `e_ident`.
pub const EI_NIDENT: usize = 16;
/// Index of the file class byte in `e_ident`.
pub const EI_CLASS: usize = 4;
/// Index of the data encoding byte in `e_ident`.
pub const EI_DATA: usize = 5;
/// Index of the ELF header version byte in `e_ident`.
pub const EI_VERSION: usize = 6;
/// Index of the OS ABI byte in `e_ident`.
pub const EI_OSABI: usize = 7;
/// Index of the ABI version byte in `e_ident`.
pub const EI_ABIVERSION: usize = 8;

/// Invalid class.
pub const ELFCLASSNONE: u8 = 0;
/// 32-bit objects.
pub const ELFCLASS32: u8 = 1;
/// 64-bit objects.
pub const ELFCLASS64: u8 = 2;

/// Invalid data encoding.
pub const ELFDATANONE: u8 = 0;
/// Two's complement, little-endian.
pub const ELFDATA2LSB: u8 = 1;
/// Two's complement, big-endian.
pub const ELFDATA2MSB: u8 = 2;

/// The current ELF version, in `e_ident[EI_VERSION]` and `e_version`.
pub const EV_CURRENT: u8 = 1;

/// No OS extensions (System V).
pub const ELFOSABI_NONE: u8 = 0;
/// GNU/Linux extensions (`ELFOSABI_LINUX`).
pub const ELFOSABI_GNU: u8 = 3;
/// FreeBSD.
pub const ELFOSABI_FREEBSD: u8 = 9;
/// Standalone (embedded) application.
pub const ELFOSABI_STANDALONE: u8 = 255;

// ---------------------------------------------------------------------------
// e_type
// ---------------------------------------------------------------------------

/// No file type.
pub const ET_NONE: u16 = 0;
/// Relocatable object.
pub const ET_REL: u16 = 1;
/// Executable.
pub const ET_EXEC: u16 = 2;
/// Shared object (or position-independent executable).
pub const ET_DYN: u16 = 3;
/// Core dump.
pub const ET_CORE: u16 = 4;

// ---------------------------------------------------------------------------
// e_machine
// ---------------------------------------------------------------------------

/// No machine.
pub const EM_NONE: u16 = 0;
/// SPARC.
pub const EM_SPARC: u16 = 2;
/// Intel 80386.
pub const EM_386: u16 = 3;
/// Motorola 68000.
pub const EM_68K: u16 = 4;
/// MIPS.
pub const EM_MIPS: u16 = 8;
/// 32-bit PowerPC.
pub const EM_PPC: u16 = 20;
/// 64-bit PowerPC.
pub const EM_PPC64: u16 = 21;
/// IBM S/390 and z/Architecture.
pub const EM_S390: u16 = 22;
/// 32-bit Arm.
pub const EM_ARM: u16 = 40;
/// Hitachi SH.
pub const EM_SH: u16 = 42;
/// SPARC v9 (64-bit).
pub const EM_SPARCV9: u16 = 43;
/// Intel MCU (i386 ABI variant).
pub const EM_IAMCU: u16 = 6;
/// Intel Itanium.
pub const EM_IA_64: u16 = 50;
/// AMD x86-64.
pub const EM_X86_64: u16 = 62;
/// Atmel AVR.
pub const EM_AVR: u16 = 83;
/// TI MSP430.
pub const EM_MSP430: u16 = 105;
/// 64-bit Arm.
pub const EM_AARCH64: u16 = 183;
/// RISC-V.
pub const EM_RISCV: u16 = 243;
/// Linux BPF.
pub const EM_BPF: u16 = 247;
/// LoongArch.
pub const EM_LOONGARCH: u16 = 258;

// ---------------------------------------------------------------------------
// Special section indices
// ---------------------------------------------------------------------------

/// Undefined section.
pub const SHN_UNDEF: u16 = 0;
/// Start of the reserved index range.
pub const SHN_LORESERVE: u16 = 0xff00;
/// Start of the processor-specific range.
pub const SHN_LOPROC: u16 = 0xff00;
/// End of the processor-specific range.
pub const SHN_HIPROC: u16 = 0xff1f;
/// Start of the OS-specific range.
pub const SHN_LOOS: u16 = 0xff20;
/// End of the OS-specific range.
pub const SHN_HIOS: u16 = 0xff3f;
/// Absolute symbol.
pub const SHN_ABS: u16 = 0xfff1;
/// Common symbol.
pub const SHN_COMMON: u16 = 0xfff2;
/// The real index is elsewhere (`SHT_SYMTAB_SHNDX`, or section 0 for
/// `e_shstrndx`).
pub const SHN_XINDEX: u16 = 0xffff;
/// End of the reserved index range.
pub const SHN_HIRESERVE: u16 = 0xffff;
/// `e_phnum` value meaning the real count is in section 0's `sh_info`.
pub const PN_XNUM: u16 = 0xffff;

// ---------------------------------------------------------------------------
// sh_type
// ---------------------------------------------------------------------------

/// Inactive section header.
pub const SHT_NULL: u32 = 0;
/// Program-defined contents.
pub const SHT_PROGBITS: u32 = 1;
/// Static symbol table.
pub const SHT_SYMTAB: u32 = 2;
/// String table.
pub const SHT_STRTAB: u32 = 3;
/// Relocations with explicit addends.
pub const SHT_RELA: u32 = 4;
/// System V symbol hash table.
pub const SHT_HASH: u32 = 5;
/// Dynamic linking information.
pub const SHT_DYNAMIC: u32 = 6;
/// Notes.
pub const SHT_NOTE: u32 = 7;
/// Occupies no file space (`.bss`).
pub const SHT_NOBITS: u32 = 8;
/// Relocations with implicit addends.
pub const SHT_REL: u32 = 9;
/// Reserved.
pub const SHT_SHLIB: u32 = 10;
/// Dynamic symbol table.
pub const SHT_DYNSYM: u32 = 11;
/// Array of constructors.
pub const SHT_INIT_ARRAY: u32 = 14;
/// Array of destructors.
pub const SHT_FINI_ARRAY: u32 = 15;
/// Array of pre-constructors.
pub const SHT_PREINIT_ARRAY: u32 = 16;
/// Section group.
pub const SHT_GROUP: u32 = 17;
/// Extended section indices for a symbol table.
pub const SHT_SYMTAB_SHNDX: u32 = 18;
/// Compact relative relocations.
pub const SHT_RELR: u32 = 19;
/// Start of the OS-specific range.
pub const SHT_LOOS: u32 = 0x6000_0000;
/// Android packed relocations with implicit addends.
pub const SHT_ANDROID_REL: u32 = 0x6000_0001;
/// Android packed relocations with explicit addends.
pub const SHT_ANDROID_RELA: u32 = 0x6000_0002;
/// LLVM ODR table.
pub const SHT_LLVM_ODRTAB: u32 = 0x6fff_4c00;
/// LLVM linker options.
pub const SHT_LLVM_LINKER_OPTIONS: u32 = 0x6fff_4c01;
/// LLVM address-significance table.
pub const SHT_LLVM_ADDRSIG: u32 = 0x6fff_4c03;
/// LLVM dependent libraries.
pub const SHT_LLVM_DEPENDENT_LIBRARIES: u32 = 0x6fff_4c04;
/// LLVM symbol partition specification.
pub const SHT_LLVM_SYMPART: u32 = 0x6fff_4c05;
/// LLVM basic block address map.
pub const SHT_LLVM_BB_ADDR_MAP: u32 = 0x6fff_4c0a;
/// LLVM fat LTO bitcode (`.llvm.lto`).
pub const SHT_LLVM_LTO: u32 = 0x6fff_4c0c;
/// Android compact relative relocations (pre-standard `SHT_RELR`).
pub const SHT_ANDROID_RELR: u32 = 0x6fff_ff00;
/// GNU SFrame stack trace information.
pub const SHT_GNU_SFRAME: u32 = 0x6fff_fff4;
/// GNU object attributes.
pub const SHT_GNU_ATTRIBUTES: u32 = 0x6fff_fff5;
/// GNU-style symbol hash table.
pub const SHT_GNU_HASH: u32 = 0x6fff_fff6;
/// GNU prelink library list.
pub const SHT_GNU_LIBLIST: u32 = 0x6fff_fff7;
/// Checksum for DSO content.
pub const SHT_CHECKSUM: u32 = 0x6fff_fff8;
/// Symbol version definitions (`SHT_GNU_verdef`).
pub const SHT_GNU_VERDEF: u32 = 0x6fff_fffd;
/// Symbol version requirements (`SHT_GNU_verneed`).
pub const SHT_GNU_VERNEED: u32 = 0x6fff_fffe;
/// Symbol version table (`SHT_GNU_versym`).
pub const SHT_GNU_VERSYM: u32 = 0x6fff_ffff;
/// End of the OS-specific range.
pub const SHT_HIOS: u32 = 0x6fff_ffff;
/// Start of the processor-specific range.
pub const SHT_LOPROC: u32 = 0x7000_0000;
/// x86-64 unwind information (`.eh_frame` may use this type).
pub const SHT_X86_64_UNWIND: u32 = 0x7000_0001;
/// Arm exception index table.
pub const SHT_ARM_EXIDX: u32 = 0x7000_0001;
/// Arm build attributes.
pub const SHT_ARM_ATTRIBUTES: u32 = 0x7000_0003;
/// AArch64 build attributes.
pub const SHT_AARCH64_ATTRIBUTES: u32 = 0x7000_0003;
/// RISC-V attributes.
pub const SHT_RISCV_ATTRIBUTES: u32 = 0x7000_0003;
/// End of the processor-specific range.
pub const SHT_HIPROC: u32 = 0x7fff_ffff;
/// Start of the application-specific range.
pub const SHT_LOUSER: u32 = 0x8000_0000;
/// End of the application-specific range.
pub const SHT_HIUSER: u32 = 0xffff_ffff;

// ---------------------------------------------------------------------------
// sh_flags
// ---------------------------------------------------------------------------

/// Writable at run time.
pub const SHF_WRITE: u64 = 0x1;
/// Occupies memory at run time.
pub const SHF_ALLOC: u64 = 0x2;
/// Executable.
pub const SHF_EXECINSTR: u64 = 0x4;
/// Contents may be merged.
pub const SHF_MERGE: u64 = 0x10;
/// Contents are NUL-terminated strings.
pub const SHF_STRINGS: u64 = 0x20;
/// `sh_info` holds a section index.
pub const SHF_INFO_LINK: u64 = 0x40;
/// Preserve order after combining (`sh_link` names the associated section).
pub const SHF_LINK_ORDER: u64 = 0x80;
/// OS-specific processing required.
pub const SHF_OS_NONCONFORMING: u64 = 0x100;
/// Member of a section group.
pub const SHF_GROUP: u64 = 0x200;
/// Holds thread-local storage.
pub const SHF_TLS: u64 = 0x400;
/// Contents are compressed and start with a compression header.
pub const SHF_COMPRESSED: u64 = 0x800;
/// OS-specific flag bits.
pub const SHF_MASKOS: u64 = 0x0ff0_0000;
/// Keep the section even under `--gc-sections`.
pub const SHF_GNU_RETAIN: u64 = 0x0020_0000;
/// Section is bound to a memory region (`SHF_GNU_MBIND`).
pub const SHF_GNU_MBIND: u64 = 0x0100_0000;
/// Processor-specific flag bits.
pub const SHF_MASKPROC: u64 = 0xf000_0000;
/// x86-64: section may be more than 2 GiB away (large code model).
pub const SHF_X86_64_LARGE: u64 = 0x1000_0000;
/// Exclude from the link unless referenced (Solaris/GNU extension).
pub const SHF_EXCLUDE: u64 = 0x8000_0000;

// ---------------------------------------------------------------------------
// Compression
// ---------------------------------------------------------------------------

/// zlib (deflate) compression.
pub const ELFCOMPRESS_ZLIB: u32 = 1;
/// Zstandard compression.
pub const ELFCOMPRESS_ZSTD: u32 = 2;

// ---------------------------------------------------------------------------
// Groups
// ---------------------------------------------------------------------------

/// The group is a COMDAT group.
pub const GRP_COMDAT: u32 = 0x1;

// ---------------------------------------------------------------------------
// Symbols
// ---------------------------------------------------------------------------

/// Local symbol.
pub const STB_LOCAL: u8 = 0;
/// Global symbol.
pub const STB_GLOBAL: u8 = 1;
/// Weak symbol.
pub const STB_WEAK: u8 = 2;
/// GNU unique symbol.
pub const STB_GNU_UNIQUE: u8 = 10;

/// Unspecified type.
pub const STT_NOTYPE: u8 = 0;
/// Data object.
pub const STT_OBJECT: u8 = 1;
/// Function.
pub const STT_FUNC: u8 = 2;
/// Section symbol.
pub const STT_SECTION: u8 = 3;
/// Source file name.
pub const STT_FILE: u8 = 4;
/// Common data object.
pub const STT_COMMON: u8 = 5;
/// Thread-local data object.
pub const STT_TLS: u8 = 6;
/// Indirect function (GNU).
pub const STT_GNU_IFUNC: u8 = 10;

/// Default visibility.
pub const STV_DEFAULT: u8 = 0;
/// Internal visibility.
pub const STV_INTERNAL: u8 = 1;
/// Hidden visibility.
pub const STV_HIDDEN: u8 = 2;
/// Protected visibility.
pub const STV_PROTECTED: u8 = 3;

/// Symbol index meaning "no symbol".
pub const STN_UNDEF: u32 = 0;

// ---------------------------------------------------------------------------
// Program headers
// ---------------------------------------------------------------------------

/// Unused entry.
pub const PT_NULL: u32 = 0;
/// Loadable segment.
pub const PT_LOAD: u32 = 1;
/// Dynamic linking information.
pub const PT_DYNAMIC: u32 = 2;
/// Program interpreter path.
pub const PT_INTERP: u32 = 3;
/// Auxiliary information (notes).
pub const PT_NOTE: u32 = 4;
/// Reserved.
pub const PT_SHLIB: u32 = 5;
/// The program header table itself.
pub const PT_PHDR: u32 = 6;
/// Thread-local storage template.
pub const PT_TLS: u32 = 7;
/// `.eh_frame_hdr` location.
pub const PT_GNU_EH_FRAME: u32 = 0x6474_e550;
/// Stack executability.
pub const PT_GNU_STACK: u32 = 0x6474_e551;
/// Read-only after relocation.
pub const PT_GNU_RELRO: u32 = 0x6474_e552;
/// `.note.gnu.property` location.
pub const PT_GNU_PROPERTY: u32 = 0x6474_e553;
/// SFrame stack trace information.
pub const PT_GNU_SFRAME: u32 = 0x6474_e554;

/// Segment is executable.
pub const PF_X: u32 = 0x1;
/// Segment is writable.
pub const PF_W: u32 = 0x2;
/// Segment is readable.
pub const PF_R: u32 = 0x4;

// ---------------------------------------------------------------------------
// Dynamic tags
// ---------------------------------------------------------------------------

/// End of the dynamic array.
pub const DT_NULL: i64 = 0;
/// Name of a needed library.
pub const DT_NEEDED: i64 = 1;
/// Size of the PLT relocations.
pub const DT_PLTRELSZ: i64 = 2;
/// Address of the PLT GOT.
pub const DT_PLTGOT: i64 = 3;
/// Address of the System V hash table.
pub const DT_HASH: i64 = 4;
/// Address of the dynamic string table.
pub const DT_STRTAB: i64 = 5;
/// Address of the dynamic symbol table.
pub const DT_SYMTAB: i64 = 6;
/// Address of the `Rela` relocations.
pub const DT_RELA: i64 = 7;
/// Size of the `Rela` relocations.
pub const DT_RELASZ: i64 = 8;
/// Size of one `Rela` entry.
pub const DT_RELAENT: i64 = 9;
/// Size of the dynamic string table.
pub const DT_STRSZ: i64 = 10;
/// Size of one symbol table entry.
pub const DT_SYMENT: i64 = 11;
/// Address of the initialization function.
pub const DT_INIT: i64 = 12;
/// Address of the termination function.
pub const DT_FINI: i64 = 13;
/// Name of this shared object.
pub const DT_SONAME: i64 = 14;
/// Library search path (deprecated).
pub const DT_RPATH: i64 = 15;
/// Resolve symbols in this object first.
pub const DT_SYMBOLIC: i64 = 16;
/// Address of the `Rel` relocations.
pub const DT_REL: i64 = 17;
/// Size of the `Rel` relocations.
pub const DT_RELSZ: i64 = 18;
/// Size of one `Rel` entry.
pub const DT_RELENT: i64 = 19;
/// Type of the PLT relocations.
pub const DT_PLTREL: i64 = 20;
/// Debugger hook.
pub const DT_DEBUG: i64 = 21;
/// Relocations may modify a read-only segment.
pub const DT_TEXTREL: i64 = 22;
/// Address of the PLT relocations.
pub const DT_JMPREL: i64 = 23;
/// Process all relocations before transferring control.
pub const DT_BIND_NOW: i64 = 24;
/// Address of the initializer array.
pub const DT_INIT_ARRAY: i64 = 25;
/// Address of the finalizer array.
pub const DT_FINI_ARRAY: i64 = 26;
/// Size of the initializer array.
pub const DT_INIT_ARRAYSZ: i64 = 27;
/// Size of the finalizer array.
pub const DT_FINI_ARRAYSZ: i64 = 28;
/// Library search path.
pub const DT_RUNPATH: i64 = 29;
/// Flags (`DF_*`).
pub const DT_FLAGS: i64 = 30;
/// Address of the pre-initializer array.
pub const DT_PREINIT_ARRAY: i64 = 32;
/// Size of the pre-initializer array.
pub const DT_PREINIT_ARRAYSZ: i64 = 33;
/// Address of the extended section index table.
pub const DT_SYMTAB_SHNDX: i64 = 34;
/// Size of the RELR relocations.
pub const DT_RELRSZ: i64 = 35;
/// Address of the RELR relocations.
pub const DT_RELR: i64 = 36;
/// Size of one RELR entry.
pub const DT_RELRENT: i64 = 37;
/// Address of the GNU hash table.
pub const DT_GNU_HASH: i64 = 0x6fff_fef5;
/// Address of the symbol version table.
pub const DT_VERSYM: i64 = 0x6fff_fff0;
/// Number of relative `Rela` relocations.
pub const DT_RELACOUNT: i64 = 0x6fff_fff9;
/// Number of relative `Rel` relocations.
pub const DT_RELCOUNT: i64 = 0x6fff_fffa;
/// Flags (`DF_1_*`).
pub const DT_FLAGS_1: i64 = 0x6fff_fffb;
/// Address of the version definitions.
pub const DT_VERDEF: i64 = 0x6fff_fffc;
/// Number of version definitions.
pub const DT_VERDEFNUM: i64 = 0x6fff_fffd;
/// Address of the version requirements.
pub const DT_VERNEED: i64 = 0x6fff_fffe;
/// Number of version requirements.
pub const DT_VERNEEDNUM: i64 = 0x6fff_ffff;
/// Auxiliary filter library name.
pub const DT_AUXILIARY: i64 = 0x7fff_fffd;
/// Standard filter library name.
pub const DT_FILTER: i64 = 0x7fff_ffff;

/// `DT_FLAGS`: `$ORIGIN` processing required.
pub const DF_ORIGIN: u64 = 0x1;
/// `DT_FLAGS`: symbolic resolution.
pub const DF_SYMBOLIC: u64 = 0x2;
/// `DT_FLAGS`: text relocations present.
pub const DF_TEXTREL: u64 = 0x4;
/// `DT_FLAGS`: bind all symbols at load time.
pub const DF_BIND_NOW: u64 = 0x8;
/// `DT_FLAGS`: uses static TLS.
pub const DF_STATIC_TLS: u64 = 0x10;

/// `DT_FLAGS_1`: bind all symbols at load time.
pub const DF_1_NOW: u64 = 0x1;
/// `DT_FLAGS_1`: global symbol lookup.
pub const DF_1_GLOBAL: u64 = 0x2;
/// `DT_FLAGS_1`: group.
pub const DF_1_GROUP: u64 = 0x4;
/// `DT_FLAGS_1`: never unload.
pub const DF_1_NODELETE: u64 = 0x8;
/// `DT_FLAGS_1`: load filtees immediately.
pub const DF_1_LOADFLTR: u64 = 0x10;
/// `DT_FLAGS_1`: initialize first.
pub const DF_1_INITFIRST: u64 = 0x20;
/// `DT_FLAGS_1`: cannot be `dlopen`ed.
pub const DF_1_NOOPEN: u64 = 0x40;
/// `DT_FLAGS_1`: `$ORIGIN` processing required.
pub const DF_1_ORIGIN: u64 = 0x80;
/// `DT_FLAGS_1`: direct binding.
pub const DF_1_DIRECT: u64 = 0x100;
/// `DT_FLAGS_1`: interposer.
pub const DF_1_INTERPOSE: u64 = 0x400;
/// `DT_FLAGS_1`: ignore default library search paths.
pub const DF_1_NODEFLIB: u64 = 0x800;
/// `DT_FLAGS_1`: cannot be dumped.
pub const DF_1_NODUMP: u64 = 0x1000;
/// `DT_FLAGS_1`: configuration alternative.
pub const DF_1_CONFALT: u64 = 0x2000;
/// `DT_FLAGS_1`: filtee terminates the search.
pub const DF_1_ENDFILTEE: u64 = 0x4000;
/// `DT_FLAGS_1`: displacement relocations done.
pub const DF_1_DISPRELDNE: u64 = 0x8000;
/// `DT_FLAGS_1`: displacement relocations pending.
pub const DF_1_DISPRELPND: u64 = 0x1_0000;
/// `DT_FLAGS_1`: no direct binding.
pub const DF_1_NODIRECT: u64 = 0x2_0000;
/// `DT_FLAGS_1`: ignore multiple definitions.
pub const DF_1_IGNMULDEF: u64 = 0x4_0000;
/// `DT_FLAGS_1`: no kernel symbols.
pub const DF_1_NOKSYMS: u64 = 0x8_0000;
/// `DT_FLAGS_1`: no ELF header.
pub const DF_1_NOHDR: u64 = 0x10_0000;
/// `DT_FLAGS_1`: edited.
pub const DF_1_EDITED: u64 = 0x20_0000;
/// `DT_FLAGS_1`: no relocations.
pub const DF_1_NORELOC: u64 = 0x40_0000;
/// `DT_FLAGS_1`: symbol interposers.
pub const DF_1_SYMINTPOSE: u64 = 0x80_0000;
/// `DT_FLAGS_1`: global auditing.
pub const DF_1_GLOBAUDIT: u64 = 0x100_0000;
/// `DT_FLAGS_1`: singleton symbols.
pub const DF_1_SINGLETON: u64 = 0x200_0000;
/// `DT_FLAGS_1`: stub.
pub const DF_1_STUB: u64 = 0x400_0000;
/// `DT_FLAGS_1`: position-independent executable.
pub const DF_1_PIE: u64 = 0x800_0000;

// ---------------------------------------------------------------------------
// Symbol versioning
// ---------------------------------------------------------------------------

/// Current `vd_version`.
pub const VER_DEF_CURRENT: u16 = 1;
/// Current `vn_version`.
pub const VER_NEED_CURRENT: u16 = 1;
/// Version definition of the file itself.
pub const VER_FLG_BASE: u16 = 0x1;
/// Weak version reference.
pub const VER_FLG_WEAK: u16 = 0x2;
/// Informational version reference.
pub const VER_FLG_INFO: u16 = 0x4;
/// Version index of a local symbol.
pub const VER_NDX_LOCAL: u16 = 0;
/// Version index of an unversioned global symbol.
pub const VER_NDX_GLOBAL: u16 = 1;
/// `versym` bit marking a hidden (non-default) version.
pub const VERSYM_HIDDEN: u16 = 0x8000;
/// `versym` mask selecting the version index.
pub const VERSYM_VERSION: u16 = 0x7fff;

// ---------------------------------------------------------------------------
// Notes
// ---------------------------------------------------------------------------

/// Owner name of GNU notes.
pub const ELF_NOTE_GNU: &[u8] = b"GNU";
/// `.note.ABI-tag`.
pub const NT_GNU_ABI_TAG: u32 = 1;
/// Hardware capabilities.
pub const NT_GNU_HWCAP: u32 = 2;
/// Build ID.
pub const NT_GNU_BUILD_ID: u32 = 3;
/// gold version.
pub const NT_GNU_GOLD_VERSION: u32 = 4;
/// Program properties (`.note.gnu.property`).
pub const NT_GNU_PROPERTY_TYPE_0: u32 = 5;
/// FDO packaging metadata.
pub const NT_FDO_PACKAGING_METADATA: u32 = 0xcafe_1a7e;

// ---------------------------------------------------------------------------
// GNU properties
// ---------------------------------------------------------------------------

/// Stack size property.
pub const GNU_PROPERTY_STACK_SIZE: u32 = 1;
/// No copy relocations on protected data symbols.
pub const GNU_PROPERTY_NO_COPY_ON_PROTECTED: u32 = 2;
/// Start of the generic "AND" `u32` property range.
pub const GNU_PROPERTY_UINT32_AND_LO: u32 = 0xb000_0000;
/// End of the generic "AND" `u32` property range.
pub const GNU_PROPERTY_UINT32_AND_HI: u32 = 0xb000_7fff;
/// Start of the generic "OR" `u32` property range.
pub const GNU_PROPERTY_UINT32_OR_LO: u32 = 0xb000_8000;
/// End of the generic "OR" `u32` property range.
pub const GNU_PROPERTY_UINT32_OR_HI: u32 = 0xb000_ffff;
/// Generic "needed" feature bits.
pub const GNU_PROPERTY_1_NEEDED: u32 = GNU_PROPERTY_UINT32_OR_LO;
/// `GNU_PROPERTY_1_NEEDED`: indirect external access.
pub const GNU_PROPERTY_1_NEEDED_INDIRECT_EXTERN_ACCESS: u32 = 1;
/// Start of the processor-specific property range.
pub const GNU_PROPERTY_LOPROC: u32 = 0xc000_0000;
/// End of the processor-specific property range.
pub const GNU_PROPERTY_HIPROC: u32 = 0xdfff_ffff;
/// Start of the application-specific property range.
pub const GNU_PROPERTY_LOUSER: u32 = 0xe000_0000;
/// End of the application-specific property range.
pub const GNU_PROPERTY_HIUSER: u32 = 0xffff_ffff;

/// AArch64 feature bits, combined with AND.
pub const GNU_PROPERTY_AARCH64_FEATURE_1_AND: u32 = 0xc000_0000;
/// AArch64: branch target identification.
pub const GNU_PROPERTY_AARCH64_FEATURE_1_BTI: u32 = 1 << 0;
/// AArch64: pointer authentication.
pub const GNU_PROPERTY_AARCH64_FEATURE_1_PAC: u32 = 1 << 1;
/// AArch64: guarded control stack.
pub const GNU_PROPERTY_AARCH64_FEATURE_1_GCS: u32 = 1 << 2;

/// RISC-V feature bits, combined with AND.
pub const GNU_PROPERTY_RISCV_FEATURE_1_AND: u32 = 0xc000_0000;
/// RISC-V: unlabeled landing pads (Zicfilp).
pub const GNU_PROPERTY_RISCV_FEATURE_1_CFI_LP_UNLABELED: u32 = 1 << 0;
/// RISC-V: shadow stack (Zicfiss).
pub const GNU_PROPERTY_RISCV_FEATURE_1_CFI_SS: u32 = 1 << 1;

/// x86: start of the "AND" `u32` property range.
pub const GNU_PROPERTY_X86_UINT32_AND_LO: u32 = 0xc000_0002;
/// x86: end of the "AND" `u32` property range.
pub const GNU_PROPERTY_X86_UINT32_AND_HI: u32 = 0xc000_7fff;
/// x86: start of the "OR" `u32` property range.
pub const GNU_PROPERTY_X86_UINT32_OR_LO: u32 = 0xc000_8000;
/// x86: end of the "OR" `u32` property range.
pub const GNU_PROPERTY_X86_UINT32_OR_HI: u32 = 0xc000_ffff;
/// x86: start of the "OR and AND" `u32` property range.
pub const GNU_PROPERTY_X86_UINT32_OR_AND_LO: u32 = 0xc001_0000;
/// x86: end of the "OR and AND" `u32` property range.
pub const GNU_PROPERTY_X86_UINT32_OR_AND_HI: u32 = 0xc001_7fff;
/// x86 feature bits (IBT, SHSTK, LAM), combined with AND.
pub const GNU_PROPERTY_X86_FEATURE_1_AND: u32 = 0xc000_0002;
/// x86 features needed (`X86_FEATURE_2_*` bits).
pub const GNU_PROPERTY_X86_FEATURE_2_NEEDED: u32 = 0xc000_8001;
/// x86 ISA level needed (`X86_ISA_1_*` bits).
pub const GNU_PROPERTY_X86_ISA_1_NEEDED: u32 = 0xc000_8002;
/// x86 features used (`X86_FEATURE_2_*` bits).
pub const GNU_PROPERTY_X86_FEATURE_2_USED: u32 = 0xc001_0001;
/// x86 ISA level used (`X86_ISA_1_*` bits).
pub const GNU_PROPERTY_X86_ISA_1_USED: u32 = 0xc001_0002;
/// x86: indirect branch tracking.
pub const GNU_PROPERTY_X86_FEATURE_1_IBT: u32 = 1 << 0;
/// x86: shadow stack.
pub const GNU_PROPERTY_X86_FEATURE_1_SHSTK: u32 = 1 << 1;
/// x86: linear address masking, 48-bit user space.
pub const GNU_PROPERTY_X86_FEATURE_1_LAM_U48: u32 = 1 << 2;
/// x86: linear address masking, 57-bit user space.
pub const GNU_PROPERTY_X86_FEATURE_1_LAM_U57: u32 = 1 << 3;
/// x86-64 baseline ISA level.
pub const GNU_PROPERTY_X86_ISA_1_BASELINE: u32 = 1 << 0;
/// x86-64-v2 ISA level.
pub const GNU_PROPERTY_X86_ISA_1_V2: u32 = 1 << 1;
/// x86-64-v3 ISA level.
pub const GNU_PROPERTY_X86_ISA_1_V3: u32 = 1 << 2;
/// x86-64-v4 ISA level.
pub const GNU_PROPERTY_X86_ISA_1_V4: u32 = 1 << 3;
/// x86 feature: x86 instructions.
pub const GNU_PROPERTY_X86_FEATURE_2_X86: u32 = 1 << 0;
/// x86 feature: x87.
pub const GNU_PROPERTY_X86_FEATURE_2_X87: u32 = 1 << 1;
/// x86 feature: MMX.
pub const GNU_PROPERTY_X86_FEATURE_2_MMX: u32 = 1 << 2;
/// x86 feature: XMM registers.
pub const GNU_PROPERTY_X86_FEATURE_2_XMM: u32 = 1 << 3;
/// x86 feature: YMM registers.
pub const GNU_PROPERTY_X86_FEATURE_2_YMM: u32 = 1 << 4;
/// x86 feature: ZMM registers.
pub const GNU_PROPERTY_X86_FEATURE_2_ZMM: u32 = 1 << 5;
/// x86 feature: FXSR.
pub const GNU_PROPERTY_X86_FEATURE_2_FXSR: u32 = 1 << 6;
/// x86 feature: XSAVE.
pub const GNU_PROPERTY_X86_FEATURE_2_XSAVE: u32 = 1 << 7;
/// x86 feature: XSAVEOPT.
pub const GNU_PROPERTY_X86_FEATURE_2_XSAVEOPT: u32 = 1 << 8;
/// x86 feature: XSAVEC.
pub const GNU_PROPERTY_X86_FEATURE_2_XSAVEC: u32 = 1 << 9;
/// x86 feature: TMM registers.
pub const GNU_PROPERTY_X86_FEATURE_2_TMM: u32 = 1 << 10;
/// x86 feature: mask registers.
pub const GNU_PROPERTY_X86_FEATURE_2_MASK: u32 = 1 << 11;

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)]
mod tests {
    use super::*;

    #[test]
    fn reloc_names() {
        assert_eq!(reloc_name(EM_X86_64, 4), Some("R_X86_64_PLT32"));
        assert_eq!(
            reloc_name(EM_X86_64, x86_64::R_X86_64_CODE_6_GOTPC32_TLSDESC),
            Some("R_X86_64_CODE_6_GOTPC32_TLSDESC")
        );
        assert_eq!(reloc_name(EM_AARCH64, 0x11b), Some("R_AARCH64_CALL26"));
        assert_eq!(reloc_name(EM_RISCV, 19), Some("R_RISCV_CALL_PLT"));
        assert_eq!(reloc_name(EM_386, 43), Some("R_386_GOT32X"));
        assert_eq!(reloc_name(EM_X86_64, 9999), None);
        assert_eq!(reloc_name(EM_MIPS, 1), None);
    }
}
