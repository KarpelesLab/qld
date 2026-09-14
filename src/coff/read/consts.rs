//! PE/COFF ABI constants, with the names used by the Windows SDK headers
//! (`winnt.h`) and LLVM's `BinaryFormat/COFF.h`.

use crate::target::Architecture;

// ---------------------------------------------------------------------------
// Machines
// ---------------------------------------------------------------------------

/// Unknown machine; also `Sig1` of import and anonymous object headers.
pub const IMAGE_FILE_MACHINE_UNKNOWN: u16 = 0;
/// Intel 386 and compatibles.
pub const IMAGE_FILE_MACHINE_I386: u16 = 0x014c;
/// ARM little-endian (classic).
pub const IMAGE_FILE_MACHINE_ARM: u16 = 0x01c0;
/// ARM Thumb.
pub const IMAGE_FILE_MACHINE_THUMB: u16 = 0x01c2;
/// ARM Thumb-2 little-endian (ARMNT, Windows on ARM 32-bit).
pub const IMAGE_FILE_MACHINE_ARMNT: u16 = 0x01c4;
/// Intel Itanium.
pub const IMAGE_FILE_MACHINE_IA64: u16 = 0x0200;
/// RISC-V 32-bit.
pub const IMAGE_FILE_MACHINE_RISCV32: u16 = 0x5032;
/// RISC-V 64-bit.
pub const IMAGE_FILE_MACHINE_RISCV64: u16 = 0x5064;
/// LoongArch 64-bit.
pub const IMAGE_FILE_MACHINE_LOONGARCH64: u16 = 0x6264;
/// x86-64.
pub const IMAGE_FILE_MACHINE_AMD64: u16 = 0x8664;
/// ARM64 emulation-compatible (ARM64EC) code.
pub const IMAGE_FILE_MACHINE_ARM64EC: u16 = 0xa641;
/// ARM64X: a hybrid image with both ARM64 and ARM64EC code.
pub const IMAGE_FILE_MACHINE_ARM64X: u16 = 0xa64e;
/// ARM64 little-endian.
pub const IMAGE_FILE_MACHINE_ARM64: u16 = 0xaa64;

/// The architecture a COFF `Machine` value denotes, if qld knows it.
///
/// ARM64EC and ARM64X map to [`Architecture::Aarch64`]; ARM, Thumb and ARMNT
/// map to [`Architecture::Arm`].
#[must_use]
pub fn machine_architecture(machine: u16) -> Option<Architecture> {
    Some(match machine {
        IMAGE_FILE_MACHINE_I386 => Architecture::X86,
        IMAGE_FILE_MACHINE_AMD64 => Architecture::X86_64,
        IMAGE_FILE_MACHINE_ARM64 | IMAGE_FILE_MACHINE_ARM64EC | IMAGE_FILE_MACHINE_ARM64X => {
            Architecture::Aarch64
        }
        IMAGE_FILE_MACHINE_ARM | IMAGE_FILE_MACHINE_THUMB | IMAGE_FILE_MACHINE_ARMNT => {
            Architecture::Arm
        }
        IMAGE_FILE_MACHINE_RISCV32 => Architecture::Riscv32,
        IMAGE_FILE_MACHINE_RISCV64 => Architecture::Riscv64,
        IMAGE_FILE_MACHINE_LOONGARCH64 => Architecture::LoongArch64,
        _ => return None,
    })
}

/// The machine name as `llvm-readobj` prints it (`IMAGE_FILE_MACHINE_AMD64`).
#[must_use]
pub fn machine_name(machine: u16) -> Option<&'static str> {
    Some(match machine {
        IMAGE_FILE_MACHINE_UNKNOWN => "IMAGE_FILE_MACHINE_UNKNOWN",
        IMAGE_FILE_MACHINE_I386 => "IMAGE_FILE_MACHINE_I386",
        IMAGE_FILE_MACHINE_ARM => "IMAGE_FILE_MACHINE_ARM",
        IMAGE_FILE_MACHINE_THUMB => "IMAGE_FILE_MACHINE_THUMB",
        IMAGE_FILE_MACHINE_ARMNT => "IMAGE_FILE_MACHINE_ARMNT",
        IMAGE_FILE_MACHINE_IA64 => "IMAGE_FILE_MACHINE_IA64",
        IMAGE_FILE_MACHINE_RISCV32 => "IMAGE_FILE_MACHINE_RISCV32",
        IMAGE_FILE_MACHINE_RISCV64 => "IMAGE_FILE_MACHINE_RISCV64",
        IMAGE_FILE_MACHINE_LOONGARCH64 => "IMAGE_FILE_MACHINE_LOONGARCH64",
        IMAGE_FILE_MACHINE_AMD64 => "IMAGE_FILE_MACHINE_AMD64",
        IMAGE_FILE_MACHINE_ARM64EC => "IMAGE_FILE_MACHINE_ARM64EC",
        IMAGE_FILE_MACHINE_ARM64X => "IMAGE_FILE_MACHINE_ARM64X",
        IMAGE_FILE_MACHINE_ARM64 => "IMAGE_FILE_MACHINE_ARM64",
        _ => return None,
    })
}

/// Whether the machine is one of the ARM64 variants (ARM64, ARM64EC,
/// ARM64X), which share relocation types.
#[must_use]
pub fn is_arm64(machine: u16) -> bool {
    matches!(
        machine,
        IMAGE_FILE_MACHINE_ARM64 | IMAGE_FILE_MACHINE_ARM64EC | IMAGE_FILE_MACHINE_ARM64X
    )
}

// ---------------------------------------------------------------------------
// File header characteristics (`IMAGE_FILE_*`)
// ---------------------------------------------------------------------------

