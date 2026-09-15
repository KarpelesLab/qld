//! Small byte-buffer helpers for building output structures.

#![deny(clippy::arithmetic_side_effects)]

/// Appends `value` as unsigned LEB128.
pub fn push_uleb(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// Appends `value` as signed LEB128.
pub fn push_sleb(out: &mut Vec<u8>, mut value: i64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        let done = (value == 0 && byte & 0x40 == 0) || (value == -1 && byte & 0x40 != 0);
        if done {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// The size of `value` as unsigned LEB128.
#[must_use]
pub fn uleb_size(value: u64) -> usize {
    let bits = 64u32.saturating_sub(value.leading_zeros()).max(1);
    usize::try_from(bits.div_ceil(7)).unwrap_or(10)
}

/// Appends a little-endian `u16`.
pub fn push16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}

/// Appends a little-endian `u32`.
pub fn push32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

/// Appends a little-endian `u64`.
pub fn push64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

/// Appends a fixed 16-byte name field, NUL padded.
pub fn push_name16(out: &mut Vec<u8>, name: &[u8]) {
    let mut field = [0u8; 16];
    for (slot, &byte) in field.iter_mut().zip(name) {
        *slot = byte;
    }
    out.extend_from_slice(&field);
}

/// Pads `out` with zeros to a multiple of `align` (a power of two).
pub fn pad_to(out: &mut Vec<u8>, align: usize) {
    let len = out.len().next_multiple_of(align.max(1));
    out.resize(len, 0);
}

/// Writes a little-endian `u32` at `at`. Returns `None` when out of range.
pub fn put32(out: &mut [u8], at: usize, value: u32) -> Option<()> {
    out.get_mut(at..at.checked_add(4)?)?
        .copy_from_slice(&value.to_le_bytes());
    Some(())
}

/// Writes a little-endian `u64` at `at`. Returns `None` when out of range.
pub fn put64(out: &mut [u8], at: usize, value: u64) -> Option<()> {
    out.get_mut(at..at.checked_add(8)?)?
        .copy_from_slice(&value.to_le_bytes());
    Some(())
}

/// Reads a little-endian `u32` at `at`.
#[must_use]
pub fn get32(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(*data.get(at..)?.first_chunk::<4>()?))
}

/// Reads a little-endian `u64` at `at`.
#[must_use]
pub fn get64(data: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(*data.get(at..)?.first_chunk::<8>()?))
}

/// Converts a `u64` to `usize`, saturating (callers bounds-check the
/// result).
#[must_use]
pub fn to_usize(value: u64) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

/// Converts a `usize` to `u64`.
#[must_use]
pub fn to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

/// Rounds `value` up to a multiple of `align` (a power of two), saturating.
#[must_use]
pub fn align_up(value: u64, align: u64) -> u64 {
    if align <= 1 {
        return value;
    }
    let mask = align.saturating_sub(1);
    value.checked_add(mask).map_or(u64::MAX, |v| v & !mask)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::macho::read::bytes::{read_sleb, read_uleb};

    #[test]
    fn leb_round_trip() {
        for value in [0u64, 1, 127, 128, 300, 624_485, u64::MAX] {
            let mut out = Vec::new();
            push_uleb(&mut out, value);
            assert_eq!(out.len(), uleb_size(value));
            let mut pos = 0;
            assert_eq!(read_uleb(&out, &mut pos), Some(value));
        }
        for value in [0i64, 1, -1, 63, 64, -64, -65, -123_456, i64::MIN, i64::MAX] {
            let mut out = Vec::new();
            push_sleb(&mut out, value);
            let mut pos = 0;
            assert_eq!(read_sleb(&out, &mut pos), Some(value));
        }
        assert_eq!(align_up(13, 8), 16);
        assert_eq!(align_up(16, 8), 16);
    }
}
