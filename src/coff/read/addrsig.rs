//! `.llvm_addrsig`: the address-significance table clang emits for ICF.
//!
//! The section lists, as ULEB128 numbers, the symbol table indices of the
//! symbols whose address is taken. Symbols not listed may be folded by
//! identical code folding even in "safe" mode. The section is marked
//! `IMAGE_SCN_LNK_REMOVE`.

/// Name of the address-significance section.
pub const ADDRSIG_SECTION: &[u8] = b".llvm_addrsig";

/// Iterator over the symbol indices of an `.llvm_addrsig` section.
///
/// Yields `Err(offset)` for a truncated or overlong ULEB128 number (with the
/// offset of its first byte in the section) and then stops.
#[derive(Clone, Debug)]
pub struct AddrsigIter<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> AddrsigIter<'a> {
    /// Iterates over the contents of an `.llvm_addrsig` section.
    #[must_use]
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }
}

impl Iterator for AddrsigIter<'_> {
    type Item = Result<u32, usize>;

    fn next(&mut self) -> Option<Self::Item> {
        let start = self.pos;
        let rest = self.data.get(start..).filter(|r| !r.is_empty())?;
        let mut value: u64 = 0;
        for (i, &byte) in rest.iter().enumerate().take(10) {
            let shift = u32::try_from(i).unwrap_or(u32::MAX).saturating_mul(7);
            let bits = u64::from(byte & 0x7f)
                .checked_shl(shift)
                .filter(|b| b >> shift == u64::from(byte & 0x7f));
            let Some(bits) = bits else { break };
            value |= bits;
            if byte & 0x80 == 0 {
                self.pos = start.saturating_add(i).saturating_add(1);
                return Some(u32::try_from(value).map_err(|_| start));
            }
        }
        self.pos = self.data.len();
        Some(Err(start))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes() {
        let data = [0x01, 0x80, 0x01, 0xff, 0xff, 0xff, 0xff, 0x0f, 0x05];
        let values: Vec<_> = AddrsigIter::new(&data).collect();
        assert_eq!(values, [Ok(1), Ok(128), Ok(u32::MAX), Ok(5)]);
        let values: Vec<_> = AddrsigIter::new(&[0x02, 0x80]).collect();
        assert_eq!(values, [Ok(2), Err(1)]);
        let values: Vec<_> = AddrsigIter::new(&[0xff, 0xff, 0xff, 0xff, 0x7f]).collect();
        assert_eq!(values, [Err(0)]);
        let values: Vec<_> = AddrsigIter::new(&[0xff; 12]).collect();
        assert_eq!(values, [Err(0)]);
    }
}