/// Image only; no base relocations.
pub const IMAGE_FILE_RELOCS_STRIPPED: u16 = 0x0001;
/// The image is valid and can be run.
pub const IMAGE_FILE_EXECUTABLE_IMAGE: u16 = 0x0002;
/// COFF line numbers removed (deprecated).
pub const IMAGE_FILE_LINE_NUMS_STRIPPED: u16 = 0x0004;
/// COFF local symbols removed (deprecated).
pub const IMAGE_FILE_LOCAL_SYMS_STRIPPED: u16 = 0x0008;
/// Aggressively trim the working set (obsolete).
pub const IMAGE_FILE_AGGRESSIVE_WS_TRIM: u16 = 0x0010;
/// The application can handle addresses above 2 GiB.
pub const IMAGE_FILE_LARGE_ADDRESS_AWARE: u16 = 0x0020;
/// Big-endian bytes (obsolete).
pub const IMAGE_FILE_BYTES_REVERSED_LO: u16 = 0x0080;
/// A 32-bit-word machine.
pub const IMAGE_FILE_32BIT_MACHINE: u16 = 0x0100;
/// Debugging information removed.
pub const IMAGE_FILE_DEBUG_STRIPPED: u16 = 0x0200;
/// Copy to swap if run from removable media.
pub const IMAGE_FILE_REMOVABLE_RUN_FROM_SWAP: u16 = 0x0400;
/// Copy to swap if run from the network.
pub const IMAGE_FILE_NET_RUN_FROM_SWAP: u16 = 0x0800;
/// A system file.
pub const IMAGE_FILE_SYSTEM: u16 = 0x1000;
/// A dynamic-link library.
pub const IMAGE_FILE_DLL: u16 = 0x2000;
/// Run only on a uniprocessor machine.
pub const IMAGE_FILE_UP_SYSTEM_ONLY: u16 = 0x4000;
/// Big-endian (obsolete).
pub const IMAGE_FILE_BYTES_REVERSED_HI: u16 = 0x8000;

// ---------------------------------------------------------------------------
// Section characteristics (`IMAGE_SCN_*`)
// ---------------------------------------------------------------------------

/// Do not pad the section to the next boundary (obsolete).
pub const IMAGE_SCN_TYPE_NO_PAD: u32 = 0x0000_0008;
/// Executable code.
pub const IMAGE_SCN_CNT_CODE: u32 = 0x0000_0020;
/// Initialized data.
pub const IMAGE_SCN_CNT_INITIALIZED_DATA: u32 = 0x0000_0040;
/// Uninitialized data (no file contents).
pub const IMAGE_SCN_CNT_UNINITIALIZED_DATA: u32 = 0x0000_0080;
/// Reserved.
pub const IMAGE_SCN_LNK_OTHER: u32 = 0x0000_0100;
/// Comments or other information for the linker (`.drectve`); objects only.
pub const IMAGE_SCN_LNK_INFO: u32 = 0x0000_0200;
/// Not part of the image; objects only.
pub const IMAGE_SCN_LNK_REMOVE: u32 = 0x0000_0800;
/// COMDAT data; objects only.
pub const IMAGE_SCN_LNK_COMDAT: u32 = 0x0000_1000;
/// Data referenced through the global pointer.
pub const IMAGE_SCN_GPREL: u32 = 0x0000_8000;
/// Reserved.
pub const IMAGE_SCN_MEM_PURGEABLE: u32 = 0x0002_0000;
/// Reserved (same value as `IMAGE_SCN_MEM_PURGEABLE`).
pub const IMAGE_SCN_MEM_16BIT: u32 = 0x0002_0000;
/// Reserved.
pub const IMAGE_SCN_MEM_LOCKED: u32 = 0x0004_0000;
/// Reserved.
pub const IMAGE_SCN_MEM_PRELOAD: u32 = 0x0008_0000;
/// Mask of the `IMAGE_SCN_ALIGN_*` field.
pub const IMAGE_SCN_ALIGN_MASK: u32 = 0x00f0_0000;
/// Align to 1 byte.
pub const IMAGE_SCN_ALIGN_1BYTES: u32 = 0x0010_0000;
/// Align to 2 bytes.
pub const IMAGE_SCN_ALIGN_2BYTES: u32 = 0x0020_0000;
/// Align to 4 bytes.
pub const IMAGE_SCN_ALIGN_4BYTES: u32 = 0x0030_0000;
/// Align to 8 bytes.
pub const IMAGE_SCN_ALIGN_8BYTES: u32 = 0x0040_0000;
/// Align to 16 bytes.
pub const IMAGE_SCN_ALIGN_16BYTES: u32 = 0x0050_0000;
/// Align to 32 bytes.
pub const IMAGE_SCN_ALIGN_32BYTES: u32 = 0x0060_0000;
/// Align to 64 bytes.
pub const IMAGE_SCN_ALIGN_64BYTES: u32 = 0x0070_0000;
/// Align to 128 bytes.
pub const IMAGE_SCN_ALIGN_128BYTES: u32 = 0x0080_0000;
/// Align to 256 bytes.
pub const IMAGE_SCN_ALIGN_256BYTES: u32 = 0x0090_0000;
/// Align to 512 bytes.
pub const IMAGE_SCN_ALIGN_512BYTES: u32 = 0x00a0_0000;
/// Align to 1024 bytes.
pub const IMAGE_SCN_ALIGN_1024BYTES: u32 = 0x00b0_0000;
/// Align to 2048 bytes.
pub const IMAGE_SCN_ALIGN_2048BYTES: u32 = 0x00c0_0000;
/// Align to 4096 bytes.
pub const IMAGE_SCN_ALIGN_4096BYTES: u32 = 0x00d0_0000;
/// Align to 8192 bytes.
pub const IMAGE_SCN_ALIGN_8192BYTES: u32 = 0x00e0_0000;
/// The relocation count overflowed 16 bits: the real count is in the
/// `VirtualAddress` of the first relocation.
pub const IMAGE_SCN_LNK_NRELOC_OVFL: u32 = 0x0100_0000;
/// Can be discarded.
pub const IMAGE_SCN_MEM_DISCARDABLE: u32 = 0x0200_0000;
/// Shareable.
pub const IMAGE_SCN_MEM_SHARED: u32 = 0x1000_0000;
/// Executable.
pub const IMAGE_SCN_MEM_EXECUTE: u32 = 0x2000_0000;
/// Readable.
pub const IMAGE_SCN_MEM_READ: u32 = 0x4000_0000;
/// Writable.
pub const IMAGE_SCN_MEM_WRITE: u32 = 0x8000_0000;

