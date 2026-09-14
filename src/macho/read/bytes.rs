//! Byte-level helpers: error context, byte order, bounds-checked reads and
//! LEB128 decoding.

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
    /// Offset of the parsed bytes within the file on disk: non-zero for a
    /// slice of a universal binary. Added to every error offset.
    pub base: u64,
}

impl<'a> Source<'a> {
    /// A source for a stand-alone file.
    #[must_use]
    pub fn new(path: &'a Path) -> Self {
        Self {
            path,
            member: None,
            base: 0,
        }
    }

    /// A source for an archive member.
    #[must_use]
    pub fn member(path: &'a Path, member: &'a str) -> Self {
        Self {
            path,
            member: Some(member),
            base: 0,
        }
    }

    /// The same source, for bytes that start `offset` bytes into it (a fat
    /// slice).
    #[must_use]
    pub fn at(self, offset: u64) -> Self {
        Self {
            base: self.base.saturating_add(offset),
            ..self
        }
    }

    /// Builds an [`Error::Malformed`] for this file.
    ///
    /// `offset` is relative to the parsed bytes; `what` is phrased as a noun
    /// ("load command"), as the error's `Display` implementation expects.
    #[cold]
    #[inline(never)]
    #[must_use]
    pub fn malformed(&self, offset: u64, what: impl Into<String>) -> Error {
        Error::Malformed {
            file: self.path.to_path_buf(),
            member: self.member.map(str::to_owned),
            offset: self.base.saturating_add(offset),
            what: what.into(),
        }
    }
}

/// Byte order of a Mach-O file, decided at run time.
///
/// Mach-O inputs are little-endian in practice (arm64, x86_64); big-endian
/// files (PowerPC) are decoded by the same code, so the readers are not
/// generic over the byte order. The branch on this flag is perfectly
/// predictable.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Endian {
    big: bool,
}

impl Endian {
    /// Little-endian.
    pub const LITTLE: Self = Self { big: false };
    /// Big-endian.
    pub const BIG: Self = Self { big: true };

    /// Whether this is big-endian.
    #[must_use]
    pub fn is_big(self) -> bool {
        self.big
    }

    /// Reads a `u16` at `offset`.
    #[inline]
    #[must_use]
    pub fn u16(self, data: &[u8], offset: usize) -> Option<u16> {
        let bytes = array::<2>(data, offset)?;
        Some(if self.big {
            u16::from_be_bytes(bytes)
        } else {
            u16::from_le_bytes(bytes)
        })
    }

    /// Reads a `u32` at `offset`.
    #[inline]
    #[must_use]
    pub fn u32(self, data: &[u8], offset: usize) -> Option<u32> {
        let bytes = array::<4>(data, offset)?;
        Some(if self.big {
            u32::from_be_bytes(bytes)
        } else {
            u32::from_le_bytes(bytes)
        })
    }

    /// Reads a `u64` at `offset`.
    #[inline]
    #[must_use]
    pub fn u64(self, data: &[u8], offset: usize) -> Option<u64> {
        let bytes = array::<8>(data, offset)?;
        Some(if self.big {
            u64::from_be_bytes(bytes)
        } else {
            u64::from_le_bytes(bytes)
        })
    }

    /// Reads a pointer-sized word (`u32` or `u64`) at `offset`.
    #[inline]
    #[must_use]
    pub fn word(self, data: &[u8], offset: usize, is64: bool) -> Option<u64> {
        if is64 {
            self.u64(data, offset)
        } else {
            self.u32(data, offset).map(u64::from)
        }
    }
}

/// Reads `N` bytes at `offset`.
#[inline]
pub(crate) fn array<const N: usize>(data: &[u8], offset: usize) -> Option<[u8; N]> {
    let end = offset.checked_add(N)?;
    data.get(offset..end)?.try_into().ok()
}

/// Returns `data[offset..offset + size]`, or `None` if it does not fit.
#[inline]
pub(crate) fn subslice(data: &[u8], offset: u64, size: u64) -> Option<&[u8]> {
    let start = usize::try_from(offset).ok()?;
    let len = usize::try_from(size).ok()?;
    let end = start.checked_add(len)?;
    data.get(start..end)
}

