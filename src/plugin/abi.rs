//! The binary interface between a linker and an LTO plugin.
//!
//! Everything here is an interoperability fact: the numeric values, record
//! layouts and function signatures that GCC's `liblto_plugin.so`, LLVM's
//! `LLVMgold.so` and the GNU linkers agree on. The bindings are written for
//! qld from that ABI; they are not a translation of any header.
//!
//! The interface is plain C: every enumeration crosses the boundary as a C
//! `int`, every callback uses the platform's C calling convention, and a
//! plugin exports a single entry point, `onload`, which receives a
//! null-terminated array of tagged values (the *transfer vector*).

// Some items exist only to document the ABI or are used on some hosts only.
#![allow(dead_code)]

use core::ffi::{c_char, c_int, c_uint, c_void};

/// Status code returned by nearly every function on both sides.
pub(crate) type Status = c_int;

/// Success.
pub(crate) const STATUS_OK: Status = 0;
/// The file has no symbols to report (or was not selected for the link).
pub(crate) const STATUS_NO_SYMS: Status = 1;
/// The handle does not name a file this linker knows.
pub(crate) const STATUS_BAD_HANDLE: Status = 2;
/// Any other failure.
pub(crate) const STATUS_ERR: Status = 3;

/// Value of the `API_VERSION` tag: the only version of the vector format.
pub(crate) const API_VERSION: c_int = 1;

/// Version number qld reports through the `GNU_LD_VERSION` tag, encoded as
/// `major * 100 + minor` (binutils 2.44).
///
/// Plugins use it only to adapt to old GNU ld bugs. qld deliberately does not
/// send `GOLD_VERSION`: GCC's plugin treats its presence as "running under
/// gold" and then expects gold's handling of `-pass-through` libraries.
pub(crate) const GNU_LD_VERSION: c_int = 244;

/// Negotiated API levels (`get_api_version`).
pub(crate) const LINKER_API_V0: c_int = 0;
/// Level 1: the linker offers `get_symbols_v3` and `add_symbols_v2`, and
/// `add_symbols` may be called from several threads at once.
pub(crate) const LINKER_API_V1: c_int = 1;

/// Tags of the transfer vector entries.
pub(crate) mod tag {
    use core::ffi::c_int;

    pub(crate) const NULL: c_int = 0;
    pub(crate) const API_VERSION: c_int = 1;
    pub(crate) const GOLD_VERSION: c_int = 2;
    pub(crate) const LINKER_OUTPUT: c_int = 3;
    pub(crate) const OPTION: c_int = 4;
    pub(crate) const REGISTER_CLAIM_FILE_HOOK: c_int = 5;
    pub(crate) const REGISTER_ALL_SYMBOLS_READ_HOOK: c_int = 6;
    pub(crate) const REGISTER_CLEANUP_HOOK: c_int = 7;
    pub(crate) const ADD_SYMBOLS: c_int = 8;
    pub(crate) const GET_SYMBOLS: c_int = 9;
    pub(crate) const ADD_INPUT_FILE: c_int = 10;
    pub(crate) const MESSAGE: c_int = 11;
    pub(crate) const GET_INPUT_FILE: c_int = 12;
    pub(crate) const RELEASE_INPUT_FILE: c_int = 13;
    pub(crate) const ADD_INPUT_LIBRARY: c_int = 14;
    pub(crate) const OUTPUT_NAME: c_int = 15;
    pub(crate) const SET_EXTRA_LIBRARY_PATH: c_int = 16;
    pub(crate) const GNU_LD_VERSION: c_int = 17;
    pub(crate) const GET_VIEW: c_int = 18;
    pub(crate) const GET_INPUT_SECTION_COUNT: c_int = 19;
    pub(crate) const GET_INPUT_SECTION_TYPE: c_int = 20;
    pub(crate) const GET_INPUT_SECTION_NAME: c_int = 21;
    pub(crate) const GET_INPUT_SECTION_CONTENTS: c_int = 22;
    pub(crate) const UPDATE_SECTION_ORDER: c_int = 23;
    pub(crate) const ALLOW_SECTION_ORDERING: c_int = 24;
    pub(crate) const GET_SYMBOLS_V2: c_int = 25;
    pub(crate) const ALLOW_UNIQUE_SEGMENT_FOR_SECTIONS: c_int = 26;
    pub(crate) const UNIQUE_SEGMENT_FOR_SECTIONS: c_int = 27;
    pub(crate) const GET_SYMBOLS_V3: c_int = 28;
    pub(crate) const GET_INPUT_SECTION_ALIGNMENT: c_int = 29;
    pub(crate) const GET_INPUT_SECTION_SIZE: c_int = 30;
    pub(crate) const REGISTER_NEW_INPUT_HOOK: c_int = 31;
    pub(crate) const GET_WRAP_SYMBOLS: c_int = 32;
    pub(crate) const ADD_SYMBOLS_V2: c_int = 33;
    pub(crate) const GET_API_VERSION: c_int = 34;
    pub(crate) const REGISTER_CLAIM_FILE_HOOK_V2: c_int = 35;
}