/// Every named `IMAGE_SCN_*` flag bit, for printing (the alignment field is
/// printed separately).
pub const SECTION_FLAG_NAMES: &[(u32, &str)] = &[
    (IMAGE_SCN_TYPE_NO_PAD, "IMAGE_SCN_TYPE_NO_PAD"),
    (IMAGE_SCN_CNT_CODE, "IMAGE_SCN_CNT_CODE"),
    (
        IMAGE_SCN_CNT_INITIALIZED_DATA,
        "IMAGE_SCN_CNT_INITIALIZED_DATA",
    ),
    (
        IMAGE_SCN_CNT_UNINITIALIZED_DATA,
        "IMAGE_SCN_CNT_UNINITIALIZED_DATA",
    ),
    (IMAGE_SCN_LNK_OTHER, "IMAGE_SCN_LNK_OTHER"),
    (IMAGE_SCN_LNK_INFO, "IMAGE_SCN_LNK_INFO"),
    (IMAGE_SCN_LNK_REMOVE, "IMAGE_SCN_LNK_REMOVE"),
    (IMAGE_SCN_LNK_COMDAT, "IMAGE_SCN_LNK_COMDAT"),
    (IMAGE_SCN_GPREL, "IMAGE_SCN_GPREL"),
    (IMAGE_SCN_MEM_PURGEABLE, "IMAGE_SCN_MEM_PURGEABLE"),
    (IMAGE_SCN_MEM_LOCKED, "IMAGE_SCN_MEM_LOCKED"),
    (IMAGE_SCN_MEM_PRELOAD, "IMAGE_SCN_MEM_PRELOAD"),
    (IMAGE_SCN_LNK_NRELOC_OVFL, "IMAGE_SCN_LNK_NRELOC_OVFL"),
    (IMAGE_SCN_MEM_DISCARDABLE, "IMAGE_SCN_MEM_DISCARDABLE"),
    (IMAGE_SCN_MEM_SHARED, "IMAGE_SCN_MEM_SHARED"),
    (IMAGE_SCN_MEM_EXECUTE, "IMAGE_SCN_MEM_EXECUTE"),
    (IMAGE_SCN_MEM_READ, "IMAGE_SCN_MEM_READ"),
    (IMAGE_SCN_MEM_WRITE, "IMAGE_SCN_MEM_WRITE"),
];

// ---------------------------------------------------------------------------
// Symbols
// ---------------------------------------------------------------------------

/// Section number of an undefined (or common) symbol.
pub const IMAGE_SYM_UNDEFINED: i32 = 0;
/// Section number of an absolute symbol.
pub const IMAGE_SYM_ABSOLUTE: i32 = -1;
/// Section number of a debugging symbol.
pub const IMAGE_SYM_DEBUG: i32 = -2;
/// Largest section number a regular (16-bit) symbol record can hold; values
/// above it are the negative reserved numbers.
pub const MAX_NUMBER_OF_SECTIONS_16: u16 = 0xfeff;

/// Base type: none.
pub const IMAGE_SYM_TYPE_NULL: u16 = 0;
/// Complex type: none.
pub const IMAGE_SYM_DTYPE_NULL: u16 = 0;
/// Complex type: pointer.
pub const IMAGE_SYM_DTYPE_POINTER: u16 = 1;
/// Complex type: function returning the base type.
pub const IMAGE_SYM_DTYPE_FUNCTION: u16 = 2;
/// Complex type: array.
pub const IMAGE_SYM_DTYPE_ARRAY: u16 = 3;

/// End of function (debugging).
pub const IMAGE_SYM_CLASS_END_OF_FUNCTION: u8 = 0xff;
/// No storage class.
pub const IMAGE_SYM_CLASS_NULL: u8 = 0;
/// Automatic (stack) variable.
pub const IMAGE_SYM_CLASS_AUTOMATIC: u8 = 1;
/// External symbol: defined, undefined or common depending on the section
/// number and value.
pub const IMAGE_SYM_CLASS_EXTERNAL: u8 = 2;
/// Static symbol: a local definition, or a section symbol when the value is
/// 0 and a section definition aux record follows.
pub const IMAGE_SYM_CLASS_STATIC: u8 = 3;
/// Register variable.
pub const IMAGE_SYM_CLASS_REGISTER: u8 = 4;
/// Externally defined symbol.
pub const IMAGE_SYM_CLASS_EXTERNAL_DEF: u8 = 5;
/// Code label defined in the module.
pub const IMAGE_SYM_CLASS_LABEL: u8 = 6;
/// Reference to an undefined code label.
pub const IMAGE_SYM_CLASS_UNDEFINED_LABEL: u8 = 7;
/// Structure member.
pub const IMAGE_SYM_CLASS_MEMBER_OF_STRUCT: u8 = 8;
/// Formal function argument.
pub const IMAGE_SYM_CLASS_ARGUMENT: u8 = 9;
/// Structure tag.
pub const IMAGE_SYM_CLASS_STRUCT_TAG: u8 = 10;
/// Union member.
pub const IMAGE_SYM_CLASS_MEMBER_OF_UNION: u8 = 11;
/// Union tag.
pub const IMAGE_SYM_CLASS_UNION_TAG: u8 = 12;
/// Typedef.
pub const IMAGE_SYM_CLASS_TYPE_DEFINITION: u8 = 13;
/// Static data declaration.
pub const IMAGE_SYM_CLASS_UNDEFINED_STATIC: u8 = 14;
/// Enumeration tag.
pub const IMAGE_SYM_CLASS_ENUM_TAG: u8 = 15;
/// Enumeration member.
pub const IMAGE_SYM_CLASS_MEMBER_OF_ENUM: u8 = 16;
/// Register parameter.
pub const IMAGE_SYM_CLASS_REGISTER_PARAM: u8 = 17;
/// Bit-field reference.
pub const IMAGE_SYM_CLASS_BIT_FIELD: u8 = 18;
/// `.bb` / `.eb` block markers.
pub const IMAGE_SYM_CLASS_BLOCK: u8 = 100;
/// `.bf` / `.ef` / `.lf` function markers.
pub const IMAGE_SYM_CLASS_FUNCTION: u8 = 101;
/// End of structure.
pub const IMAGE_SYM_CLASS_END_OF_STRUCT: u8 = 102;
/// Source file name; the name follows in aux records.
pub const IMAGE_SYM_CLASS_FILE: u8 = 103;
/// Section definition (Microsoft tools use `STATIC` instead).
pub const IMAGE_SYM_CLASS_SECTION: u8 = 104;
/// Weak external; a weak external aux record follows.
pub const IMAGE_SYM_CLASS_WEAK_EXTERNAL: u8 = 105;
/// CLR token.
pub const IMAGE_SYM_CLASS_CLR_TOKEN: u8 = 107;

