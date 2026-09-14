//! Relocation sections: `SHT_RELA`, `SHT_REL` and `SHT_RELR`.
//!
//! Relocations stay in the input mapping until they are needed. The slice
//! types here are thin wrappers over the raw records; their iterators decode
//! one entry per step, with no allocation and no run-time format dispatch.

use core::marker::PhantomData;

use super::format::ElfFormat;
use super::section::SectionHeader;

/// A decoded relocation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Relocation {
    /// Offset of the relocated location within the target section (or a
    /// virtual address, in executables and shared objects).
    pub offset: u64,
    /// Symbol table index.
    pub symbol: u32,
    /// Relocation type; its meaning depends on `e_machine`.
    pub r_type: u32,
    /// Explicit addend; 0 for `SHT_REL`, where the addend is stored in the
    /// relocated location.
    pub addend: i64,
}

macro_rules! reloc_slice {
    ($(#[$doc:meta])* $name:ident, $iter:ident, $raw:ident, $decode:ident) => {
        $(#[$doc])*
        #[derive(Debug)]
        pub struct $name<'a, F: ElfFormat> {
            raw: &'a [F::$raw],
        }

        impl<F: ElfFormat> Clone for $name<'_, F> {
            fn clone(&self) -> Self {
                *self
            }
        }
        impl<F: ElfFormat> Copy for $name<'_, F> {}

        impl<F: ElfFormat> Default for $name<'_, F> {
            fn default() -> Self {
                Self { raw: &[] }
            }
        }

        impl<'a, F: ElfFormat> $name<'a, F> {
            /// Wraps raw records.
            #[inline]
            #[must_use]
            pub fn new(raw: &'a [F::$raw]) -> Self {
                Self { raw }
            }

            /// Number of relocations.
            #[inline]
            #[must_use]
            pub fn len(&self) -> usize {
                self.raw.len()
            }

            /// Whether there are no relocations.
            #[inline]
            #[must_use]
            pub fn is_empty(&self) -> bool {
                self.raw.is_empty()
            }

            /// The raw records.
            #[inline]
            #[must_use]
            pub fn raw(&self) -> &'a [F::$raw] {
                self.raw
            }

            /// Decodes relocation `index`.
            #[inline]
            #[must_use]
            pub fn get(&self, index: usize) -> Option<Relocation> {
                self.raw.get(index).map(F::$decode)
            }

            /// Iterates over the decoded relocations.
            #[inline]
            #[must_use]
            pub fn iter(&self) -> $iter<'a, F> {
                $iter {
                    raw: self.raw.iter(),
                    _format: PhantomData,
                }
            }
        }

        impl<'a, F: ElfFormat> IntoIterator for $name<'a, F> {
            type Item = Relocation;
            type IntoIter = $iter<'a, F>;
            fn into_iter(self) -> Self::IntoIter {
                self.iter()
            }
        }

        #[doc = concat!("Iterator over the relocations of a [`", stringify!($name), "`].")]
        #[derive(Debug)]
        pub struct $iter<'a, F: ElfFormat> {
            raw: core::slice::Iter<'a, F::$raw>,
            _format: PhantomData<F>,
        }

        impl<F: ElfFormat> Clone for $iter<'_, F> {
            fn clone(&self) -> Self {
                Self {
                    raw: self.raw.clone(),
                    _format: PhantomData,
                }
            }
        }

        impl<F: ElfFormat> Iterator for $iter<'_, F> {
            type Item = Relocation;

            #[inline]
            fn next(&mut self) -> Option<Relocation> {
                self.raw.next().map(F::$decode)
            }

            #[inline]
            fn size_hint(&self) -> (usize, Option<usize>) {
                self.raw.size_hint()
            }

            #[inline]
            fn nth(&mut self, n: usize) -> Option<Relocation> {
                self.raw.nth(n).map(F::$decode)
            }
        }

        impl<F: ElfFormat> ExactSizeIterator for $iter<'_, F> {}

        impl<F: ElfFormat> DoubleEndedIterator for $iter<'_, F> {
            #[inline]
            fn next_back(&mut self) -> Option<Relocation> {
                self.raw.next_back().map(F::$decode)
            }
        }
    };
}

reloc_slice! {
    /// The contents of an `SHT_RELA` section.
    RelaSlice, RelaIter, Rela, decode_rela
}

reloc_slice! {
    /// The contents of an `SHT_REL` section (addends are implicit).
    RelSlice, RelIter, Rel, decode_rel
}

/// Relocations with either implicit or explicit addends.
///
/// Prefer matching once and looping over the typed slice in hot code; the
/// accessors here branch on the variant per call.
#[derive(Debug)]
pub enum Relocations<'a, F: ElfFormat> {
    /// `SHT_REL`.
    Rel(RelSlice<'a, F>),
    /// `SHT_RELA`.
    Rela(RelaSlice<'a, F>),
}

impl<F: ElfFormat> Clone for Relocations<'_, F> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<F: ElfFormat> Copy for Relocations<'_, F> {}

impl<F: ElfFormat> Relocations<'_, F> {
    /// Number of relocations.
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::Rel(r) => r.len(),
            Self::Rela(r) => r.len(),
        }
    }

    /// Whether there are no relocations.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether addends are explicit.
    #[inline]
    #[must_use]
    pub fn is_rela(&self) -> bool {
        matches!(self, Self::Rela(_))
    }

    /// Decodes relocation `index`.
    #[inline]
    #[must_use]
    pub fn get(&self, index: usize) -> Option<Relocation> {
        match self {
            Self::Rel(r) => r.get(index),
            Self::Rela(r) => r.get(index),
        }
    }
}

