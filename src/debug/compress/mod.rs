//! In-crate compression codecs for debug sections.
//!
//! - [`zlib_decompress_into`] / [`inflate_into`]: zlib and raw DEFLATE
//!   decoding (RFC 1950/1951).
//! - [`zstd_decompress_into`]: Zstandard decoding (RFC 8878), without
//!   dictionaries.
//! - [`deflate`]: DEFLATE encoding with parallel, concatenable chunks, the
//!   way lld compresses `--compress-debug-sections=zlib` output.
//! - [`adler32`], [`adler32_combine`]: the zlib checksum.
//!
//! Decoders write into a caller-provided buffer whose size is known from
//! the section header, and fail with a [`DecodeError`] (an input offset and
//! a static description) that the caller turns into
//! [`Error::Malformed`](crate::Error::Malformed) with
//! [`DecodeError::into_error`].
//!
//! [`Codec`] is the only place that dispatches on the algorithm, so swapping
//! an implementation for a library crate is a local change.

mod adler32;
pub mod deflate;
mod inflate;
pub mod zstd;

use std::fmt;
use std::path::Path;

pub use adler32::{ADLER32_INIT, adler32, adler32_combine, adler32_update};
pub use inflate::{inflate_into, zlib_decompress_into};
pub use zstd::zstd_decompress_into;

use crate::error::Error;

/// A decompression failure: what was wrong, and where in the compressed
/// input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DecodeError {
    /// Byte offset within the compressed input (approximate for bit-level
    /// problems).
    pub offset: usize,
    /// What was wrong, as a noun phrase ("zlib header").
    pub what: &'static str,
}

impl DecodeError {
    /// Creates an error at `offset`.
    #[must_use]
    pub const fn new(offset: usize, what: &'static str) -> Self {
        Self { offset, what }
    }

    /// Moves the offset by `base` bytes, for errors found in a sub-slice.
    #[must_use]
    pub const fn shifted(self, base: usize) -> Self {
        Self {
            offset: self.offset.saturating_add(base),
            what: self.what,
        }
    }

    /// Converts to [`Error::Malformed`], where the compressed input starts
    /// at file offset `base`.
    #[cold]
    #[must_use]
    pub fn into_error(self, file: &Path, member: Option<&str>, base: u64) -> Error {
        Error::Malformed {
            file: file.to_path_buf(),
            member: member.map(str::to_owned),
            offset: base.saturating_add(u64::try_from(self.offset).unwrap_or(u64::MAX)),
            what: self.what.to_owned(),
        }
    }
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "malformed {} at offset {:#x}", self.what, self.offset)
    }
}

impl std::error::Error for DecodeError {}

/// A compression algorithm for debug sections.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Codec {
    /// zlib (`ELFCOMPRESS_ZLIB`, and the legacy `.zdebug` format).
    Zlib,
    /// Zstandard (`ELFCOMPRESS_ZSTD`).
    Zstd,
}

impl Codec {
    /// Decompresses `input` into `out`, which must come out exactly full.
    ///
    /// # Errors
    ///
    /// Returns a [`DecodeError`] if the data is corrupt, truncated, fails
    /// its checksum, or does not decompress to exactly `out.len()` bytes.
    pub fn decompress_into(self, input: &[u8], out: &mut [u8]) -> Result<(), DecodeError> {
        match self {
            Self::Zlib => zlib_decompress_into(input, out),
            Self::Zstd => zstd_decompress_into(input, out),
        }
    }
}
