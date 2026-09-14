//! The COFF string table, which follows the symbol table.

use super::source::c_string;

/// A COFF string table: a 4-byte little-endian size (counting itself)
/// followed by NUL-terminated strings addressed by offset from the start of
/// the size field.
#[derive(Clone, Copy, Debug, Default)]
pub struct StringTable<'a> {
    /// The whole table, size field included.
    data: &'a [u8],
    file_offset: u64,
}

impl<'a> StringTable<'a> {
    /// Wraps a table found at `file_offset`. `data` includes the size field
    /// and is already cut to the declared size.
    #[must_use]
    pub fn new(data: &'a [u8], file_offset: u64) -> Self {
        Self { data, file_offset }
    }

    /// The raw table, size field included.
    #[must_use]
    pub fn data(&self) -> &'a [u8] {
        self.data
    }

    /// File offset of the table.
    #[must_use]
    pub fn file_offset(&self) -> u64 {
        self.file_offset
    }

    /// Size of the table in bytes, including the size field.
    #[must_use]
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Whether the table holds no strings.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.data.len() <= 4
    }

    /// Returns the string at `offset`, without its terminator.
    ///
    /// Offset 0 denotes the empty string. Offsets inside the size field,
    /// past the end, or strings without a terminator return `None`.
    #[inline]
    #[must_use]
    pub fn get(&self, offset: u32) -> Option<&'a [u8]> {
        if offset == 0 {
            return Some(&[]);
        }
        let start = usize::try_from(offset).ok()?;
        if start < 4 {
            return None;
        }
        c_string(self.data.get(start..)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookups() {
        let table = StringTable::new(b"\x0e\0\0\0foo\0bar\0baz", 0);
        assert_eq!(table.get(0), Some(&b""[..]));
        assert_eq!(table.get(2), None);
        assert_eq!(table.get(4), Some(&b"foo"[..]));
        assert_eq!(table.get(8), Some(&b"bar"[..]));
        assert_eq!(table.get(12), None);
        assert_eq!(table.get(u32::MAX), None);
    }
}
