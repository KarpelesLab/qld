//! Section headers and compressed-section headers.

use core::marker::PhantomData;

use super::consts::{SHF_ALLOC, SHF_COMPRESSED, SHF_GROUP, SHF_TLS, SHT_NOBITS};
use super::format::ElfFormat;

/// A decoded section header, with address-sized fields widened to 64 bits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SectionHeader {
    /// Offset of the name in the section name string table.
    pub sh_name: u32,
    /// Section type (`SHT_*`).
    pub sh_type: u32,
    /// Flags (`SHF_*`).
    pub sh_flags: u64,
    /// Address at run time.
    pub sh_addr: u64,
    /// File offset of the contents.
    pub sh_offset: u64,
    /// Size of the contents (in the file, unless `SHT_NOBITS`).
    pub sh_size: u64,
    /// Type-dependent link to another section.
    pub sh_link: u32,
    /// Type-dependent extra information.
    pub sh_info: u32,
    /// Alignment constraint (0 and 1 mean none).
    pub sh_addralign: u64,
    /// Entry size for sections holding fixed-size entries.
    pub sh_entsize: u64,
}

impl SectionHeader {
    /// Whether the section occupies no file space (`SHT_NOBITS`).
    #[inline]
    #[must_use]
    pub fn is_nobits(&self) -> bool {
        self.sh_type == SHT_NOBITS
    }

    /// Whether `SHF_ALLOC` is set.
    #[inline]
    #[must_use]
    pub fn is_alloc(&self) -> bool {
        self.sh_flags & SHF_ALLOC != 0
    }

    /// Whether `SHF_COMPRESSED` is set.
    #[inline]
    #[must_use]
    pub fn is_compressed(&self) -> bool {
        self.sh_flags & SHF_COMPRESSED != 0
    }

    /// Whether `SHF_GROUP` is set.
    #[inline]
    #[must_use]
    pub fn is_group_member(&self) -> bool {
        self.sh_flags & SHF_GROUP != 0
    }

    /// Whether `SHF_TLS` is set.
    #[inline]
    #[must_use]
    pub fn is_tls(&self) -> bool {
        self.sh_flags & SHF_TLS != 0
    }
}

/// A decoded compression header (`Elf32_Chdr` / `Elf64_Chdr`).
///
/// Compressed sections (`SHF_COMPRESSED`) start with this header; the
/// compressed stream follows it. Decompression is done elsewhere.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CompressionHeader {
    /// Compression algorithm (`ELFCOMPRESS_*`).
    pub ch_type: u32,
    /// Size of the uncompressed data.
    pub ch_size: u64,
    /// Alignment of the uncompressed data.
    pub ch_addralign: u64,
}

/// The section header table: a lazily decoded slice of section headers.
#[derive(Debug)]
pub struct SectionTable<'a, F: ElfFormat> {
    raw: &'a [F::Shdr],
    file_offset: u64,
}

impl<F: ElfFormat> Clone for SectionTable<'_, F> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<F: ElfFormat> Copy for SectionTable<'_, F> {}

impl<F: ElfFormat> Default for SectionTable<'_, F> {
    fn default() -> Self {
        Self {
            raw: &[],
            file_offset: 0,
        }
    }
}

impl<'a, F: ElfFormat> SectionTable<'a, F> {
    /// Wraps raw section headers found at `file_offset`.
    #[must_use]
    pub fn new(raw: &'a [F::Shdr], file_offset: u64) -> Self {
        Self { raw, file_offset }
    }

    /// Number of section headers, including the null section 0.
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.raw.len()
    }

    /// Whether the table has no entries at all.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.raw.is_empty()
    }

    /// File offset of the table.
    #[must_use]
    pub fn file_offset(&self) -> u64 {
        self.file_offset
    }

    /// The raw records.
    #[must_use]
    pub fn raw(&self) -> &'a [F::Shdr] {
        self.raw
    }

    /// Decodes section header `index`, or `None` if out of range.
    #[inline]
    #[must_use]
    pub fn get(&self, index: u32) -> Option<SectionHeader> {
        let index = usize::try_from(index).ok()?;
        self.raw.get(index).map(F::decode_shdr)
    }

    /// Iterates over all section headers in index order.
    #[must_use]
    pub fn iter(&self) -> SectionIter<'a, F> {
        SectionIter {
            raw: self.raw.iter(),
            _format: PhantomData,
        }
    }
}

impl<'a, F: ElfFormat> IntoIterator for SectionTable<'a, F> {
    type Item = SectionHeader;
    type IntoIter = SectionIter<'a, F>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// Iterator over decoded section headers.
#[derive(Debug)]
pub struct SectionIter<'a, F: ElfFormat> {
    raw: core::slice::Iter<'a, F::Shdr>,
    _format: PhantomData<F>,
}

impl<F: ElfFormat> Clone for SectionIter<'_, F> {
    fn clone(&self) -> Self {
        Self {
            raw: self.raw.clone(),
            _format: PhantomData,
        }
    }
}

impl<F: ElfFormat> Iterator for SectionIter<'_, F> {
    type Item = SectionHeader;

    #[inline]
    fn next(&mut self) -> Option<SectionHeader> {
        self.raw.next().map(F::decode_shdr)
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.raw.size_hint()
    }

    #[inline]
    fn nth(&mut self, n: usize) -> Option<SectionHeader> {
        self.raw.nth(n).map(F::decode_shdr)
    }
}

impl<F: ElfFormat> ExactSizeIterator for SectionIter<'_, F> {}

impl<F: ElfFormat> DoubleEndedIterator for SectionIter<'_, F> {
    #[inline]
    fn next_back(&mut self) -> Option<SectionHeader> {
        self.raw.next_back().map(F::decode_shdr)
    }
}
