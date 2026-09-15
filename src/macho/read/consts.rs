//! Mach-O ABI constants, from `<mach-o/loader.h>`, `<mach-o/nlist.h>`,
//! `<mach-o/reloc.h>`, `<mach-o/arm64/reloc.h>`, `<mach-o/x86_64/reloc.h>`,
//! `<mach-o/fat.h>`, `<mach-o/compact_unwind_encoding.h>` and
//! `<mach-o/fixup-chains.h>`, plus relocation type names.

// ---------------------------------------------------------------------------
// Magic numbers
// ---------------------------------------------------------------------------

/// 32-bit Mach-O magic, in the file's byte order.
pub const MH_MAGIC: u32 = 0xfeed_face;
/// [`MH_MAGIC`] read with the wrong byte order.
pub const MH_CIGAM: u32 = 0xcefa_edfe;
/// 64-bit Mach-O magic, in the file's byte order.
pub const MH_MAGIC_64: u32 = 0xfeed_facf;
/// [`MH_MAGIC_64`] read with the wrong byte order.
pub const MH_CIGAM_64: u32 = 0xcffa_edfe;
/// Universal binary magic (big-endian, 32-bit offsets).
pub const FAT_MAGIC: u32 = 0xcafe_babe;
/// Universal binary magic (big-endian, 64-bit offsets).
pub const FAT_MAGIC_64: u32 = 0xcafe_babf;

// ---------------------------------------------------------------------------
// CPU types
// ---------------------------------------------------------------------------

/// `cputype` flag: 64-bit ABI.
pub const CPU_ARCH_ABI64: u32 = 0x0100_0000;
/// `cputype` flag: 64-bit hardware with 32-bit pointers (arm64_32).
pub const CPU_ARCH_ABI64_32: u32 = 0x0200_0000;
/// 32-bit x86.
pub const CPU_TYPE_X86: u32 = 7;
/// x86-64.
pub const CPU_TYPE_X86_64: u32 = CPU_TYPE_X86 | CPU_ARCH_ABI64;
/// 32-bit Arm.
pub const CPU_TYPE_ARM: u32 = 12;
/// 64-bit Arm.
pub const CPU_TYPE_ARM64: u32 = CPU_TYPE_ARM | CPU_ARCH_ABI64;
/// 64-bit Arm with 32-bit pointers.
pub const CPU_TYPE_ARM64_32: u32 = CPU_TYPE_ARM | CPU_ARCH_ABI64_32;
/// 32-bit PowerPC.
pub const CPU_TYPE_POWERPC: u32 = 18;
/// 64-bit PowerPC.
pub const CPU_TYPE_POWERPC64: u32 = CPU_TYPE_POWERPC | CPU_ARCH_ABI64;

/// Capability bits of `cpusubtype`; masked off before comparing subtypes.
pub const CPU_SUBTYPE_MASK: u32 = 0xff00_0000;
/// `cpusubtype` capability bit: 64-bit libraries (x86_64 executables).
pub const CPU_SUBTYPE_LIB64: u32 = 0x8000_0000;
/// `cpusubtype` capability bit on arm64e: pointer authentication ABI.
pub const CPU_SUBTYPE_PTRAUTH_ABI: u32 = 0x8000_0000;
/// All 32-bit x86 processors.
pub const CPU_SUBTYPE_I386_ALL: u32 = 3;
/// All x86-64 processors.
pub const CPU_SUBTYPE_X86_64_ALL: u32 = 3;
/// x86-64 Haswell and later (`x86_64h`).
pub const CPU_SUBTYPE_X86_64_H: u32 = 8;
/// All 64-bit Arm processors.
pub const CPU_SUBTYPE_ARM64_ALL: u32 = 0;
/// Armv8.
pub const CPU_SUBTYPE_ARM64_V8: u32 = 1;
/// arm64e (pointer authentication).
pub const CPU_SUBTYPE_ARM64E: u32 = 2;
/// arm64_32 on Armv8.
pub const CPU_SUBTYPE_ARM64_32_V8: u32 = 1;
/// All 32-bit Arm processors.
pub const CPU_SUBTYPE_ARM_ALL: u32 = 0;
/// Armv6.
pub const CPU_SUBTYPE_ARM_V6: u32 = 6;
/// Armv7.
pub const CPU_SUBTYPE_ARM_V7: u32 = 9;
/// Armv7s.
pub const CPU_SUBTYPE_ARM_V7S: u32 = 11;
/// Armv7k.
pub const CPU_SUBTYPE_ARM_V7K: u32 = 12;
/// Armv6-M.
pub const CPU_SUBTYPE_ARM_V6M: u32 = 14;
/// Armv7-M.
pub const CPU_SUBTYPE_ARM_V7M: u32 = 15;
/// Armv7E-M.
pub const CPU_SUBTYPE_ARM_V7EM: u32 = 16;
/// All PowerPC processors.
pub const CPU_SUBTYPE_POWERPC_ALL: u32 = 0;

// ---------------------------------------------------------------------------
// Header
// ---------------------------------------------------------------------------

/// Relocatable object file.
pub const MH_OBJECT: u32 = 0x1;
/// Demand-paged executable.
pub const MH_EXECUTE: u32 = 0x2;
/// Fixed VM shared library.
pub const MH_FVMLIB: u32 = 0x3;
/// Core file.
pub const MH_CORE: u32 = 0x4;
/// Preloaded executable.
pub const MH_PRELOAD: u32 = 0x5;
/// Dynamically bound shared library.
pub const MH_DYLIB: u32 = 0x6;
/// The dynamic linker.
pub const MH_DYLINKER: u32 = 0x7;
/// Dynamically bound bundle.
pub const MH_BUNDLE: u32 = 0x8;
/// Shared library stub for static linking only (no section contents).
pub const MH_DYLIB_STUB: u32 = 0x9;
/// Companion file with debug sections only.
pub const MH_DSYM: u32 = 0xa;
/// Kernel extension bundle.
pub const MH_KEXT_BUNDLE: u32 = 0xb;
/// Kernel file set.
pub const MH_FILESET: u32 = 0xc;

