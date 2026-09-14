//! Input files: mapping, format identification and archives.
//!
//! **Workstream W2.** This module owns stages 2 and 3 of the pipeline in
//! `docs/architecture.md`: it turns input specifications into bytes, and the
//! bytes into a format identity.
//!
//! - [`identify()`] recognizes a file by its magic: ELF, COFF objects and short
//!   import libraries, PE images, Mach-O, fat binaries, `!<arch>`, `!<thin>`,
//!   LLVM bitcode and text. GCC LTO IR is an ELF file with `.gnu.lto_*`
//!   sections, which this module does not parse; the ELF reader plugs in a
//!   [`GccLtoProbe`] instead.
//! - [`Archive`] reads `ar` archives in the GNU, BSD and thin variants, with
//!   the SysV (`/`, `/SYM64/`) and BSD (`__.SYMDEF`, `__.SYMDEF_64`) symbol
//!   indexes. Members are not parsed here: the reader yields their names and
//!   byte ranges (or paths, for thin archives), and the caller parses them
//!   lazily when resolution extracts them.
//!
//! Everything here treats input as untrusted: no panics, no unchecked
//! arithmetic on offsets, and every malformed structure becomes
//! [`crate::Error::Malformed`].

#![deny(clippy::arithmetic_side_effects)]

pub mod archive;
pub mod identify;
mod read;

pub use archive::{
    Archive, ArchiveKind, ArchiveSymbol, Member, MemberData, SymbolIndex, SymbolIndexKind,
};
pub use identify::{FileFormat, GccLtoProbe, identify, identify_with};