/// The storage class name as `llvm-readobj` prints it (`External`).
#[must_use]
pub fn storage_class_name(class: u8) -> Option<&'static str> {
    Some(match class {
        IMAGE_SYM_CLASS_END_OF_FUNCTION => "EndOfFunction",
        IMAGE_SYM_CLASS_NULL => "Null",
        IMAGE_SYM_CLASS_AUTOMATIC => "Automatic",
        IMAGE_SYM_CLASS_EXTERNAL => "External",
        IMAGE_SYM_CLASS_STATIC => "Static",
        IMAGE_SYM_CLASS_REGISTER => "Register",
        IMAGE_SYM_CLASS_EXTERNAL_DEF => "ExternalDef",
        IMAGE_SYM_CLASS_LABEL => "Label",
        IMAGE_SYM_CLASS_UNDEFINED_LABEL => "UndefinedLabel",
        IMAGE_SYM_CLASS_MEMBER_OF_STRUCT => "MemberOfStruct",
        IMAGE_SYM_CLASS_ARGUMENT => "Argument",
        IMAGE_SYM_CLASS_STRUCT_TAG => "StructTag",
        IMAGE_SYM_CLASS_MEMBER_OF_UNION => "MemberOfUnion",
        IMAGE_SYM_CLASS_UNION_TAG => "UnionTag",
        IMAGE_SYM_CLASS_TYPE_DEFINITION => "TypeDefinition",
        IMAGE_SYM_CLASS_UNDEFINED_STATIC => "UndefinedStatic",
        IMAGE_SYM_CLASS_ENUM_TAG => "EnumTag",
        IMAGE_SYM_CLASS_MEMBER_OF_ENUM => "MemberOfEnum",
        IMAGE_SYM_CLASS_REGISTER_PARAM => "RegisterParam",
        IMAGE_SYM_CLASS_BIT_FIELD => "BitField",
        IMAGE_SYM_CLASS_BLOCK => "Block",
        IMAGE_SYM_CLASS_FUNCTION => "Function",
        IMAGE_SYM_CLASS_END_OF_STRUCT => "EndOfStruct",
        IMAGE_SYM_CLASS_FILE => "File",
        IMAGE_SYM_CLASS_SECTION => "Section",
        IMAGE_SYM_CLASS_WEAK_EXTERNAL => "WeakExternal",
        IMAGE_SYM_CLASS_CLR_TOKEN => "CLRToken",
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// COMDAT selection and weak externals
// ---------------------------------------------------------------------------

/// Duplicate definitions are an error.
pub const IMAGE_COMDAT_SELECT_NODUPLICATES: u8 = 1;
/// Any copy may be picked.
pub const IMAGE_COMDAT_SELECT_ANY: u8 = 2;
/// Copies must have the same size.
pub const IMAGE_COMDAT_SELECT_SAME_SIZE: u8 = 3;
/// Copies must be identical (checksum).
pub const IMAGE_COMDAT_SELECT_EXACT_MATCH: u8 = 4;
/// The section follows another section's selection (associative).
pub const IMAGE_COMDAT_SELECT_ASSOCIATIVE: u8 = 5;
/// The largest copy is picked.
pub const IMAGE_COMDAT_SELECT_LARGEST: u8 = 6;
/// The newest copy is picked (unused by modern tools).
pub const IMAGE_COMDAT_SELECT_NEWEST: u8 = 7;

/// Weak external: do not search libraries for the symbol.
pub const IMAGE_WEAK_EXTERN_SEARCH_NOLIBRARY: u32 = 1;
/// Weak external: search libraries for the symbol.
pub const IMAGE_WEAK_EXTERN_SEARCH_LIBRARY: u32 = 2;
/// Weak external: the symbol is an alias for the tag symbol.
pub const IMAGE_WEAK_EXTERN_SEARCH_ALIAS: u32 = 3;
/// Weak external: an anti-dependency alias (ARM64EC).
pub const IMAGE_WEAK_EXTERN_ANTI_DEPENDENCY: u32 = 4;

// ---------------------------------------------------------------------------
// `@feat.00` flags
// ---------------------------------------------------------------------------

/// The object is compatible with SafeSEH (i386 `/SAFESEH`).
pub const FEAT00_SAFESEH: u32 = 0x1;
/// The object was compiled with `/GS` (security cookies).
pub const FEAT00_GUARD_STACK: u32 = 0x100;
/// The object was compiled with `/sdl`.
pub const FEAT00_SDL: u32 = 0x200;
/// The object was compiled with Control Flow Guard (`/guard:cf`).
pub const FEAT00_GUARD_CF: u32 = 0x800;
/// The object was compiled with EH continuation metadata (`/guard:ehcont`).
pub const FEAT00_GUARD_EHCONT: u32 = 0x4000;
/// The object was compiled for kernel mode (`/kernel`).
pub const FEAT00_KERNEL: u32 = 0x4000_0000;

// ---------------------------------------------------------------------------
// Relocations
// ---------------------------------------------------------------------------

/// x86-64 (`IMAGE_REL_AMD64_*`) relocation types.
pub mod amd64 {
    /// Ignored.
    pub const IMAGE_REL_AMD64_ABSOLUTE: u16 = 0x0000;
    /// 64-bit virtual address.
    pub const IMAGE_REL_AMD64_ADDR64: u16 = 0x0001;
    /// 32-bit virtual address.
    pub const IMAGE_REL_AMD64_ADDR32: u16 = 0x0002;
    /// 32-bit address without the image base (RVA).
    pub const IMAGE_REL_AMD64_ADDR32NB: u16 = 0x0003;
    /// 32-bit PC-relative, from the byte after the field.
    pub const IMAGE_REL_AMD64_REL32: u16 = 0x0004;
    /// 32-bit PC-relative, 1 byte past the field.
    pub const IMAGE_REL_AMD64_REL32_1: u16 = 0x0005;
    /// 32-bit PC-relative, 2 bytes past the field.
    pub const IMAGE_REL_AMD64_REL32_2: u16 = 0x0006;
    /// 32-bit PC-relative, 3 bytes past the field.
    pub const IMAGE_REL_AMD64_REL32_3: u16 = 0x0007;
    /// 32-bit PC-relative, 4 bytes past the field.
    pub const IMAGE_REL_AMD64_REL32_4: u16 = 0x0008;
    /// 32-bit PC-relative, 5 bytes past the field.
    pub const IMAGE_REL_AMD64_REL32_5: u16 = 0x0009;
    /// 16-bit section index.
    pub const IMAGE_REL_AMD64_SECTION: u16 = 0x000a;
    /// 32-bit offset from the start of the target's section.
    pub const IMAGE_REL_AMD64_SECREL: u16 = 0x000b;
    /// 7-bit section offset.
    pub const IMAGE_REL_AMD64_SECREL7: u16 = 0x000c;
    /// CLR token.
    pub const IMAGE_REL_AMD64_TOKEN: u16 = 0x000d;
    /// 32-bit span-dependent value.
    pub const IMAGE_REL_AMD64_SREL32: u16 = 0x000e;
    /// Pair (follows a span-dependent relocation).
    pub const IMAGE_REL_AMD64_PAIR: u16 = 0x000f;
    /// 32-bit span-dependent value applied at link time.
    pub const IMAGE_REL_AMD64_SSPAN32: u16 = 0x0010;

    /// The relocation type name, as `llvm-readobj` prints it.
    #[must_use]
    pub fn name(r_type: u16) -> Option<&'static str> {
        Some(match r_type {
            IMAGE_REL_AMD64_ABSOLUTE => "IMAGE_REL_AMD64_ABSOLUTE",
            IMAGE_REL_AMD64_ADDR64 => "IMAGE_REL_AMD64_ADDR64",
            IMAGE_REL_AMD64_ADDR32 => "IMAGE_REL_AMD64_ADDR32",
            IMAGE_REL_AMD64_ADDR32NB => "IMAGE_REL_AMD64_ADDR32NB",
            IMAGE_REL_AMD64_REL32 => "IMAGE_REL_AMD64_REL32",
            IMAGE_REL_AMD64_REL32_1 => "IMAGE_REL_AMD64_REL32_1",
            IMAGE_REL_AMD64_REL32_2 => "IMAGE_REL_AMD64_REL32_2",
            IMAGE_REL_AMD64_REL32_3 => "IMAGE_REL_AMD64_REL32_3",
            IMAGE_REL_AMD64_REL32_4 => "IMAGE_REL_AMD64_REL32_4",
            IMAGE_REL_AMD64_REL32_5 => "IMAGE_REL_AMD64_REL32_5",
            IMAGE_REL_AMD64_SECTION => "IMAGE_REL_AMD64_SECTION",
            IMAGE_REL_AMD64_SECREL => "IMAGE_REL_AMD64_SECREL",
            IMAGE_REL_AMD64_SECREL7 => "IMAGE_REL_AMD64_SECREL7",
            IMAGE_REL_AMD64_TOKEN => "IMAGE_REL_AMD64_TOKEN",
            IMAGE_REL_AMD64_SREL32 => "IMAGE_REL_AMD64_SREL32",
            IMAGE_REL_AMD64_PAIR => "IMAGE_REL_AMD64_PAIR",
            IMAGE_REL_AMD64_SSPAN32 => "IMAGE_REL_AMD64_SSPAN32",
            _ => return None,
        })
    }
}

/// i386 (`IMAGE_REL_I386_*`) relocation types.
pub mod i386 {
    /// Ignored.
    pub const IMAGE_REL_I386_ABSOLUTE: u16 = 0x0000;
    /// 16-bit virtual address (unsupported).
    pub const IMAGE_REL_I386_DIR16: u16 = 0x0001;
    /// 16-bit PC-relative (unsupported).
    pub const IMAGE_REL_I386_REL16: u16 = 0x0002;
    /// 32-bit virtual address.
    pub const IMAGE_REL_I386_DIR32: u16 = 0x0006;
    /// 32-bit address without the image base (RVA).
    pub const IMAGE_REL_I386_DIR32NB: u16 = 0x0007;
    /// Segment selector (unsupported).
    pub const IMAGE_REL_I386_SEG12: u16 = 0x0009;
    /// 16-bit section index.
    pub const IMAGE_REL_I386_SECTION: u16 = 0x000a;
    /// 32-bit offset from the start of the target's section.
    pub const IMAGE_REL_I386_SECREL: u16 = 0x000b;
    /// CLR token.
    pub const IMAGE_REL_I386_TOKEN: u16 = 0x000c;
    /// 7-bit section offset.
    pub const IMAGE_REL_I386_SECREL7: u16 = 0x000d;
    /// 32-bit PC-relative.
    pub const IMAGE_REL_I386_REL32: u16 = 0x0014;

    /// The relocation type name, as `llvm-readobj` prints it.
    #[must_use]
    pub fn name(r_type: u16) -> Option<&'static str> {
        Some(match r_type {
            IMAGE_REL_I386_ABSOLUTE => "IMAGE_REL_I386_ABSOLUTE",
            IMAGE_REL_I386_DIR16 => "IMAGE_REL_I386_DIR16",
            IMAGE_REL_I386_REL16 => "IMAGE_REL_I386_REL16",
            IMAGE_REL_I386_DIR32 => "IMAGE_REL_I386_DIR32",
            IMAGE_REL_I386_DIR32NB => "IMAGE_REL_I386_DIR32NB",
            IMAGE_REL_I386_SEG12 => "IMAGE_REL_I386_SEG12",
            IMAGE_REL_I386_SECTION => "IMAGE_REL_I386_SECTION",
            IMAGE_REL_I386_SECREL => "IMAGE_REL_I386_SECREL",
            IMAGE_REL_I386_TOKEN => "IMAGE_REL_I386_TOKEN",
            IMAGE_REL_I386_SECREL7 => "IMAGE_REL_I386_SECREL7",
            IMAGE_REL_I386_REL32 => "IMAGE_REL_I386_REL32",
            _ => return None,
        })
    }
}

