//! Zero-copy ELF reading.
//!
//! **Workstream W7.** This module parses relocatable objects and shared
//! objects straight out of the input mapping:
//!
//! - [`format`](mod@format): the [`ElfFormat`] trait and its four instantiations
//!   ([`Elf64Le`], [`Elf64Be`], [`Elf32Le`], [`Elf32Be`]), plus [`ElfKind`]
//!   for run-time identification. Every reader is generic over the format,
//!   so hot loops are monomorphized and never branch on class or byte order.
//! - [`ElfFile`]: header validation, section and program header tables
//!   (with extended section numbering), section names and contents.
//! - [`ObjectFile`]: `ET_REL` objects — symbols ([`SymbolTable`]),
//!   relocations ([`RelaSlice`], [`RelSlice`], [`RelrSlice`]), groups
//!   ([`Group`]), GNU properties ([`GnuProperties`]), compression headers,
//!   `.eh_frame` splitting ([`split_eh_frame`]) and GCC LTO detection.
//! - [`SharedObject`]: `ET_DYN` objects — `.dynamic`, `DT_SONAME`,
//!   `DT_NEEDED`, `.dynsym` and symbol versions.
//! - [`consts`]: ELF ABI constants and relocation type names.
//!
//! # Safety and robustness
//!
//! Input is untrusted. Records are byte arrays of alignment 1, decoded with
//! `from_le_bytes`/`from_be_bytes`; there is no `unsafe` and no pointer
//! cast. Every offset and size is bounds-checked with checked arithmetic
//! (this module denies `clippy::arithmetic_side_effects`), and problems are
//! reported as [`Error::Malformed`](crate::Error::Malformed) naming the file
//! and offset.
//!
//! # Performance model
//!
//! Parsing an object reads its section headers once and allocates nothing.
//! Symbols and relocations are decoded one entry at a time by iterators over
//! the mapped bytes. Symbol names are returned as `&'a [u8]` slices, ready
//! for prehashed interning by the caller.
//!
//! # Limitations
//!
//! - MIPS64 little-endian relocations, whose `r_info` layout differs from
//!   every other target, are decoded with the generic layout.
//! - Shared objects are read through their section headers; without them
//!   only the dynamic array (via `PT_DYNAMIC`) is available.

#![deny(clippy::arithmetic_side_effects)]

pub mod consts;
pub mod dynamic;
pub mod eh_frame;
pub mod file;
pub mod format;
pub mod group;
pub mod header;
pub mod note;
pub mod object;
pub mod reloc;
pub mod section;
pub mod segment;
pub mod source;
pub mod strtab;
pub mod symbol;

pub use dynamic::{DynEntry, DynamicSymbol, SharedObject, SymbolVersion, VersionInfo, VersionKind};
pub use eh_frame::{EhFrameEntry, EhFrameIter, EhFrameRecord, EhFrameRecordKind, split_eh_frame};
pub use file::ElfFile;
pub use format::{
    Big, Elf32, Elf32Be, Elf32Le, Elf64, Elf64Be, Elf64Le, ElfFormat, ElfKind, Endian, Little,
    RawRecord,
};
pub use group::Group;
pub use header::{FileHeader, architecture};
pub use note::{GnuProperties, GnuProperty, GnuPropertyIter, Note, NoteIter};
pub use object::{GCC_LTO_SECTION_PREFIX, ObjectFile};
pub use reloc::{
    RelIter, RelSlice, RelaIter, RelaSlice, Relocation, RelocationSection, Relocations, RelrIter,
    RelrSlice,
};
pub use section::{CompressionHeader, SectionHeader, SectionIter, SectionTable};
pub use segment::{ProgramHeader, ProgramHeaderTable};
pub use source::Source;
pub use strtab::StringTable;
pub use symbol::{RawSymbol, SectionIndex, Symbol, SymbolIter, SymbolTable};
