//! Error context and bounds-checked little-endian field readers.

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
    /// Archive member name, when the file is an archive member.
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
    /// `what` is phrased as a noun ("section table"), as the error's
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

/// Reads a fixed-size array at `offset`.
#[inline]
pub(crate) fn array<const N: usize>(data: &[u8], offset: usize) -> Option<[u8; N]> {
    let end = offset.checked_add(N)?;
    data.get(offset..end)?.try_into().ok()
}

/// Reads a little-endian `u8` at `offset`.
#[inline]
pub(crate) fn u8_at(data: &[u8], offset: usize) -> Option<u8> {
    data.get(offset).copied()
}

/// Reads a little-endian `u16` at `offset`.
#[inline]
pub(crate) fn u16_at(data: &[u8], offset: usize) -> Option<u16> {
    array(data, offset).map(u16::from_le_bytes)
}

/// Reads a little-endian `u32` at `offset`.
#[inline]
pub(crate) fn u32_at(data: &[u8], offset: usize) -> Option<u32> {
    array(data, offset).map(u32::from_le_bytes)
}

/// Reads a little-endian `u64` at `offset`.
#[inline]
pub(crate) fn u64_at(data: &[u8], offset: usize) -> Option<u64> {
    array(data, offset).map(u64::from_le_bytes)
}

/// Converts a `usize` to `u64` for error offsets.
#[inline]
pub(crate) fn to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

/// Computes `base + index * size` for error offsets, saturating.
#[inline]
pub(crate) fn entry_offset(base: u64, index: u64, size: usize) -> u64 {
    base.saturating_add(index.saturating_mul(to_u64(size)))
}

/// The bytes before the first NUL (all of `bytes` if there is none).
#[inline]
pub(crate) fn until_nul(bytes: &[u8]) -> &[u8] {
    match bytes.iter().position(|&b| b == 0) {
        Some(end) => bytes.get(..end).unwrap_or(bytes),
        None => bytes,
    }
}

/// The NUL-terminated string at the start of `bytes`, or `None` if there is
/// no terminator.
#[inline]
pub(crate) fn c_string(bytes: &[u8]) -> Option<&[u8]> {
    let end = bytes.iter().position(|&b| b == 0)?;
    bytes.get(..end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readers() {
        let data = [1u8, 2, 3, 4, 5, 6, 7, 8];
        assert_eq!(u16_at(&data, 0), Some(0x0201));
        assert_eq!(u32_at(&data, 4), Some(0x0807_0605));
        assert_eq!(u64_at(&data, 0), Some(0x0807_0605_0403_0201));
        assert_eq!(u32_at(&data, 5), None);
        assert_eq!(u16_at(&data, usize::MAX), None);
        assert_eq!(subslice(&data, 6, 2), Some(&data[6..]));
        assert_eq!(subslice(&data, 6, 3), None);
        assert_eq!(subslice(&data, u64::MAX, 1), None);
        assert_eq!(until_nul(b"ab\0c"), b"ab");
        assert_eq!(until_nul(b"abc"), b"abc");
        assert_eq!(c_string(b"abc"), None);
        assert_eq!(c_string(b"a\0"), Some(&b"a"[..]));
    }
}
