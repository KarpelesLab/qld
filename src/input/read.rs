//! Bounds-checked integer readers over untrusted byte slices.
//!
//! Every reader returns `None` when the requested bytes are out of range, so
//! callers turn a short read into an error instead of a panic.

/// Returns `data[offset..offset + len]`, or `None` if any part is out of range.
#[inline]
pub(crate) fn bytes(data: &[u8], offset: usize, len: usize) -> Option<&[u8]> {
    data.get(offset..offset.checked_add(len)?)
}

/// Returns the fixed-size array at `offset`.
#[inline]
pub(crate) fn array<const N: usize>(data: &[u8], offset: usize) -> Option<[u8; N]> {
    bytes(data, offset, N)?.try_into().ok()
}

/// Reads a little-endian `u16`.
#[inline]
pub(crate) fn u16_le(data: &[u8], offset: usize) -> Option<u16> {
    array(data, offset).map(u16::from_le_bytes)
}

/// Reads a big-endian `u16`.
#[inline]
pub(crate) fn u16_be(data: &[u8], offset: usize) -> Option<u16> {
    array(data, offset).map(u16::from_be_bytes)
}

/// Reads a little-endian `u32`.
#[inline]
pub(crate) fn u32_le(data: &[u8], offset: usize) -> Option<u32> {
    array(data, offset).map(u32::from_le_bytes)
}

/// Reads a big-endian `u32`.
#[inline]
pub(crate) fn u32_be(data: &[u8], offset: usize) -> Option<u32> {
    array(data, offset).map(u32::from_be_bytes)
}

/// Reads a little-endian `u64`.
#[inline]
pub(crate) fn u64_le(data: &[u8], offset: usize) -> Option<u64> {
    array(data, offset).map(u64::from_le_bytes)
}

/// Reads a big-endian `u64`.
#[inline]
pub(crate) fn u64_be(data: &[u8], offset: usize) -> Option<u64> {
    array(data, offset).map(u64::from_be_bytes)
}

/// Converts a `u64` file offset or size to `usize`, failing on 32-bit hosts
/// when it does not fit.
#[inline]
pub(crate) fn to_usize(value: u64) -> Option<usize> {
    usize::try_from(value).ok()
}

/// Converts a `usize` to `u64`. Infallible on every supported host; saturates
/// rather than panicking if a future host had `usize` wider than 64 bits.
#[inline]
pub(crate) fn to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)] // Test code builds fixtures, not parses input.
mod tests {
    use super::*;

    #[test]
    fn readers_reject_out_of_range() {
        let data = [1u8, 2, 3, 4, 5, 6, 7, 8];
        assert_eq!(u32_le(&data, 0), Some(0x0403_0201));
        assert_eq!(u32_be(&data, 4), Some(0x0506_0708));
        assert_eq!(u16_be(&data, 6), Some(0x0708));
        assert_eq!(u16_le(&data, 7), None);
        assert_eq!(u64_le(&data, 0), Some(0x0807_0605_0403_0201));
        assert_eq!(u64_be(&data, 1), None);
        assert_eq!(bytes(&data, usize::MAX, 2), None);
        assert_eq!(bytes(&data, 8, 0), Some(&[][..]));
    }
}