/// ARM64 (`IMAGE_REL_ARM64_*`) relocation types, shared by ARM64EC and
/// ARM64X.
pub mod arm64 {
    /// Ignored.
    pub const IMAGE_REL_ARM64_ABSOLUTE: u16 = 0x0000;
    /// 32-bit virtual address.
    pub const IMAGE_REL_ARM64_ADDR32: u16 = 0x0001;
    /// 32-bit address without the image base (RVA).
    pub const IMAGE_REL_ARM64_ADDR32NB: u16 = 0x0002;
    /// 26-bit PC-relative branch (`B`, `BL`).
    pub const IMAGE_REL_ARM64_BRANCH26: u16 = 0x0003;
    /// Page base of the target, for `ADRP`.
    pub const IMAGE_REL_ARM64_PAGEBASE_REL21: u16 = 0x0004;
    /// 21-bit PC-relative, for `ADR`.
    pub const IMAGE_REL_ARM64_REL21: u16 = 0x0005;
    /// 12-bit page offset, for `ADD`/`ADDS`.
    pub const IMAGE_REL_ARM64_PAGEOFFSET_12A: u16 = 0x0006;
    /// 12-bit page offset, for `LDR` (scaled).
    pub const IMAGE_REL_ARM64_PAGEOFFSET_12L: u16 = 0x0007;
    /// 32-bit offset from the start of the target's section.
    pub const IMAGE_REL_ARM64_SECREL: u16 = 0x0008;
    /// Low 12 bits of the section offset, for `ADD`.
    pub const IMAGE_REL_ARM64_SECREL_LOW12A: u16 = 0x0009;
    /// Bits 12–23 of the section offset, for `ADD`.
    pub const IMAGE_REL_ARM64_SECREL_HIGH12A: u16 = 0x000a;
    /// Low 12 bits of the section offset, for `LDR`.
    pub const IMAGE_REL_ARM64_SECREL_LOW12L: u16 = 0x000b;
    /// CLR token.
    pub const IMAGE_REL_ARM64_TOKEN: u16 = 0x000c;
    /// 16-bit section index.
    pub const IMAGE_REL_ARM64_SECTION: u16 = 0x000d;
    /// 64-bit virtual address.
    pub const IMAGE_REL_ARM64_ADDR64: u16 = 0x000e;
    /// 19-bit PC-relative (conditional branch, `LDR` literal).
    pub const IMAGE_REL_ARM64_BRANCH19: u16 = 0x000f;
    /// 14-bit PC-relative (`TBZ`/`TBNZ`).
    pub const IMAGE_REL_ARM64_BRANCH14: u16 = 0x0010;
    /// 32-bit PC-relative.
    pub const IMAGE_REL_ARM64_REL32: u16 = 0x0011;

