//! The one `unsafe` operation of the writer: mapping the output file.
//!
//! Kept in its own module so the `unsafe_code` allowance covers nothing else.

#![allow(unsafe_code)]

use memmap2::MmapMut;
use std::fs::File;
use std::io;

/// Maps the first `len` bytes of `file` writable and shared.
///
/// `len` must be non-zero and the file must already be at least `len` bytes
/// long (the caller has just called `set_len`).
pub(super) fn map_output(file: &File, len: usize) -> io::Result<MmapMut> {
    // SAFETY: memmap2 requires that the mapped file is not truncated or
    // modified by anyone else while the mapping is alive, otherwise accesses
    // can fault (SIGBUS) or observe torn data. The file was created by this
    // process a moment ago (a fresh temporary file, or a path we just
    // unlinked and recreated) and sized with `set_len`; no other process has
    // a reason to hold it. Another process deliberately truncating a
    // linker's output while it is being written is outside what we defend
    // against, as for every mmap-based linker (see docs/development.md).
    // The mapping is only exposed as `&mut [u8]` through the owning
    // `OutputFile`, so Rust aliasing rules hold within this process.
    unsafe { memmap2::MmapOptions::new().len(len).map_mut(file) }
}