/// No undefined references.
pub const MH_NOUNDEFS: u32 = 0x1;
/// Output of an incremental link.
pub const MH_INCRLINK: u32 = 0x2;
/// Input for the dynamic linker.
pub const MH_DYLDLINK: u32 = 0x4;
/// Undefined references bound at load.
pub const MH_BINDATLOAD: u32 = 0x8;
/// Prebound undefined references.
pub const MH_PREBOUND: u32 = 0x10;
/// Read-only and read-write segments split.
pub const MH_SPLIT_SEGS: u32 = 0x20;
/// Obsolete.
pub const MH_LAZY_INIT: u32 = 0x40;
/// Two-level namespace bindings.
pub const MH_TWOLEVEL: u32 = 0x80;
/// Flat namespace bindings forced.
pub const MH_FORCE_FLAT: u32 = 0x100;
/// No multiple definitions in sub-images.
pub const MH_NOMULTIDEFS: u32 = 0x200;
/// Do not notify the prebinding agent.
pub const MH_NOFIXPREBINDING: u32 = 0x400;
/// Not prebound, but can be.
pub const MH_PREBINDABLE: u32 = 0x800;
/// Bound to all two-level namespace modules.
pub const MH_ALLMODSBOUND: u32 = 0x1000;
/// Sections can be divided into atoms at symbol boundaries
/// (`.subsections_via_symbols`).
pub const MH_SUBSECTIONS_VIA_SYMBOLS: u32 = 0x2000;
/// Canonicalized by unprebinding.
pub const MH_CANONICAL: u32 = 0x4000;
/// The image defines weak symbols.
pub const MH_WEAK_DEFINES: u32 = 0x8000;
/// The image uses weak symbols.
pub const MH_BINDS_TO_WEAK: u32 = 0x10000;
/// Stacks are executable.
pub const MH_ALLOW_STACK_EXECUTION: u32 = 0x20000;
/// Safe for use in root processes.
pub const MH_ROOT_SAFE: u32 = 0x40000;
/// Safe for use in setuid processes.
pub const MH_SETUID_SAFE: u32 = 0x80000;
/// The dylib re-exports nothing (`LC_REEXPORT_DYLIB` absent).
pub const MH_NO_REEXPORTED_DYLIBS: u32 = 0x10_0000;
/// Position-independent executable.
pub const MH_PIE: u32 = 0x20_0000;
/// Unreferenced dylib can be dropped (`-dead_strip_dylibs`).
pub const MH_DEAD_STRIPPABLE_DYLIB: u32 = 0x40_0000;
/// Has `S_THREAD_LOCAL_VARIABLES` sections.
pub const MH_HAS_TLV_DESCRIPTORS: u32 = 0x80_0000;
/// Heap is not executable.
pub const MH_NO_HEAP_EXECUTION: u32 = 0x100_0000;
/// Safe for use in application extensions.
pub const MH_APP_EXTENSION_SAFE: u32 = 0x0200_0000;
/// The nlist symbol table is out of sync with the export trie.
pub const MH_NLIST_OUTOFSYNC_WITH_DYLDINFO: u32 = 0x0400_0000;
/// Runs in the simulator too.
pub const MH_SIM_SUPPORT: u32 = 0x0800_0000;
/// The dylib is part of the dyld shared cache.
pub const MH_DYLIB_IN_CACHE: u32 = 0x8000_0000;

// ---------------------------------------------------------------------------
// Load commands
// ---------------------------------------------------------------------------

/// Load command flag: dyld must understand this command.
pub const LC_REQ_DYLD: u32 = 0x8000_0000;
/// 32-bit segment.
pub const LC_SEGMENT: u32 = 0x1;
/// Symbol table.
pub const LC_SYMTAB: u32 = 0x2;
/// GDB symbol table (obsolete).
pub const LC_SYMSEG: u32 = 0x3;
/// Thread state.
pub const LC_THREAD: u32 = 0x4;
/// Unix thread state (entry point).
pub const LC_UNIXTHREAD: u32 = 0x5;
/// Load a fixed VM library.
pub const LC_LOADFVMLIB: u32 = 0x6;
/// Fixed VM library identification.
pub const LC_IDFVMLIB: u32 = 0x7;
/// Object identification (obsolete).
pub const LC_IDENT: u32 = 0x8;
/// Fixed VM file inclusion.
pub const LC_FVMFILE: u32 = 0x9;
/// Prepage command.
pub const LC_PREPAGE: u32 = 0xa;
/// Dynamic link-edit symbol table information.
pub const LC_DYSYMTAB: u32 = 0xb;
/// Load a dylib.
pub const LC_LOAD_DYLIB: u32 = 0xc;
/// Dylib identification (install name).
pub const LC_ID_DYLIB: u32 = 0xd;
/// Load a dynamic linker.
pub const LC_LOAD_DYLINKER: u32 = 0xe;
/// Dynamic linker identification.
pub const LC_ID_DYLINKER: u32 = 0xf;
/// Modules prebound for a dylib.
pub const LC_PREBOUND_DYLIB: u32 = 0x10;
/// Image routines.
pub const LC_ROUTINES: u32 = 0x11;
/// Sub-framework (parent umbrella).
pub const LC_SUB_FRAMEWORK: u32 = 0x12;
/// Sub-umbrella.
pub const LC_SUB_UMBRELLA: u32 = 0x13;
/// Allowable client.
pub const LC_SUB_CLIENT: u32 = 0x14;
/// Sub-library.
pub const LC_SUB_LIBRARY: u32 = 0x15;
/// Two-level namespace lookup hints.
pub const LC_TWOLEVEL_HINTS: u32 = 0x16;
/// Prebind checksum.
pub const LC_PREBIND_CKSUM: u32 = 0x17;
/// Load a dylib, allowed to be missing (weak import).
pub const LC_LOAD_WEAK_DYLIB: u32 = 0x18 | LC_REQ_DYLD;
/// 64-bit segment.
pub const LC_SEGMENT_64: u32 = 0x19;
/// 64-bit image routines.
pub const LC_ROUTINES_64: u32 = 0x1a;
/// UUID.
pub const LC_UUID: u32 = 0x1b;
/// Run path addition.
pub const LC_RPATH: u32 = 0x1c | LC_REQ_DYLD;
/// Code signature.
pub const LC_CODE_SIGNATURE: u32 = 0x1d;
/// Segment split information.
pub const LC_SEGMENT_SPLIT_INFO: u32 = 0x1e;
/// Load and re-export a dylib.
pub const LC_REEXPORT_DYLIB: u32 = 0x1f | LC_REQ_DYLD;
/// Lazily load a dylib.
pub const LC_LAZY_LOAD_DYLIB: u32 = 0x20;
/// Encrypted segment information.
pub const LC_ENCRYPTION_INFO: u32 = 0x21;
/// Compressed dyld information.
pub const LC_DYLD_INFO: u32 = 0x22;
/// Compressed dyld information only.
pub const LC_DYLD_INFO_ONLY: u32 = 0x22 | LC_REQ_DYLD;
/// Load an upward dylib.
pub const LC_LOAD_UPWARD_DYLIB: u32 = 0x23 | LC_REQ_DYLD;
/// Minimum macOS version.
pub const LC_VERSION_MIN_MACOSX: u32 = 0x24;
/// Minimum iOS version.
pub const LC_VERSION_MIN_IPHONEOS: u32 = 0x25;
/// Compressed table of function start addresses.
pub const LC_FUNCTION_STARTS: u32 = 0x26;
/// Environment variable for dyld.
pub const LC_DYLD_ENVIRONMENT: u32 = 0x27;
/// Main entry point.
pub const LC_MAIN: u32 = 0x28 | LC_REQ_DYLD;
/// Data-in-code table.
pub const LC_DATA_IN_CODE: u32 = 0x29;
/// Source version.
pub const LC_SOURCE_VERSION: u32 = 0x2a;
/// Code signing designated requirements copied from dylibs.
pub const LC_DYLIB_CODE_SIGN_DRS: u32 = 0x2b;
/// 64-bit encrypted segment information.
pub const LC_ENCRYPTION_INFO_64: u32 = 0x2c;
/// Linker options (`-l`, `-framework`) embedded in an object.
pub const LC_LINKER_OPTION: u32 = 0x2d;
/// Linker optimization hints.
pub const LC_LINKER_OPTIMIZATION_HINT: u32 = 0x2e;
/// Minimum tvOS version.
pub const LC_VERSION_MIN_TVOS: u32 = 0x2f;
/// Minimum watchOS version.
pub const LC_VERSION_MIN_WATCHOS: u32 = 0x30;
/// Arbitrary note.
pub const LC_NOTE: u32 = 0x31;
/// Platform, minimum OS and SDK versions.
pub const LC_BUILD_VERSION: u32 = 0x32;
/// Export trie.
pub const LC_DYLD_EXPORTS_TRIE: u32 = 0x33 | LC_REQ_DYLD;
/// Chained fixups.
pub const LC_DYLD_CHAINED_FIXUPS: u32 = 0x34 | LC_REQ_DYLD;
/// File set entry.
pub const LC_FILESET_ENTRY: u32 = 0x35 | LC_REQ_DYLD;
/// Atom information.
pub const LC_ATOM_INFO: u32 = 0x36;