/// A relocation section of a relocatable object.
#[derive(Debug)]
pub struct RelocationSection<'a, F: ElfFormat> {
    /// Index of the relocation section itself.
    pub index: u32,
    /// Its header.
    pub header: SectionHeader,
    /// Index of the section the relocations apply to (`sh_info`).
    pub target: u32,
    /// Index of the symbol table the relocations refer to (`sh_link`).
    pub symtab: u32,
    /// The relocations.
    pub relocations: Relocations<'a, F>,
}

impl<F: ElfFormat> Clone for RelocationSection<'_, F> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<F: ElfFormat> Copy for RelocationSection<'_, F> {}

/// The contents of an `SHT_RELR` section (or `DT_RELR` table).
#[derive(Debug)]
pub struct RelrSlice<'a, F: ElfFormat> {
    raw: &'a [F::Word],
}

impl<F: ElfFormat> Clone for RelrSlice<'_, F> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<F: ElfFormat> Copy for RelrSlice<'_, F> {}

impl<'a, F: ElfFormat> RelrSlice<'a, F> {
    /// Wraps raw entries.
    #[must_use]
    pub fn new(raw: &'a [F::Word]) -> Self {
        Self { raw }
    }

    /// Number of encoded entries (not relocations).
    #[must_use]
    pub fn len(&self) -> usize {
        self.raw.len()
    }

    /// Whether there are no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.raw.is_empty()
    }

    /// Iterates over the relocated addresses.
    #[must_use]
    pub fn iter(&self) -> RelrIter<'a, F> {
        RelrIter {
            raw: self.raw.iter(),
            base: 0,
            bitmap: 0,
            bit_base: 0,
        }
    }
}

/// Iterator over the addresses encoded in a [`RelrSlice`].
///
/// An even entry is an address, and sets the next address to the following
/// word. An odd entry is a bitmap: bit `i` (for `i >= 1`) marks the address
/// `i - 1` words past the current one; afterwards the current address moves
/// by `word_bits - 1` words. Arithmetic wraps on malformed input.
#[derive(Debug)]
pub struct RelrIter<'a, F: ElfFormat> {
    raw: core::slice::Iter<'a, F::Word>,
    /// Address the next bitmap starts at.
    base: u64,
    /// Remaining bits of the current bitmap (bit 0 already consumed).
    bitmap: u64,
    /// Address corresponding to bit 1 of the current bitmap.
    bit_base: u64,
}

impl<F: ElfFormat> Clone for RelrIter<'_, F> {
    fn clone(&self) -> Self {
        Self {
            raw: self.raw.clone(),
            base: self.base,
            bitmap: self.bitmap,
            bit_base: self.bit_base,
        }
    }
}

impl<F: ElfFormat> Iterator for RelrIter<'_, F> {
    type Item = u64;

    fn next(&mut self) -> Option<u64> {
        let word = F::WORD_SIZE as u64;
        loop {
            if self.bitmap != 0 {
                let bit = u64::from(self.bitmap.trailing_zeros());
                self.bitmap &= self.bitmap.wrapping_sub(1);
                // Bit `bit` stands for `bit - 1` words past `bit_base`.
                let skip = bit.wrapping_sub(1).wrapping_mul(word);
                return Some(self.bit_base.wrapping_add(skip));
            }
            let entry = F::decode_word(self.raw.next()?);
            if entry & 1 == 0 {
                self.base = entry.wrapping_add(word);
                return Some(entry);
            }
            let bits = (F::WORD_SIZE as u64).wrapping_mul(8);
            self.bitmap = entry & !1;
            self.bit_base = self.base;
            self.base = self
                .base
                .wrapping_add(bits.wrapping_sub(1).wrapping_mul(word));
        }
    }
}

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)]
mod tests {
    use super::*;
    use crate::elf::read::format::{Elf32Le, Elf64Le};

    fn words64(values: &[u64]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    #[test]
    fn relr_decoding_64() {
        // 0x1000, then a bitmap covering 0x1008 (bit 1) and 0x1018 (bit 3),
        // then a bitmap with bit 63 set: 0x1000 + 8 + 63*8 + 62*8.
        let bytes = words64(&[0x1000, 0b1011, 1 | (1 << 63)]);
        let (raw, _) = <[u8; 8] as crate::elf::read::format::RawRecord>::slice_from(&bytes);
        let got: Vec<u64> = RelrSlice::<Elf64Le>::new(raw).iter().collect();
        assert_eq!(got, vec![0x1000, 0x1008, 0x1018, 0x1008 + 63 * 8 + 62 * 8]);
    }

    #[test]
    fn relr_decoding_32() {
        let bytes: Vec<u8> = [0x2000u32, 0b111]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let (raw, _) = <[u8; 4] as crate::elf::read::format::RawRecord>::slice_from(&bytes);
        let got: Vec<u64> = RelrSlice::<Elf32Le>::new(raw).iter().collect();
        assert_eq!(got, vec![0x2000, 0x2004, 0x2008]);
    }

    #[test]
    fn relr_malformed_does_not_panic() {
        let bytes = words64(&[u64::MAX, u64::MAX - 1, u64::MAX]);
        let (raw, _) = <[u8; 8] as crate::elf::read::format::RawRecord>::slice_from(&bytes);
        assert!(RelrSlice::<Elf64Le>::new(raw).iter().count() > 0);
    }
}