    /// The relocation type name, as `llvm-readobj` prints it.
    #[must_use]
    pub fn name(r_type: u16) -> Option<&'static str> {
        Some(match r_type {
            IMAGE_REL_ARM64_ABSOLUTE => "IMAGE_REL_ARM64_ABSOLUTE",
            IMAGE_REL_ARM64_ADDR32 => "IMAGE_REL_ARM64_ADDR32",
            IMAGE_REL_ARM64_ADDR32NB => "IMAGE_REL_ARM64_ADDR32NB",
            IMAGE_REL_ARM64_BRANCH26 => "IMAGE_REL_ARM64_BRANCH26",
            IMAGE_REL_ARM64_PAGEBASE_REL21 => "IMAGE_REL_ARM64_PAGEBASE_REL21",
            IMAGE_REL_ARM64_REL21 => "IMAGE_REL_ARM64_REL21",
            IMAGE_REL_ARM64_PAGEOFFSET_12A => "IMAGE_REL_ARM64_PAGEOFFSET_12A",
            IMAGE_REL_ARM64_PAGEOFFSET_12L => "IMAGE_REL_ARM64_PAGEOFFSET_12L",
            IMAGE_REL_ARM64_SECREL => "IMAGE_REL_ARM64_SECREL",
            IMAGE_REL_ARM64_SECREL_LOW12A => "IMAGE_REL_ARM64_SECREL_LOW12A",
            IMAGE_REL_ARM64_SECREL_HIGH12A => "IMAGE_REL_ARM64_SECREL_HIGH12A",
            IMAGE_REL_ARM64_SECREL_LOW12L => "IMAGE_REL_ARM64_SECREL_LOW12L",
            IMAGE_REL_ARM64_TOKEN => "IMAGE_REL_ARM64_TOKEN",
            IMAGE_REL_ARM64_SECTION => "IMAGE_REL_ARM64_SECTION",
            IMAGE_REL_ARM64_ADDR64 => "IMAGE_REL_ARM64_ADDR64",
            IMAGE_REL_ARM64_BRANCH19 => "IMAGE_REL_ARM64_BRANCH19",
            IMAGE_REL_ARM64_BRANCH14 => "IMAGE_REL_ARM64_BRANCH14",
            IMAGE_REL_ARM64_REL32 => "IMAGE_REL_ARM64_REL32",
            _ => return None,
        })
    }
}

