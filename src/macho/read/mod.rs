//! Zero-copy Mach-O reading.
//!
//! **Workstream W15.** This module parses the inputs of an ld64-flavor link
//! straight out of the input mapping:
//!
//! - [`FatFile`]: universal binaries, and slice selection by architecture
//!   ([`Arch`]) with an error listing the available slices.
//! - [`MachOFile`]: the header and load commands of any Mach-O file, with
//!   typed decoders in [`commands`].
//! - [`ObjectFile`]: `MH_OBJECT` files — sections ([`Section`]), symbols
//!   ([`SymbolTable`]), relocations ([`RelocationTable`], paired into
//!   [`PairedRelocation`]s), `LC_BUILD_VERSION`, `LC_LINKER_OPTION`,
//!   `LC_DATA_IN_CODE` and `LC_LINKER_OPTIMIZATION_HINT`.
//! - [`atoms`]: splitting sections into atoms for dead stripping
//!   (`MH_SUBSECTIONS_VIA_SYMBOLS`, literal sections, `__compact_unwind`).
//! - [`unwind`]: `__LD,__compact_unwind` entries; [`eh_frame`]: CIE and FDE
//!   records of `__TEXT,__eh_frame`.
//! - [`Dylib`]: `MH_DYLIB` and `MH_DYLIB_STUB` files — install name,
//!   dependencies, run paths, the export trie ([`ExportTrieIter`]) and the
//!   imports of `LC_DYLD_CHAINED_FIXUPS` ([`chained`]).
//! - [`tbd`]: text-based stubs, v3 and v4 (YAML) and v5 (JSON).
//! - [`consts`]: ABI constants and relocation type names.
//!
//! # Safety and robustness
//!
//! Input is untrusted. Records are decoded field by field with
//! `from_le_bytes`/`from_be_bytes`; there is no `unsafe`. Every offset and
//! size is bounds-checked with checked arithmetic (this module denies
//! `clippy::arithmetic_side_effects`), and problems are reported as
//! [`Error::Malformed`](crate::Error::Malformed) naming the file and offset
//! (text stubs report byte offsets too).
//!
//! # Byte order and word size
//!
//! Unlike the ELF reader, these readers are not generic over the format:
//! the byte order and the 32/64-bit layout are run-time flags. Mach-O inputs
//! are 64-bit little-endian in practice, so the branches are perfectly
//! predictable, and a non-generic `ObjectFile<'a>` is simpler for the linker
//! to store. 32-bit and big-endian files decode through the same code.

#![deny(clippy::arithmetic_side_effects)]

pub mod arch;
pub mod atoms;
pub mod bytes;
pub mod chained;
pub mod commands;
pub mod consts;
pub mod dylib;
pub mod eh_frame;
pub mod fat;
pub mod file;
pub mod object;
pub mod reloc;
pub mod section;
pub mod symbol;
pub mod trie;
pub mod unwind;

pub use arch::Arch;
pub use atoms::{Atom, AtomKind, AtomRelocation, Atomization};
pub use bytes::{Endian, Source};
pub use chained::{ChainedFixups, ChainedFixupsHeader, ChainedImport};
pub use commands::{
    BuildVersion, DataInCodeEntry, DyldInfoCommand, DylibCommand, DylibLoadKind, DysymtabCommand,
    LinkeditData, LinkerOptionHint, LoadCommand, LoadCommandIter, OptimizationHint,
    OptimizationHintIter, PackedVersion, Segment, SymtabCommand,
};
pub use dylib::{Dylib, DylibDependency};
pub use eh_frame::{Cie, EhFrame, EhFrameInput, EhFrameKind, EhFrameRecord, EhPointer, Fde};
pub use fat::{FatFile, FatSlice};
pub use file::{MachHeader, MachOFile};
pub use object::ObjectFile;
pub use reloc::{
    PairedRelocation, PairedRelocationIter, Relocation, RelocationTable, RelocationTarget,
};
pub use section::{LiteralKind, Section, SectionTable};
pub use symbol::{Symbol, SymbolIter, SymbolKind, SymbolTable};
pub use trie::{Export, ExportTarget, ExportTrieIter};
pub use unwind::{CompactUnwindEntry, UnwindField, compact_unwind_entries};
