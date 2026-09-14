//! Input files: mapping, format identification and archives.
//!
//! **Workstream W2.** Responsibilities:
//!
//! - Map input files (`memmap2`), with a read fallback for small or unmappable
//!   files, and hold them for the lifetime of the link.
//! - Identify each file by magic: ELF, COFF, Mach-O, fat, `!<arch>`,
//!   `!<thin>`, LLVM bitcode, GCC LTO IR, or text (a linker script or `.tbd`).
//! - Read `ar` archives in the GNU, BSD and thin variants: the member table,
//!   long names, and the symbol index (armap), including the 64-bit variant.
//!   Members are parsed lazily, when resolution extracts them.
//!
//! Mapping needs `unsafe`; it must stay inside this module, with a `SAFETY`
//! comment. Everything here must treat input as untrusted: no panics, no
//! unchecked arithmetic on offsets. See `docs/architecture.md`.
