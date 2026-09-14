//! String tables (`SHT_STRTAB`).

/// A string table: NUL-terminated byte strings addressed by offset.
#[derive(Clone, Copy, Debug, Default)]
pub struct StringTable<'a> {
    data: &'a [u8],
    file_offset: u64,
}

impl<'a> StringTable<'a> {
    /// Wraps the contents of a string table found at `file_offset` in the
    /// file (the offset is only used for error messages).
    #[must_use]
    pub fn new(data: &'a [u8], file_offset: u64) -> Self {
        Self { data, file_offset }
    }

    /// The raw table contents.
    #[must_use]
    pub fn data(&self) -> &'a [u8] {
        self.data
    }

    /// File offset of the table.
    #[must_use]
    pub fn file_offset(&self) -> u64 {
        self.file_offset
    }

    /// Returns the string starting at `offset`, without its terminator.
    ///
    /// Returns `None` if `offset` is out of range or the string is not
    /// NUL-terminated within the table.
    #[inline]
    #[must_use]
    pub fn get(&self, offset: u32) -> Option<&'a [u8]> {
        let start = usize::try_from(offset).ok()?;
        let tail = self.data.get(start..)?;
        let len = find_nul(tail)?;
        tail.get(..len)
    }
}

/// Finds the first NUL byte, a word at a time.
#[inline]
pub(crate) fn find_nul(bytes: &[u8]) -> Option<usize> {
    const LO: u64 = 0x0101_0101_0101_0101;
    const HI: u64 = 0x8080_8080_8080_8080;
    let (words, _) = bytes.as_chunks::<8>();
    let mut base = 0usize;
    for word in words {
        let w = u64::from_le_bytes(*word);
        let zero = w.wrapping_sub(LO) & !w & HI;
        if zero != 0 {
            // The lowest set bit marks the first zero byte (bytes after it
            // may be false positives, but they come later).
            let byte = (zero.trailing_zeros() / 8) as usize;
            return Some(base.wrapping_add(byte));
        }
        base = base.wrapping_add(8);
    }
    let rest = bytes.get(base..)?;
    rest.iter()
        .position(|&b| b == 0)
        .map(|i| base.wrapping_add(i))
}

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)]
mod tests {
    use super::*;

    #[test]
    fn finds_nul_everywhere() {
        for len in 0..40 {
            for pos in 0..len {
                let mut v = vec![b'a'; len];
                v[pos] = 0;
                if pos + 1 < len {
                    v[len - 1] = 0;
                }
                assert_eq!(find_nul(&v), Some(pos), "len {len} pos {pos}");
            }
            assert_eq!(find_nul(&vec![0x80u8; len]), None);
        }
        // Bytes with the high bit set next to a zero must not confuse it.
        assert_eq!(find_nul(&[0x81, 0x80, 0x01, 0x00, 0xff, 0, 0, 0]), Some(3));
    }

    #[test]
    fn lookups() {
        let table = StringTable::new(b"\0foo\0bar\0unterminated", 0);
        assert_eq!(table.get(0), Some(&b""[..]));
        assert_eq!(table.get(1), Some(&b"foo"[..]));
        assert_eq!(table.get(3), Some(&b"o"[..]));
        assert_eq!(table.get(5), Some(&b"bar"[..]));
        assert_eq!(table.get(9), None);
        assert_eq!(table.get(100), None);
        assert_eq!(table.get(u32::MAX), None);
    }
}