/// Returns the name of a load command, such as `"LC_SEGMENT_64"`.
#[must_use]
pub fn load_command_name(cmd: u32) -> Option<&'static str> {
    Some(match cmd {
        LC_SEGMENT => "LC_SEGMENT",
        LC_SYMTAB => "LC_SYMTAB",
        LC_SYMSEG => "LC_SYMSEG",
        LC_THREAD => "LC_THREAD",
        LC_UNIXTHREAD => "LC_UNIXTHREAD",
        LC_LOADFVMLIB => "LC_LOADFVMLIB",
        LC_IDFVMLIB => "LC_IDFVMLIB",
        LC_IDENT => "LC_IDENT",
        LC_FVMFILE => "LC_FVMFILE",
        LC_PREPAGE => "LC_PREPAGE",
        LC_DYSYMTAB => "LC_DYSYMTAB",
        LC_LOAD_DYLIB => "LC_LOAD_DYLIB",
        LC_ID_DYLIB => "LC_ID_DYLIB",
        LC_LOAD_DYLINKER => "LC_LOAD_DYLINKER",
        LC_ID_DYLINKER => "LC_ID_DYLINKER",
        LC_PREBOUND_DYLIB => "LC_PREBOUND_DYLIB",
        LC_ROUTINES => "LC_ROUTINES",
        LC_SUB_FRAMEWORK => "LC_SUB_FRAMEWORK",
        LC_SUB_UMBRELLA => "LC_SUB_UMBRELLA",
        LC_SUB_CLIENT => "LC_SUB_CLIENT",
        LC_SUB_LIBRARY => "LC_SUB_LIBRARY",
        LC_TWOLEVEL_HINTS => "LC_TWOLEVEL_HINTS",
        LC_PREBIND_CKSUM => "LC_PREBIND_CKSUM",
        LC_LOAD_WEAK_DYLIB => "LC_LOAD_WEAK_DYLIB",
        LC_SEGMENT_64 => "LC_SEGMENT_64",
        LC_ROUTINES_64 => "LC_ROUTINES_64",
        LC_UUID => "LC_UUID",
        LC_RPATH => "LC_RPATH",
        LC_CODE_SIGNATURE => "LC_CODE_SIGNATURE",
        LC_SEGMENT_SPLIT_INFO => "LC_SEGMENT_SPLIT_INFO",
        LC_REEXPORT_DYLIB => "LC_REEXPORT_DYLIB",
        LC_LAZY_LOAD_DYLIB => "LC_LAZY_LOAD_DYLIB",
        LC_ENCRYPTION_INFO => "LC_ENCRYPTION_INFO",
        LC_DYLD_INFO => "LC_DYLD_INFO",
        LC_DYLD_INFO_ONLY => "LC_DYLD_INFO_ONLY",
        LC_LOAD_UPWARD_DYLIB => "LC_LOAD_UPWARD_DYLIB",
        LC_VERSION_MIN_MACOSX => "LC_VERSION_MIN_MACOSX",
        LC_VERSION_MIN_IPHONEOS => "LC_VERSION_MIN_IPHONEOS",
        LC_FUNCTION_STARTS => "LC_FUNCTION_STARTS",
        LC_DYLD_ENVIRONMENT => "LC_DYLD_ENVIRONMENT",
        LC_MAIN => "LC_MAIN",
        LC_DATA_IN_CODE => "LC_DATA_IN_CODE",
        LC_SOURCE_VERSION => "LC_SOURCE_VERSION",
        LC_DYLIB_CODE_SIGN_DRS => "LC_DYLIB_CODE_SIGN_DRS",
        LC_ENCRYPTION_INFO_64 => "LC_ENCRYPTION_INFO_64",
        LC_LINKER_OPTION => "LC_LINKER_OPTION",
        LC_LINKER_OPTIMIZATION_HINT => "LC_LINKER_OPTIMIZATION_HINT",
        LC_VERSION_MIN_TVOS => "LC_VERSION_MIN_TVOS",
        LC_VERSION_MIN_WATCHOS => "LC_VERSION_MIN_WATCHOS",
        LC_NOTE => "LC_NOTE",
        LC_BUILD_VERSION => "LC_BUILD_VERSION",
        LC_DYLD_EXPORTS_TRIE => "LC_DYLD_EXPORTS_TRIE",
        LC_DYLD_CHAINED_FIXUPS => "LC_DYLD_CHAINED_FIXUPS",
        LC_FILESET_ENTRY => "LC_FILESET_ENTRY",
        LC_ATOM_INFO => "LC_ATOM_INFO",
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// Platforms and tools (LC_BUILD_VERSION)
// ---------------------------------------------------------------------------

/// Unknown platform.
pub const PLATFORM_UNKNOWN: u32 = 0;
/// macOS.
pub const PLATFORM_MACOS: u32 = 1;
/// iOS.
pub const PLATFORM_IOS: u32 = 2;
/// tvOS.
pub const PLATFORM_TVOS: u32 = 3;
/// watchOS.
pub const PLATFORM_WATCHOS: u32 = 4;
/// bridgeOS.
pub const PLATFORM_BRIDGEOS: u32 = 5;
/// Mac Catalyst.
pub const PLATFORM_MACCATALYST: u32 = 6;
/// iOS simulator.
pub const PLATFORM_IOSSIMULATOR: u32 = 7;
/// tvOS simulator.
pub const PLATFORM_TVOSSIMULATOR: u32 = 8;
/// watchOS simulator.
pub const PLATFORM_WATCHOSSIMULATOR: u32 = 9;
/// DriverKit.
pub const PLATFORM_DRIVERKIT: u32 = 10;
/// visionOS.
pub const PLATFORM_XROS: u32 = 11;
/// visionOS simulator.
pub const PLATFORM_XROS_SIMULATOR: u32 = 12;

/// `clang`.
pub const TOOL_CLANG: u32 = 1;
/// `swiftc`.
pub const TOOL_SWIFT: u32 = 2;
/// ld64.
pub const TOOL_LD: u32 = 3;
/// lld.
pub const TOOL_LLD: u32 = 4;

// ---------------------------------------------------------------------------
// Sections
// ---------------------------------------------------------------------------

/// Mask of the section type in `flags`.
pub const SECTION_TYPE: u32 = 0x0000_00ff;
/// Mask of the section attributes in `flags`.
pub const SECTION_ATTRIBUTES: u32 = 0xffff_ff00;
/// Regular section.
pub const S_REGULAR: u32 = 0x0;
/// Zero-fill on demand.
pub const S_ZEROFILL: u32 = 0x1;
/// Only literal C strings.
pub const S_CSTRING_LITERALS: u32 = 0x2;
/// Only 4-byte literals.
pub const S_4BYTE_LITERALS: u32 = 0x3;
/// Only 8-byte literals.
pub const S_8BYTE_LITERALS: u32 = 0x4;
/// Only pointers to literals.
pub const S_LITERAL_POINTERS: u32 = 0x5;
/// Non-lazy symbol pointers.
pub const S_NON_LAZY_SYMBOL_POINTERS: u32 = 0x6;
/// Lazy symbol pointers.
pub const S_LAZY_SYMBOL_POINTERS: u32 = 0x7;
/// Symbol stubs.
pub const S_SYMBOL_STUBS: u32 = 0x8;
/// Initialization function pointers.
pub const S_MOD_INIT_FUNC_POINTERS: u32 = 0x9;
/// Termination function pointers.
pub const S_MOD_TERM_FUNC_POINTERS: u32 = 0xa;
/// Coalesced symbols.
pub const S_COALESCED: u32 = 0xb;
/// Zero fill on demand, may exceed 4 GiB.
pub const S_GB_ZEROFILL: u32 = 0xc;
/// Pairs of function pointers for interposing.
pub const S_INTERPOSING: u32 = 0xd;
/// Only 16-byte literals.
pub const S_16BYTE_LITERALS: u32 = 0xe;
/// DTrace object format.
pub const S_DTRACE_DOF: u32 = 0xf;
/// Lazy symbol pointers to lazily loaded dylibs.
pub const S_LAZY_DYLIB_SYMBOL_POINTERS: u32 = 0x10;
/// Initial values of thread-local variables.
pub const S_THREAD_LOCAL_REGULAR: u32 = 0x11;
/// Zero-filled thread-local variables.
pub const S_THREAD_LOCAL_ZEROFILL: u32 = 0x12;
/// Thread-local variable descriptors.
pub const S_THREAD_LOCAL_VARIABLES: u32 = 0x13;
/// Pointers to thread-local variable descriptors.
pub const S_THREAD_LOCAL_VARIABLE_POINTERS: u32 = 0x14;
/// Thread-local initialization function pointers.
pub const S_THREAD_LOCAL_INIT_FUNCTION_POINTERS: u32 = 0x15;
/// 32-bit offsets to initializers.
pub const S_INIT_FUNC_OFFSETS: u32 = 0x16;

/// Only true machine instructions.
pub const S_ATTR_PURE_INSTRUCTIONS: u32 = 0x8000_0000;
/// Coalesced symbols not to be put in a table of contents.
pub const S_ATTR_NO_TOC: u32 = 0x4000_0000;
/// Static symbols may be stripped (`-dead_strip` of locals).
pub const S_ATTR_STRIP_STATIC_SYMS: u32 = 0x2000_0000;
/// Never dead strip the contents.
pub const S_ATTR_NO_DEAD_STRIP: u32 = 0x1000_0000;
/// Live if any block it references is live.
pub const S_ATTR_LIVE_SUPPORT: u32 = 0x0800_0000;
/// Used with i386 code stubs written on by dyld.
pub const S_ATTR_SELF_MODIFYING_CODE: u32 = 0x0400_0000;
/// Debug information (DWARF).
pub const S_ATTR_DEBUG: u32 = 0x0200_0000;
/// Some machine instructions.
pub const S_ATTR_SOME_INSTRUCTIONS: u32 = 0x0000_0400;
/// Has external relocation entries.
pub const S_ATTR_EXT_RELOC: u32 = 0x0000_0200;
/// Has local relocation entries.
pub const S_ATTR_LOC_RELOC: u32 = 0x0000_0100;

// ---------------------------------------------------------------------------
// Symbols (nlist)
// ---------------------------------------------------------------------------

/// Mask of the STABS bits in `n_type`: non-zero means a debugging entry.
pub const N_STAB: u8 = 0xe0;
/// Private external symbol.
pub const N_PEXT: u8 = 0x10;
/// Mask of the type bits in `n_type`.
pub const N_TYPE: u8 = 0x0e;
/// External symbol.
pub const N_EXT: u8 = 0x01;
/// Undefined (or common, when `n_value` is non-zero).
pub const N_UNDF: u8 = 0x0;
/// Absolute.
pub const N_ABS: u8 = 0x2;
/// Defined in section `n_sect`.
pub const N_SECT: u8 = 0xe;
/// Prebound undefined.
pub const N_PBUD: u8 = 0xc;
/// Indirect: an alias of the symbol named at string offset `n_value`.
pub const N_INDR: u8 = 0xa;
/// `n_sect` value: no section.
pub const NO_SECT: u8 = 0;
/// Largest section ordinal.
pub const MAX_SECT: u8 = 255;

/// Mask of the reference type in `n_desc`.
pub const REFERENCE_TYPE: u16 = 0x7;
/// Undefined, non-lazy reference.
pub const REFERENCE_FLAG_UNDEFINED_NON_LAZY: u16 = 0;
/// Undefined, lazy reference.
pub const REFERENCE_FLAG_UNDEFINED_LAZY: u16 = 1;
/// Defined.
pub const REFERENCE_FLAG_DEFINED: u16 = 2;
/// Private defined.
pub const REFERENCE_FLAG_PRIVATE_DEFINED: u16 = 3;
/// Private undefined, non-lazy.
pub const REFERENCE_FLAG_PRIVATE_UNDEFINED_NON_LAZY: u16 = 4;
/// Private undefined, lazy.
pub const REFERENCE_FLAG_PRIVATE_UNDEFINED_LAZY: u16 = 5;
/// Must stay in the symbol table of a dynamically linked image.
pub const REFERENCED_DYNAMICALLY: u16 = 0x10;
/// Thumb function (32-bit Arm).
pub const N_ARM_THUMB_DEF: u16 = 0x8;
/// Never dead strip (defined symbols in objects).
pub const N_NO_DEAD_STRIP: u16 = 0x20;
/// Discarded by the static linker (same bit as [`N_NO_DEAD_STRIP`], in
/// linked images).
pub const N_DESC_DISCARDED: u16 = 0x20;
/// Weak reference: the definition may be missing.
pub const N_WEAK_REF: u16 = 0x40;
/// Weak definition: coalesced with other definitions.
pub const N_WEAK_DEF: u16 = 0x80;
/// Undefined reference to a weak symbol (same bit as [`N_WEAK_DEF`]).
pub const N_REF_TO_WEAK: u16 = 0x80;
/// Symbol is a resolver function.
pub const N_SYMBOL_RESOLVER: u16 = 0x100;
/// Alternate entry point: does not start a new atom.
pub const N_ALT_ENTRY: u16 = 0x200;
/// Cold function.
pub const N_COLD_FUNC: u16 = 0x400;

/// Library ordinal: this image.
pub const SELF_LIBRARY_ORDINAL: u8 = 0x0;
/// Largest real library ordinal.
pub const MAX_LIBRARY_ORDINAL: u8 = 0xfd;
/// Library ordinal: look the symbol up dynamically.
pub const DYNAMIC_LOOKUP_ORDINAL: u8 = 0xfe;
/// Library ordinal: the main executable.
pub const EXECUTABLE_ORDINAL: u8 = 0xff;

/// STABS: global symbol.
pub const N_GSYM: u8 = 0x20;
/// STABS: procedure name (f77).
pub const N_FNAME: u8 = 0x22;
/// STABS: procedure.
pub const N_FUN: u8 = 0x24;
/// STABS: static symbol.
pub const N_STSYM: u8 = 0x26;
/// STABS: `.lcomm` symbol.
pub const N_LCSYM: u8 = 0x28;
/// STABS: begin nsect symbol.
pub const N_BNSYM: u8 = 0x2e;
/// STABS: AST file path.
pub const N_AST: u8 = 0x32;
/// STABS: compiler options.
pub const N_OPT: u8 = 0x3c;
/// STABS: register symbol.
pub const N_RSYM: u8 = 0x40;
/// STABS: source line.
pub const N_SLINE: u8 = 0x44;
/// STABS: end nsect symbol.
pub const N_ENSYM: u8 = 0x4e;
/// STABS: structure element.
pub const N_SSYM: u8 = 0x60;
/// STABS: source file name.
pub const N_SO: u8 = 0x64;
/// STABS: object file name.
pub const N_OSO: u8 = 0x66;
/// STABS: local symbol.
pub const N_LSYM: u8 = 0x80;
/// STABS: include file beginning.
pub const N_BINCL: u8 = 0x82;
/// STABS: included file name.
pub const N_SOL: u8 = 0x84;
/// STABS: compiler parameters.
pub const N_PARAMS: u8 = 0x86;
/// STABS: compiler version.
pub const N_VERSION: u8 = 0x88;
/// STABS: compiler optimization level.
pub const N_OLEVEL: u8 = 0x8a;
/// STABS: parameter.
pub const N_PSYM: u8 = 0xa0;
/// STABS: include file end.
pub const N_EINCL: u8 = 0xa2;
/// STABS: alternate entry.
pub const N_ENTRY: u8 = 0xa4;
/// STABS: left bracket.
pub const N_LBRAC: u8 = 0xc0;
/// STABS: deleted include file.
pub const N_EXCL: u8 = 0xc2;
/// STABS: right bracket.
pub const N_RBRAC: u8 = 0xe0;
/// STABS: begin common.
pub const N_BCOMM: u8 = 0xe2;
/// STABS: end common.
pub const N_ECOMM: u8 = 0xe4;
/// STABS: end common (local name).
pub const N_ECOML: u8 = 0xe8;
/// STABS: second stab entry with length information.
pub const N_LENG: u8 = 0xfe;

// ---------------------------------------------------------------------------
// Relocations
// ---------------------------------------------------------------------------

/// Bit of `r_address` marking a scattered relocation (32-bit targets only).
pub const R_SCATTERED: u32 = 0x8000_0000;
/// `r_symbolnum` of a non-external relocation against an absolute value.
pub const R_ABS: u32 = 0;

/// Generic (i386, PowerPC): vanilla.
pub const GENERIC_RELOC_VANILLA: u8 = 0;
/// Generic: second half of a pair.
pub const GENERIC_RELOC_PAIR: u8 = 1;
/// Generic: section difference.
pub const GENERIC_RELOC_SECTDIFF: u8 = 2;
/// Generic: prebound lazy pointer.
pub const GENERIC_RELOC_PB_LA_PTR: u8 = 3;
/// Generic: local section difference.
pub const GENERIC_RELOC_LOCAL_SECTDIFF: u8 = 4;
/// Generic: thread-local variable.
pub const GENERIC_RELOC_TLV: u8 = 5;

/// x86_64: absolute address.
pub const X86_64_RELOC_UNSIGNED: u8 = 0;
/// x86_64: signed 32-bit displacement.
pub const X86_64_RELOC_SIGNED: u8 = 1;
/// x86_64: `call`/`jmp` displacement.
pub const X86_64_RELOC_BRANCH: u8 = 2;
/// x86_64: `movq` load of a GOT entry.
pub const X86_64_RELOC_GOT_LOAD: u8 = 3;
/// x86_64: other GOT references.
pub const X86_64_RELOC_GOT: u8 = 4;
/// x86_64: must be followed by an `X86_64_RELOC_UNSIGNED`.
pub const X86_64_RELOC_SUBTRACTOR: u8 = 5;
/// x86_64: signed with a -1 displacement.
pub const X86_64_RELOC_SIGNED_1: u8 = 6;
/// x86_64: signed with a -2 displacement.
pub const X86_64_RELOC_SIGNED_2: u8 = 7;
/// x86_64: signed with a -4 displacement.
pub const X86_64_RELOC_SIGNED_4: u8 = 8;
/// x86_64: thread-local variable.
pub const X86_64_RELOC_TLV: u8 = 9;

/// arm64: absolute address.
pub const ARM64_RELOC_UNSIGNED: u8 = 0;
/// arm64: must be followed by an `ARM64_RELOC_UNSIGNED`.
pub const ARM64_RELOC_SUBTRACTOR: u8 = 1;
/// arm64: `b`/`bl` 26-bit displacement.
pub const ARM64_RELOC_BRANCH26: u8 = 2;
/// arm64: `adrp` page of the target.
pub const ARM64_RELOC_PAGE21: u8 = 3;
/// arm64: offset within the page, scaled by the access size.
pub const ARM64_RELOC_PAGEOFF12: u8 = 4;
/// arm64: `adrp` page of the target's GOT entry.
pub const ARM64_RELOC_GOT_LOAD_PAGE21: u8 = 5;
/// arm64: offset of the GOT entry within its page.
pub const ARM64_RELOC_GOT_LOAD_PAGEOFF12: u8 = 6;
/// arm64: pointer to the target's GOT entry.
pub const ARM64_RELOC_POINTER_TO_GOT: u8 = 7;
/// arm64: `adrp` page of the TLV descriptor.
pub const ARM64_RELOC_TLVP_LOAD_PAGE21: u8 = 8;
/// arm64: offset of the TLV descriptor within its page.
pub const ARM64_RELOC_TLVP_LOAD_PAGEOFF12: u8 = 9;
/// arm64: explicit addend for the following relocation.
pub const ARM64_RELOC_ADDEND: u8 = 10;
/// arm64e: authenticated pointer.
pub const ARM64_RELOC_AUTHENTICATED_POINTER: u8 = 11;

/// 32-bit Arm: vanilla.
pub const ARM_RELOC_VANILLA: u8 = 0;
/// 32-bit Arm: second half of a pair.
pub const ARM_RELOC_PAIR: u8 = 1;
/// 32-bit Arm: section difference.
pub const ARM_RELOC_SECTDIFF: u8 = 2;
/// 32-bit Arm: local section difference.
pub const ARM_RELOC_LOCAL_SECTDIFF: u8 = 3;
/// 32-bit Arm: prebound lazy pointer.
pub const ARM_RELOC_PB_LA_PTR: u8 = 4;
/// 32-bit Arm: 24-bit branch.
pub const ARM_RELOC_BR24: u8 = 5;
/// 32-bit Arm: Thumb 22-bit branch.
pub const ARM_THUMB_RELOC_BR22: u8 = 6;
/// 32-bit Arm: Thumb 32-bit branch (obsolete).
pub const ARM_THUMB_32BIT_BRANCH: u8 = 7;
/// 32-bit Arm: `movw`/`movt` half, followed by a pair.
pub const ARM_RELOC_HALF: u8 = 8;
/// 32-bit Arm: `movw`/`movt` section difference half, followed by a pair.
pub const ARM_RELOC_HALF_SECTDIFF: u8 = 9;

/// Name of an x86_64 relocation type.
#[must_use]
pub fn x86_64_reloc_name(r_type: u8) -> Option<&'static str> {
    Some(match r_type {
        X86_64_RELOC_UNSIGNED => "X86_64_RELOC_UNSIGNED",
        X86_64_RELOC_SIGNED => "X86_64_RELOC_SIGNED",
        X86_64_RELOC_BRANCH => "X86_64_RELOC_BRANCH",
        X86_64_RELOC_GOT_LOAD => "X86_64_RELOC_GOT_LOAD",
        X86_64_RELOC_GOT => "X86_64_RELOC_GOT",
        X86_64_RELOC_SUBTRACTOR => "X86_64_RELOC_SUBTRACTOR",
        X86_64_RELOC_SIGNED_1 => "X86_64_RELOC_SIGNED_1",
        X86_64_RELOC_SIGNED_2 => "X86_64_RELOC_SIGNED_2",
        X86_64_RELOC_SIGNED_4 => "X86_64_RELOC_SIGNED_4",
        X86_64_RELOC_TLV => "X86_64_RELOC_TLV",
        _ => return None,
    })
}

/// Name of an arm64 relocation type.
#[must_use]
pub fn arm64_reloc_name(r_type: u8) -> Option<&'static str> {
    Some(match r_type {
        ARM64_RELOC_UNSIGNED => "ARM64_RELOC_UNSIGNED",
        ARM64_RELOC_SUBTRACTOR => "ARM64_RELOC_SUBTRACTOR",
        ARM64_RELOC_BRANCH26 => "ARM64_RELOC_BRANCH26",
        ARM64_RELOC_PAGE21 => "ARM64_RELOC_PAGE21",
        ARM64_RELOC_PAGEOFF12 => "ARM64_RELOC_PAGEOFF12",
        ARM64_RELOC_GOT_LOAD_PAGE21 => "ARM64_RELOC_GOT_LOAD_PAGE21",
        ARM64_RELOC_GOT_LOAD_PAGEOFF12 => "ARM64_RELOC_GOT_LOAD_PAGEOFF12",
        ARM64_RELOC_POINTER_TO_GOT => "ARM64_RELOC_POINTER_TO_GOT",
        ARM64_RELOC_TLVP_LOAD_PAGE21 => "ARM64_RELOC_TLVP_LOAD_PAGE21",
        ARM64_RELOC_TLVP_LOAD_PAGEOFF12 => "ARM64_RELOC_TLVP_LOAD_PAGEOFF12",
        ARM64_RELOC_ADDEND => "ARM64_RELOC_ADDEND",
        ARM64_RELOC_AUTHENTICATED_POINTER => "ARM64_RELOC_AUTHENTICATED_POINTER",
        _ => return None,
    })
}

