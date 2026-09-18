//! DWARF handling.
//!
//! **Workstream W10.** Roadmap M1 (relocation and tombstones) through M5
//! (compression, indexes). Scope: `.debug_*` relocation with the right
//! tombstone values, zlib and zstd compression and decompression,
//! `--gdb-index` and `--debug-names`, and the lazy line-table lookup that
//! turns an input offset into `file:line` for a diagnostic. See
//! `docs/formats.md` ("DWARF and debug information").
//!
//! Submodules:
//!
//! - [`compress`]: zlib/DEFLATE and Zstandard codecs, implemented in-crate
//!   (decoders and parallel, chunked encoders).
//! - [`section`]: `SHF_COMPRESSED` and `.zdebug_*` input sections, and
//!   compressed output sections.
//! - [`tombstone`]: values for relocations whose target was discarded.
//! - [`dwarf`]: lazy `file:line` lookup for diagnostics.
//! - [`gdb_index`]: `--gdb-index` (W29), and the object DWARF reader the
//!   index builders share.
//!
//! # Using it from a format backend
//!
//! - **Compressed inputs.** For each input section that survives GC, call
//!   [`section::CompressedSection::detect`]; if it returns a section,
//!   decompress it (one rayon task per section) with
//!   [`decompress_into`](section::CompressedSection::decompress_into)
//!   straight into the output buffer, or with
//!   [`decompress`](section::CompressedSection::decompress). Relocation
//!   offsets refer to the decompressed contents, and `.zdebug_*` names
//!   become `.debug_*` ([`section::decompressed_name`]).
//! - **Dead relocation targets.** Build one [`tombstone::Tombstones`] per
//!   link from the style and the `-z dead-reloc-in-nonalloc=` rules, call
//!   [`for_section`](tombstone::Tombstones::for_section) per non-allocated
//!   input section, and write the value (truncated to the field) for
//!   relocations whose target section was discarded or folded.
//! - **Compressed outputs.** [`section::compress_section`] turns a debug
//!   output section's bytes into `SHF_COMPRESSED` contents.
//! - **Diagnostics.** When reporting a problem at an input position, build
//!   a [`dwarf::LineLookup`] for the object (once, lazily) and call
//!   [`find`](dwarf::LineLookup::find) to fill
//!   [`Location::source`](crate::diag::Location::source).
//!
//! Everything that reads input denies `clippy::arithmetic_side_effects`
//! and reports malformed data as errors; the two encoders, which only
//! process buffers they build, opt out locally.

#![deny(clippy::arithmetic_side_effects)]

pub mod compress;
pub mod dwarf;
pub mod gdb_index;
pub mod section;
pub mod tombstone;