/// ARM Thumb-2 (`IMAGE_REL_ARM_*`) relocation types, for ARMNT.
pub mod arm {
    /// Ignored.
    pub const IMAGE_REL_ARM_ABSOLUTE: u16 = 0x0000;
    /// 32-bit virtual address.
    pub const IMAGE_REL_ARM_ADDR32: u16 = 0x0001;
    /// 32-bit address without the image base (RVA).
    pub const IMAGE_REL_ARM_ADDR32NB: u16 = 0x0002;
    /// 24-bit ARM branch.
    pub const IMAGE_REL_ARM_BRANCH24: u16 = 0x0003;
    /// 11-bit branch (obsolete).
    pub const IMAGE_REL_ARM_BRANCH11: u16 = 0x0004;
    /// CLR token.
    pub const IMAGE_REL_ARM_TOKEN: u16 = 0x0005;
    /// 24-bit `BLX`.
    pub const IMAGE_REL_ARM_BLX24: u16 = 0x0008;
    /// 11-bit `BLX` (obsolete).
    pub const IMAGE_REL_ARM_BLX11: u16 = 0x0009;
    /// 32-bit PC-relative.
    pub const IMAGE_REL_ARM_REL32: u16 = 0x000a;
    /// 16-bit section index.
    pub const IMAGE_REL_ARM_SECTION: u16 = 0x000e;
    /// 32-bit offset from the start of the target's section.
    pub const IMAGE_REL_ARM_SECREL: u16 = 0x000f;
    /// `MOVW`/`MOVT` pair, ARM encoding.
    pub const IMAGE_REL_ARM_MOV32A: u16 = 0x0010;
    /// `MOVW`/`MOVT` pair, Thumb encoding.
    pub const IMAGE_REL_ARM_MOV32T: u16 = 0x0011;
    /// 20-bit Thumb conditional branch.
    pub const IMAGE_REL_ARM_BRANCH20T: u16 = 0x0012;
    /// 24-bit Thumb branch.
    pub const IMAGE_REL_ARM_BRANCH24T: u16 = 0x0014;
    /// 23-bit Thumb `BLX`.
    pub const IMAGE_REL_ARM_BLX23T: u16 = 0x0015;
    /// Pair (follows another relocation).
    pub const IMAGE_REL_ARM_PAIR: u16 = 0x0016;

    /// The relocation type name, as `llvm-readobj` prints it.
    #[must_use]
    pub fn name(r_type: u16) -> Option<&'static str> {
        Some(match r_type {
            IMAGE_REL_ARM_ABSOLUTE => "IMAGE_REL_ARM_ABSOLUTE",
            IMAGE_REL_ARM_ADDR32 => "IMAGE_REL_ARM_ADDR32",
            IMAGE_REL_ARM_ADDR32NB => "IMAGE_REL_ARM_ADDR32NB",
            IMAGE_REL_ARM_BRANCH24 => "IMAGE_REL_ARM_BRANCH24",
            IMAGE_REL_ARM_BRANCH11 => "IMAGE_REL_ARM_BRANCH11",
            IMAGE_REL_ARM_TOKEN => "IMAGE_REL_ARM_TOKEN",
            IMAGE_REL_ARM_BLX24 => "IMAGE_REL_ARM_BLX24",
            IMAGE_REL_ARM_BLX11 => "IMAGE_REL_ARM_BLX11",
            IMAGE_REL_ARM_REL32 => "IMAGE_REL_ARM_REL32",
            IMAGE_REL_ARM_SECTION => "IMAGE_REL_ARM_SECTION",
            IMAGE_REL_ARM_SECREL => "IMAGE_REL_ARM_SECREL",
            IMAGE_REL_ARM_MOV32A => "IMAGE_REL_ARM_MOV32A",
            IMAGE_REL_ARM_MOV32T => "IMAGE_REL_ARM_MOV32T",
            IMAGE_REL_ARM_BRANCH20T => "IMAGE_REL_ARM_BRANCH20T",
            IMAGE_REL_ARM_BRANCH24T => "IMAGE_REL_ARM_BRANCH24T",
            IMAGE_REL_ARM_BLX23T => "IMAGE_REL_ARM_BLX23T",
            IMAGE_REL_ARM_PAIR => "IMAGE_REL_ARM_PAIR",
            _ => return None,
        })
    }
}

