//! DWARF handling.
//!
//! Roadmap M1 (relocation and tombstones) through M5 (compression, indexes).
//! Scope: `.debug_*` relocation with the right tombstone values, zlib and zstd
//! compression and decompression, `--gdb-index` and `--debug-names`, and the
//! lazy line-table lookup that turns an input offset into `file:line` for a
//! diagnostic. See `docs/formats.md` ("DWARF and debug information").
