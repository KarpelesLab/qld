//! A bounds-checked cursor over DWARF data.

/// A DWARF decoding failure: what was wrong and the offset within the
/// section being read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DwarfError {
    /// Offset within the section.
    pub offset: usize,
    /// What was wrong, as a noun phrase.
    pub what: &'static str,
}

/// Result type of the DWARF readers.
pub type DwarfResult<T> = Result<T, DwarfError>;

/// A cursor over a byte slice with the byte order of the file.
#[derive(Clone, Copy, Debug)]
pub(super) struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
    big_endian: bool,
}

impl<'a> Reader<'a> {
    pub(super) fn new(data: &'a [u8], big_endian: bool) -> Self {
        Self {
            data,
            pos: 0,
            big_endian,
        }
    }

    /// A reader positioned at `pos` (which may be out of range; reads then
    /// fail).
    pub(super) fn at(data: &'a [u8], pos: usize, big_endian: bool) -> Self {
        Self {
            data,
            pos,
            big_endian,
        }
    }

    pub(super) fn pos(&self) -> usize {
        self.pos
    }

    pub(super) fn is_empty(&self) -> bool {
        self.pos >= self.data.len()
    }

    #[cold]
    pub(super) fn error(&self, what: &'static str) -> DwarfError {
        DwarfError {
            offset: self.pos,
            what,
        }
    }

    pub(super) fn bytes(&mut self, n: usize) -> DwarfResult<&'a [u8]> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or_else(|| self.error("DWARF data (truncated)"))?;
        let bytes = self
            .data
            .get(self.pos..end)
            .ok_or_else(|| self.error("DWARF data (truncated)"))?;
        self.pos = end;
        Ok(bytes)
    }

    pub(super) fn skip(&mut self, n: u64) -> DwarfResult<()> {
        let n = usize::try_from(n).map_err(|_| self.error("DWARF data (truncated)"))?;
        self.bytes(n).map(|_| ())
    }

    pub(super) fn u8(&mut self) -> DwarfResult<u8> {
        Ok(self.bytes(1)?[0])
    }

    pub(super) fn i8(&mut self) -> DwarfResult<i8> {
        Ok(i8::from_ne_bytes([self.u8()?]))
    }

    /// Reads an unsigned integer of `size` bytes (1 to 8).
    pub(super) fn uint(&mut self, size: usize) -> DwarfResult<u64> {
        if size == 0 || size > 8 {
            return Err(self.error("DWARF integer size"));
        }
        let bytes = self.bytes(size)?;
        let mut buf = [0u8; 8];
        if self.big_endian {
            if let Some(dst) = buf.get_mut(8usize.wrapping_sub(size)..) {
                dst.copy_from_slice(bytes);
            }
            Ok(u64::from_be_bytes(buf))
        } else {
            if let Some(dst) = buf.get_mut(..size) {
                dst.copy_from_slice(bytes);
            }
            Ok(u64::from_le_bytes(buf))
        }
    }

    pub(super) fn u16(&mut self) -> DwarfResult<u16> {
        self.uint(2).map(|v| v as u16)
    }

    pub(super) fn u32(&mut self) -> DwarfResult<u32> {
        self.uint(4).map(|v| v as u32)
    }

    pub(super) fn u64(&mut self) -> DwarfResult<u64> {
        self.uint(8)
    }

    /// Reads an unsigned LEB128 number. Bits beyond 64 must be zero.
    pub(super) fn uleb(&mut self) -> DwarfResult<u64> {
        let mut value = 0u64;
        let mut shift = 0u32;
        loop {
            let byte = self.u8()?;
            let low = u64::from(byte & 0x7f);
            if shift < 64 {
                if shift > 57 && low >> (64u32.wrapping_sub(shift)) != 0 {
                    return Err(self.error("DWARF LEB128 (too large)"));
                }
                value |= low << shift;
            } else if low != 0 {
                return Err(self.error("DWARF LEB128 (too large)"));
            }
            if byte & 0x80 == 0 {
                return Ok(value);
            }
            shift = shift.saturating_add(7);
        }
    }

    /// Reads a signed LEB128 number (wrapping on overlong encodings).
    pub(super) fn sleb(&mut self) -> DwarfResult<i64> {
        let mut value = 0i64;
        let mut shift = 0u32;
        loop {
            let byte = self.u8()?;
            if shift < 64 {
                value |= i64::from(byte & 0x7f).wrapping_shl(shift);
            }
            shift = shift.saturating_add(7);
            if byte & 0x80 == 0 {
                if shift < 64 && byte & 0x40 != 0 {
                    value |= (-1i64).wrapping_shl(shift);
                }
                return Ok(value);
            }
        }
    }

    /// Reads a NUL-terminated string (without the NUL).
    pub(super) fn cstr(&mut self) -> DwarfResult<&'a [u8]> {
        let rest = self.data.get(self.pos..).unwrap_or_default();
        let len = rest
            .iter()
            .position(|&b| b == 0)
            .ok_or_else(|| self.error("DWARF string (unterminated)"))?;
        let s = self.bytes(len)?;
        self.pos = self.pos.wrapping_add(1);
        Ok(s)
    }

    /// Reads an initial length field. Returns the length and the offset
    /// size (4 or 8) of the unit.
    pub(super) fn initial_length(&mut self) -> DwarfResult<(u64, usize)> {
        let length = self.u32()?;
        match length {
            0xffff_ffff => Ok((self.u64()?, 8)),
            0xffff_fff0..=0xffff_fffe => Err(self.error("DWARF unit length (reserved value)")),
            _ => Ok((u64::from(length), 4)),
        }
    }

    /// Splits off the next `length` bytes as a reader over the same section
    /// (positions stay section-relative), and moves past them.
    pub(super) fn sub(&mut self, length: u64) -> DwarfResult<Reader<'a>> {
        let start = self.pos;
        let length = usize::try_from(length).map_err(|_| self.error("DWARF unit length"))?;
        let end = start
            .checked_add(length)
            .filter(|&end| end <= self.data.len())
            .ok_or_else(|| self.error("DWARF unit length (past the end of the section)"))?;
        self.pos = end;
        Ok(Reader {
            data: self.data.get(..end).unwrap_or_default(),
            pos: start,
            big_endian: self.big_endian,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leb128() {
        let mut r = Reader::new(&[0xe5, 0x8e, 0x26, 0x7f, 0x80, 0x7f, 0x02], false);
        assert_eq!(r.uleb().unwrap(), 624_485);
        assert_eq!(r.sleb().unwrap(), -1);
        assert_eq!(r.sleb().unwrap(), -128);
        assert_eq!(r.uleb().unwrap(), 2);
        assert!(r.uleb().is_err());
        let max = [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01];
        assert_eq!(Reader::new(&max, false).uleb().unwrap(), u64::MAX);
        let over = [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x02];
        assert!(Reader::new(&over, false).uleb().is_err());
    }

    #[test]
    fn integers_and_strings() {
        let data = [
            1, 2, 3, 4, b'h', b'i', 0, 0xff, 0xff, 0xff, 0xff, 9, 0, 0, 0, 0, 0, 0, 0,
        ];
        let mut r = Reader::new(&data, false);
        assert_eq!(r.u16().unwrap(), 0x0201);
        let mut be = r;
        assert_eq!(r.u16().unwrap(), 0x0403);
        be.big_endian = true;
        assert_eq!(be.u16().unwrap(), 0x0304);
        assert_eq!(r.cstr().unwrap(), b"hi");
        assert_eq!(r.initial_length().unwrap(), (9, 8));
        assert!(r.u8().is_err());
    }
}