/// The name of relocation type `r_type` for `machine`, as `llvm-readobj`
/// prints it, or `None` for an unknown machine or type.
#[must_use]
pub fn relocation_name(machine: u16, r_type: u16) -> Option<&'static str> {
    match machine {
        IMAGE_FILE_MACHINE_AMD64 => amd64::name(r_type),
        IMAGE_FILE_MACHINE_I386 => i386::name(r_type),
        IMAGE_FILE_MACHINE_ARMNT | IMAGE_FILE_MACHINE_ARM | IMAGE_FILE_MACHINE_THUMB => {
            arm::name(r_type)
        }
        m if is_arm64(m) => arm64::name(r_type),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Import objects
// ---------------------------------------------------------------------------

/// Import type: executable code (a thunk symbol and `__imp_` pointer).
pub const IMPORT_CODE: u8 = 0;
/// Import type: data (only the `__imp_` pointer).
pub const IMPORT_DATA: u8 = 1;
/// Import type: constant (only the `__imp_` pointer; obsolete).
pub const IMPORT_CONST: u8 = 2;

/// Name type: import by ordinal.
pub const IMPORT_ORDINAL: u8 = 0;
/// Name type: the import name is the symbol name.
pub const IMPORT_NAME: u8 = 1;
/// Name type: the symbol name without its leading `?`, `@` or `_`.
pub const IMPORT_NAME_NOPREFIX: u8 = 2;
/// Name type: the symbol name without its leading `?`, `@` or `_`, cut at
/// the first `@`.
pub const IMPORT_NAME_UNDECORATE: u8 = 3;
/// Name type: the import name is stored after the DLL name.
pub const IMPORT_NAME_EXPORTAS: u8 = 4;

// ---------------------------------------------------------------------------
// PE images
// ---------------------------------------------------------------------------

/// `e_magic` of the DOS header: `MZ`.
pub const IMAGE_DOS_SIGNATURE: u16 = 0x5a4d;
/// The PE signature, `PE\0\0`.
pub const IMAGE_NT_SIGNATURE: [u8; 4] = *b"PE\0\0";
/// Optional header magic of a PE32 image.
pub const IMAGE_NT_OPTIONAL_HDR32_MAGIC: u16 = 0x10b;
/// Optional header magic of a PE32+ image.
pub const IMAGE_NT_OPTIONAL_HDR64_MAGIC: u16 = 0x20b;

/// Data directory: export table.
pub const IMAGE_DIRECTORY_ENTRY_EXPORT: usize = 0;
/// Data directory: import table.
pub const IMAGE_DIRECTORY_ENTRY_IMPORT: usize = 1;
/// Data directory: resource table.
pub const IMAGE_DIRECTORY_ENTRY_RESOURCE: usize = 2;
/// Data directory: exception table (`.pdata`).
pub const IMAGE_DIRECTORY_ENTRY_EXCEPTION: usize = 3;
/// Data directory: certificate table (a file offset, not an RVA).
pub const IMAGE_DIRECTORY_ENTRY_SECURITY: usize = 4;
/// Data directory: base relocation table.
pub const IMAGE_DIRECTORY_ENTRY_BASERELOC: usize = 5;
/// Data directory: debug directory.
pub const IMAGE_DIRECTORY_ENTRY_DEBUG: usize = 6;
/// Data directory: architecture (reserved).
pub const IMAGE_DIRECTORY_ENTRY_ARCHITECTURE: usize = 7;
/// Data directory: global pointer.
pub const IMAGE_DIRECTORY_ENTRY_GLOBALPTR: usize = 8;
/// Data directory: TLS directory.
pub const IMAGE_DIRECTORY_ENTRY_TLS: usize = 9;
/// Data directory: load configuration.
pub const IMAGE_DIRECTORY_ENTRY_LOAD_CONFIG: usize = 10;
/// Data directory: bound imports.
pub const IMAGE_DIRECTORY_ENTRY_BOUND_IMPORT: usize = 11;
/// Data directory: import address table.
pub const IMAGE_DIRECTORY_ENTRY_IAT: usize = 12;
/// Data directory: delay-load imports.
pub const IMAGE_DIRECTORY_ENTRY_DELAY_IMPORT: usize = 13;
/// Data directory: CLR runtime header.
pub const IMAGE_DIRECTORY_ENTRY_COM_DESCRIPTOR: usize = 14;

/// Subsystem: unknown.
pub const IMAGE_SUBSYSTEM_UNKNOWN: u16 = 0;
/// Subsystem: device drivers and native processes.
pub const IMAGE_SUBSYSTEM_NATIVE: u16 = 1;
/// Subsystem: Windows GUI.
pub const IMAGE_SUBSYSTEM_WINDOWS_GUI: u16 = 2;
/// Subsystem: Windows console.
pub const IMAGE_SUBSYSTEM_WINDOWS_CUI: u16 = 3;
/// Subsystem: OS/2 console.
pub const IMAGE_SUBSYSTEM_OS2_CUI: u16 = 5;
/// Subsystem: POSIX console.
pub const IMAGE_SUBSYSTEM_POSIX_CUI: u16 = 7;
/// Subsystem: native Win9x driver.
pub const IMAGE_SUBSYSTEM_NATIVE_WINDOWS: u16 = 8;
/// Subsystem: Windows CE.
pub const IMAGE_SUBSYSTEM_WINDOWS_CE_GUI: u16 = 9;
/// Subsystem: EFI application.
pub const IMAGE_SUBSYSTEM_EFI_APPLICATION: u16 = 10;
/// Subsystem: EFI boot service driver.
pub const IMAGE_SUBSYSTEM_EFI_BOOT_SERVICE_DRIVER: u16 = 11;
/// Subsystem: EFI runtime driver.
pub const IMAGE_SUBSYSTEM_EFI_RUNTIME_DRIVER: u16 = 12;
/// Subsystem: EFI ROM image.
pub const IMAGE_SUBSYSTEM_EFI_ROM: u16 = 13;
/// Subsystem: Xbox.
pub const IMAGE_SUBSYSTEM_XBOX: u16 = 14;
/// Subsystem: Windows boot application.
pub const IMAGE_SUBSYSTEM_WINDOWS_BOOT_APPLICATION: u16 = 16;

/// DLL characteristics: can handle a high-entropy 64-bit address space.
pub const IMAGE_DLLCHARACTERISTICS_HIGH_ENTROPY_VA: u16 = 0x0020;
/// DLL characteristics: can be relocated at load time (ASLR).
pub const IMAGE_DLLCHARACTERISTICS_DYNAMIC_BASE: u16 = 0x0040;
/// DLL characteristics: code integrity checks are enforced.
pub const IMAGE_DLLCHARACTERISTICS_FORCE_INTEGRITY: u16 = 0x0080;
/// DLL characteristics: compatible with DEP.
pub const IMAGE_DLLCHARACTERISTICS_NX_COMPAT: u16 = 0x0100;
/// DLL characteristics: isolation aware, but do not isolate.
pub const IMAGE_DLLCHARACTERISTICS_NO_ISOLATION: u16 = 0x0200;
/// DLL characteristics: does not use structured exception handling.
pub const IMAGE_DLLCHARACTERISTICS_NO_SEH: u16 = 0x0400;
/// DLL characteristics: do not bind the image.
pub const IMAGE_DLLCHARACTERISTICS_NO_BIND: u16 = 0x0800;
/// DLL characteristics: must run in an AppContainer.
pub const IMAGE_DLLCHARACTERISTICS_APPCONTAINER: u16 = 0x1000;
/// DLL characteristics: a WDM driver.
pub const IMAGE_DLLCHARACTERISTICS_WDM_DRIVER: u16 = 0x2000;
/// DLL characteristics: supports Control Flow Guard.
pub const IMAGE_DLLCHARACTERISTICS_GUARD_CF: u16 = 0x4000;
/// DLL characteristics: terminal-server aware.
pub const IMAGE_DLLCHARACTERISTICS_TERMINAL_SERVER_AWARE: u16 = 0x8000;