/// Name of a generic (i386) relocation type.
#[must_use]
pub fn generic_reloc_name(r_type: u8) -> Option<&'static str> {
    Some(match r_type {
        GENERIC_RELOC_VANILLA => "GENERIC_RELOC_VANILLA",
        GENERIC_RELOC_PAIR => "GENERIC_RELOC_PAIR",
        GENERIC_RELOC_SECTDIFF => "GENERIC_RELOC_SECTDIFF",
        GENERIC_RELOC_PB_LA_PTR => "GENERIC_RELOC_PB_LA_PTR",
        GENERIC_RELOC_LOCAL_SECTDIFF => "GENERIC_RELOC_LOCAL_SECTDIFF",
        GENERIC_RELOC_TLV => "GENERIC_RELOC_TLV",
        _ => return None,
    })
}

/// Name of a 32-bit Arm relocation type.
#[must_use]
pub fn arm_reloc_name(r_type: u8) -> Option<&'static str> {
    Some(match r_type {
        ARM_RELOC_VANILLA => "ARM_RELOC_VANILLA",
        ARM_RELOC_PAIR => "ARM_RELOC_PAIR",
        ARM_RELOC_SECTDIFF => "ARM_RELOC_SECTDIFF",
        ARM_RELOC_LOCAL_SECTDIFF => "ARM_RELOC_LOCAL_SECTDIFF",
        ARM_RELOC_PB_LA_PTR => "ARM_RELOC_PB_LA_PTR",
        ARM_RELOC_BR24 => "ARM_RELOC_BR24",
        ARM_THUMB_RELOC_BR22 => "ARM_THUMB_RELOC_BR22",
        ARM_THUMB_32BIT_BRANCH => "ARM_THUMB_32BIT_BRANCH",
        ARM_RELOC_HALF => "ARM_RELOC_HALF",
        ARM_RELOC_HALF_SECTDIFF => "ARM_RELOC_HALF_SECTDIFF",
        _ => return None,
    })
}

