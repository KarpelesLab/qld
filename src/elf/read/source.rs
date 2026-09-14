//! Error context and bounds-checked slicing helpers.

use std::path::Path;

use crate::error::Error;

/// Identifies the file being parsed, for error messages.
///
/// Parsers carry this value and only turn it into an owned
/// [`Error::Malformed`] when something is actually wrong, so the success path
/// never allocates.
#[derive(Clone, Copy, Debug)]
pub struct Source<'a> {
    /// Path of the file (or of the archive containing it).
    pub path: &'a Path,
    /// Archive member name, when the ELF file is an archive member.
    pub member: Option<&'a str>,
}

impl<'a> Source<'a> {
    /// A source for a stand-alone file.
    #[must_use]
    pub fn new(path: &'a Path) -> Self {
        Self { path, member: None }
    }

    /// A source for an archive member.
    #[must_use]
    pub fn member(path: &'a Path, member: &'a str) -> Self {
        Self {
            path,
            member: Some(member),
        }
    }

    /// Builds an [`Error::Malformed`] for this file.
    ///
    /// `what` is phrased as a noun ("section header table"), as the error's
    /// `Display` implementation expects.
    #[cold]
    #[inline(never)]
    #[must_use]
    pub fn malformed(&self, offset: u64, what: impl Into<String>) -> Error {
        Error::Malformed {
            file: self.path.to_path_buf(),
            member: self.member.map(str::to_owned),
            offset,
            what: what.into(),
        }
    }
}

/// Returns `data[offset..offset + size]`, or `None` if it does not fit.
#[inline]
pub(crate) fn subslice(data: &[u8], offset: u64, size: u64) -> Option<&[u8]> {
    let start = usize::try_from(offset).ok()?;
    let len = usize::try_from(size).ok()?;
    let end = start.checked_add(len)?;
    data.get(start..end)
}

/// Converts a `usize` to `u64` for error offsets.
#[inline]
pub(crate) fn to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

/// Computes `base + index * size` for error offsets, saturating.
#[inline]
pub(crate) fn entry_offset(base: u64, index: usize, size: usize) -> u64 {
    base.saturating_add(to_u64(index).saturating_mul(to_u64(size)))
}