/// C `off_t`. Plugins for 32-bit hosts are built with large-file support, so
/// it is 64 bits everywhere qld runs plugins.
pub(crate) type OffT = i64;

/// An input file as the plugin sees it (`struct ld_plugin_input_file`).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct RawInputFile {
    /// NUL-terminated path; for an archive member, the archive's path.
    pub(crate) name: *const c_char,
    /// An open descriptor for `name`.
    pub(crate) fd: c_int,
    /// Offset of the file's bytes within `name`.
    pub(crate) offset: OffT,
    /// Number of bytes.
    pub(crate) filesize: OffT,
    /// Opaque linker handle, passed back in callbacks.
    pub(crate) handle: *mut c_void,
}

/// One symbol record (`struct ld_plugin_symbol`).
///
/// The four one-byte fields replaced a single `int def` in older versions of
/// the interface, so their order follows the host's byte order: `def` keeps
/// the position of that integer's low byte.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct RawSymbol {
    pub(crate) name: *mut c_char,
    pub(crate) version: *mut c_char,
    #[cfg(target_endian = "little")]
    pub(crate) def: u8,
    #[cfg(target_endian = "little")]
    pub(crate) symbol_type: u8,
    #[cfg(target_endian = "little")]
    pub(crate) section_kind: u8,
    #[cfg(target_endian = "little")]
    pub(crate) unused: u8,
    #[cfg(target_endian = "big")]
    pub(crate) unused: u8,
    #[cfg(target_endian = "big")]
    pub(crate) section_kind: u8,
    #[cfg(target_endian = "big")]
    pub(crate) symbol_type: u8,
    #[cfg(target_endian = "big")]
    pub(crate) def: u8,
    pub(crate) visibility: c_int,
    pub(crate) size: u64,
    pub(crate) comdat_key: *mut c_char,
    pub(crate) resolution: c_int,
}

/// A section of an input file (`struct ld_plugin_section`), passed by value.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct RawSection {
    pub(crate) handle: *const c_void,
    pub(crate) shndx: c_uint,
}

/// The value half of a transfer vector entry.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) union TvValue {
    /// Integer tags.
    pub(crate) val: c_int,
    /// String tags.
    pub(crate) string: *const c_char,
    /// Callback tags: a function pointer, stored type-erased. Every function
    /// pointer has the size of a data pointer on the hosts qld supports.
    pub(crate) function: *const c_void,
}

/// One transfer vector entry (`struct ld_plugin_tv`).
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct Tv {
    pub(crate) tag: c_int,
    pub(crate) value: TvValue,
}

/// The plugin's entry point.
pub(crate) type OnloadFn = unsafe extern "C" fn(tv: *mut Tv) -> Status;
/// Claim-file handler, first version.
pub(crate) type ClaimFileFn =
    unsafe extern "C" fn(file: *const RawInputFile, claimed: *mut c_int) -> Status;
/// Claim-file handler, second version: `known_used` says whether the linker
/// already knows the file is part of the link (it is not a speculative look at
/// an archive member).
pub(crate) type ClaimFileV2Fn = unsafe extern "C" fn(
    file: *const RawInputFile,
    claimed: *mut c_int,
    known_used: c_int,
) -> Status;
/// All-symbols-read handler and cleanup handler.
pub(crate) type VoidHandlerFn = unsafe extern "C" fn() -> Status;
/// New-input handler.
pub(crate) type NewInputFn = unsafe extern "C" fn(file: *const RawInputFile) -> Status;

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::{align_of, offset_of, size_of};

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn layouts_match_the_lp64_abi() {
        assert_eq!(size_of::<RawInputFile>(), 40);
        assert_eq!(offset_of!(RawInputFile, fd), 8);
        assert_eq!(offset_of!(RawInputFile, offset), 16);
        assert_eq!(offset_of!(RawInputFile, filesize), 24);
        assert_eq!(offset_of!(RawInputFile, handle), 32);

        assert_eq!(size_of::<RawSymbol>(), 48);
        assert_eq!(offset_of!(RawSymbol, version), 8);
        assert_eq!(offset_of!(RawSymbol, visibility), 20);
        assert_eq!(offset_of!(RawSymbol, size), 24);
        assert_eq!(offset_of!(RawSymbol, comdat_key), 32);
        assert_eq!(offset_of!(RawSymbol, resolution), 40);

        assert_eq!(size_of::<RawSection>(), 16);
        assert_eq!(size_of::<Tv>(), 16);
        assert_eq!(align_of::<Tv>(), 8);
        assert_eq!(offset_of!(Tv, value), 8);
    }

    #[test]
    #[cfg(target_endian = "little")]
    fn def_byte_overlays_the_old_int_field() {
        assert_eq!(offset_of!(RawSymbol, def), 16);
        assert_eq!(offset_of!(RawSymbol, symbol_type), 17);
        assert_eq!(offset_of!(RawSymbol, section_kind), 18);
    }
}