/// Name of a relocation type for a CPU type, such as
/// `"ARM64_RELOC_BRANCH26"`.
#[must_use]
pub fn reloc_name(cpu_type: u32, r_type: u8) -> Option<&'static str> {
    match cpu_type {
        CPU_TYPE_X86_64 => x86_64_reloc_name(r_type),
        CPU_TYPE_ARM64 | CPU_TYPE_ARM64_32 => arm64_reloc_name(r_type),
        CPU_TYPE_ARM => arm_reloc_name(r_type),
        CPU_TYPE_X86 => generic_reloc_name(r_type),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Data in code
// ---------------------------------------------------------------------------

/// Data in code: data.
pub const DICE_KIND_DATA: u16 = 1;
/// Data in code: 8-bit jump table.
pub const DICE_KIND_JUMP_TABLE8: u16 = 2;
/// Data in code: 16-bit jump table.
pub const DICE_KIND_JUMP_TABLE16: u16 = 3;
/// Data in code: 32-bit jump table.
pub const DICE_KIND_JUMP_TABLE32: u16 = 4;
/// Data in code: 32-bit absolute jump table.
pub const DICE_KIND_ABS_JUMP_TABLE32: u16 = 5;

// ---------------------------------------------------------------------------
// Compact unwind encodings
// ---------------------------------------------------------------------------

/// The entry does not start a function (continuation of the previous one).
pub const UNWIND_IS_NOT_FUNCTION_START: u32 = 0x8000_0000;
/// The function has an LSDA.
pub const UNWIND_HAS_LSDA: u32 = 0x4000_0000;
/// Personality index mask.
pub const UNWIND_PERSONALITY_MASK: u32 = 0x3000_0000;
/// Architecture-specific mode mask.
pub const UNWIND_MODE_MASK: u32 = 0x0f00_0000;
/// x86_64: RBP frame.
pub const UNWIND_X86_64_MODE_RBP_FRAME: u32 = 0x0100_0000;
/// x86_64: frameless, immediate stack size.
pub const UNWIND_X86_64_MODE_STACK_IMMD: u32 = 0x0200_0000;
/// x86_64: frameless, indirect stack size.
pub const UNWIND_X86_64_MODE_STACK_IND: u32 = 0x0300_0000;
/// x86_64: fall back to DWARF (`__eh_frame`).
pub const UNWIND_X86_64_MODE_DWARF: u32 = 0x0400_0000;
/// arm64: frameless.
pub const UNWIND_ARM64_MODE_FRAMELESS: u32 = 0x0200_0000;
/// arm64: fall back to DWARF (`__eh_frame`).
pub const UNWIND_ARM64_MODE_DWARF: u32 = 0x0300_0000;
/// arm64: standard frame.
pub const UNWIND_ARM64_MODE_FRAME: u32 = 0x0400_0000;

// ---------------------------------------------------------------------------
// Export trie
// ---------------------------------------------------------------------------

/// Mask of the symbol kind in export flags.
pub const EXPORT_SYMBOL_FLAGS_KIND_MASK: u64 = 0x03;
/// Regular export.
pub const EXPORT_SYMBOL_FLAGS_KIND_REGULAR: u64 = 0x00;
/// Thread-local export.
pub const EXPORT_SYMBOL_FLAGS_KIND_THREAD_LOCAL: u64 = 0x01;
/// Absolute export.
pub const EXPORT_SYMBOL_FLAGS_KIND_ABSOLUTE: u64 = 0x02;
/// Weak definition.
pub const EXPORT_SYMBOL_FLAGS_WEAK_DEFINITION: u64 = 0x04;
/// Re-exported from another dylib.
pub const EXPORT_SYMBOL_FLAGS_REEXPORT: u64 = 0x08;
/// Stub with a resolver function.
pub const EXPORT_SYMBOL_FLAGS_STUB_AND_RESOLVER: u64 = 0x10;
/// Static resolver.
pub const EXPORT_SYMBOL_FLAGS_STATIC_RESOLVER: u64 = 0x20;

// ---------------------------------------------------------------------------
// Chained fixups
// ---------------------------------------------------------------------------

/// Imports are `dyld_chained_import` (32-bit).
pub const DYLD_CHAINED_IMPORT: u32 = 1;
/// Imports are `dyld_chained_import_addend` (32-bit plus addend).
pub const DYLD_CHAINED_IMPORT_ADDEND: u32 = 2;
/// Imports are `dyld_chained_import_addend64`.
pub const DYLD_CHAINED_IMPORT_ADDEND64: u32 = 3;
/// Symbol names are uncompressed.
pub const DYLD_CHAINED_SYMBOL_UNCOMPRESSED: u32 = 0;
/// Symbol names are zlib-compressed.
pub const DYLD_CHAINED_SYMBOL_ZLIB: u32 = 1;
/// `dyld_chained_ptr_64_rebase` / `_bind` with unslid address targets.
pub const DYLD_CHAINED_PTR_64: u16 = 2;
/// `dyld_chained_ptr_64_rebase` / `_bind` with image-relative targets.
pub const DYLD_CHAINED_PTR_64_OFFSET: u16 = 6;
/// A page of `dyld_chained_starts_in_segment` with no fixups.
pub const DYLD_CHAINED_PTR_START_NONE: u16 = 0xffff;

// ---------------------------------------------------------------------------
// Binding and rebasing (`LC_DYLD_INFO`)
// ---------------------------------------------------------------------------

/// Ordinal of the image itself.
pub const BIND_SPECIAL_DYLIB_SELF: i32 = 0;
/// Ordinal of the main executable (bundles with `-bundle_loader`).
pub const BIND_SPECIAL_DYLIB_MAIN_EXECUTABLE: i32 = -1;
/// Flat lookup in every loaded image (`-undefined dynamic_lookup`).
pub const BIND_SPECIAL_DYLIB_FLAT_LOOKUP: i32 = -2;
/// Weak definition coalescing lookup.
pub const BIND_SPECIAL_DYLIB_WEAK_LOOKUP: i32 = -3;
/// `REBASE_TYPE_POINTER`.
pub const REBASE_TYPE_POINTER: u8 = 1;
/// `REBASE_OPCODE_DONE`.
pub const REBASE_OPCODE_DONE: u8 = 0x00;
/// `REBASE_OPCODE_SET_TYPE_IMM`.
pub const REBASE_OPCODE_SET_TYPE_IMM: u8 = 0x10;
/// `REBASE_OPCODE_SET_SEGMENT_AND_OFFSET_ULEB`.
pub const REBASE_OPCODE_SET_SEGMENT_AND_OFFSET_ULEB: u8 = 0x20;
/// `REBASE_OPCODE_ADD_ADDR_ULEB`.
pub const REBASE_OPCODE_ADD_ADDR_ULEB: u8 = 0x30;
/// `REBASE_OPCODE_DO_REBASE_IMM_TIMES`.
pub const REBASE_OPCODE_DO_REBASE_IMM_TIMES: u8 = 0x50;
/// `REBASE_OPCODE_DO_REBASE_ULEB_TIMES`.
pub const REBASE_OPCODE_DO_REBASE_ULEB_TIMES: u8 = 0x60;
/// `BIND_TYPE_POINTER`.
pub const BIND_TYPE_POINTER: u8 = 1;
/// `BIND_SYMBOL_FLAGS_WEAK_IMPORT`.
pub const BIND_SYMBOL_FLAGS_WEAK_IMPORT: u8 = 0x1;
/// `BIND_OPCODE_DONE`.
pub const BIND_OPCODE_DONE: u8 = 0x00;
/// `BIND_OPCODE_SET_DYLIB_ORDINAL_IMM`.
pub const BIND_OPCODE_SET_DYLIB_ORDINAL_IMM: u8 = 0x10;
/// `BIND_OPCODE_SET_DYLIB_ORDINAL_ULEB`.
pub const BIND_OPCODE_SET_DYLIB_ORDINAL_ULEB: u8 = 0x20;
/// `BIND_OPCODE_SET_DYLIB_SPECIAL_IMM`.
pub const BIND_OPCODE_SET_DYLIB_SPECIAL_IMM: u8 = 0x30;
/// `BIND_OPCODE_SET_SYMBOL_TRAILING_FLAGS_IMM`.
pub const BIND_OPCODE_SET_SYMBOL_TRAILING_FLAGS_IMM: u8 = 0x40;
/// `BIND_OPCODE_SET_TYPE_IMM`.
pub const BIND_OPCODE_SET_TYPE_IMM: u8 = 0x50;
/// `BIND_OPCODE_SET_ADDEND_SLEB`.
pub const BIND_OPCODE_SET_ADDEND_SLEB: u8 = 0x60;
/// `BIND_OPCODE_SET_SEGMENT_AND_OFFSET_ULEB`.
pub const BIND_OPCODE_SET_SEGMENT_AND_OFFSET_ULEB: u8 = 0x70;
/// `BIND_OPCODE_DO_BIND`.
pub const BIND_OPCODE_DO_BIND: u8 = 0x90;