/// Converts a `usize` to `u64` for offsets.
#[inline]
pub(crate) fn to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

/// The NUL-terminated string at the start of `data`, without its terminator.
#[inline]
pub(crate) fn cstr(data: &[u8]) -> Option<&[u8]> {
    let len = data.iter().position(|&b| b == 0)?;
    data.get(..len)
}

/// A fixed-size name field (`segname`, `sectname`): up to 16 bytes, NUL
/// padded but not necessarily NUL terminated.
#[inline]
pub(crate) fn fixed_name(bytes: &[u8]) -> &[u8] {
    match bytes.iter().position(|&b| b == 0) {
        Some(len) => bytes.get(..len).unwrap_or(bytes),
        None => bytes,
    }
}

/// Decodes an unsigned LEB128 value at `*pos`, advancing it.
///
/// Returns `None` on truncation or when the value does not fit in 64 bits.
#[inline]
pub fn read_uleb(data: &[u8], pos: &mut usize) -> Option<u64> {
    let mut result = 0u64;
    let mut shift = 0u32;
    loop {
        let byte = *data.get(*pos)?;
        *pos = pos.checked_add(1)?;
        let low = u64::from(byte & 0x7f);
        if shift >= 64 {
            if low != 0 {
                return None;
            }
        } else {
            let shifted = low.checked_shl(shift)?;
            if shifted >> shift != low {
                return None;
            }
            result |= shifted;
        }
        if byte & 0x80 == 0 {
            return Some(result);
        }
        shift = shift.saturating_add(7);
    }
}

/// Decodes a signed LEB128 value at `*pos`, advancing it.
///
/// Returns `None` on truncation. Bits beyond 64 are ignored.
#[inline]
pub fn read_sleb(data: &[u8], pos: &mut usize) -> Option<i64> {
    let mut result = 0i64;
    let mut shift = 0u32;
    loop {
        let byte = *data.get(*pos)?;
        *pos = pos.checked_add(1)?;
        if shift < 64 {
            result |= i64::from(byte & 0x7f).wrapping_shl(shift);
        }
        shift = shift.saturating_add(7);
        if byte & 0x80 == 0 {
            if shift < 64 && byte & 0x40 != 0 {
                result |= (-1i64).wrapping_shl(shift);
            }
            return Some(result);
        }
    }
}

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)]
mod tests {
    use super::*;

    #[test]
    fn leb128() {
        let mut pos = 0;
        assert_eq!(read_uleb(&[0xe5, 0x8e, 0x26], &mut pos), Some(624_485));
        assert_eq!(pos, 3);
        let mut pos = 0;
        assert_eq!(read_sleb(&[0xc0, 0xbb, 0x78], &mut pos), Some(-123_456));
        let mut pos = 0;
        assert_eq!(read_sleb(&[0x7f], &mut pos), Some(-1));
        let mut pos = 0;
        assert_eq!(read_uleb(&[0x80, 0x80], &mut pos), None);
        let max = [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01];
        let mut pos = 0;
        assert_eq!(read_uleb(&max, &mut pos), Some(u64::MAX));
        let over = [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x02];
        let mut pos = 0;
        assert_eq!(read_uleb(&over, &mut pos), None);
        // Redundant zero continuation bytes are accepted.
        let mut pos = 0;
        assert_eq!(
            read_uleb(
                &[
                    0x81, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x00
                ],
                &mut pos
            ),
            Some(1)
        );
    }

    #[test]
    fn names() {
        assert_eq!(fixed_name(b"__TEXT\0\0\0\0\0\0\0\0\0\0"), b"__TEXT");
        assert_eq!(fixed_name(b"__gcc_except_tab"), b"__gcc_except_tab");
        assert_eq!(cstr(b"abc\0def"), Some(&b"abc"[..]));
        assert_eq!(cstr(b"abc"), None);
    }
}
