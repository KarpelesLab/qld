//! COFF relocations: 10-byte `IMAGE_RELOCATION` records.

use super::source::{u16_at, u32_at};

/// Size of a relocation record.
pub const RELOCATION_SIZE: usize = 10;

/// A decoded relocation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Relocation {
    /// `VirtualAddress`: offset of the relocated field within the section
    /// (the section's `VirtualAddress` is added in the rare objects where it
    /// is nonzero).
    pub virtual_address: u32,
    /// `SymbolTableIndex`: record index of the target symbol.
    pub symbol_table_index: u32,
    /// `Type`: machine-specific relocation type
    /// (see [`relocation_name`](super::consts::relocation_name)).
    pub r_type: u16,
}

impl Relocation {
    /// Decodes a 10-byte record.
    #[must_use]
    pub fn decode(raw: &[u8; RELOCATION_SIZE]) -> Self {
        Self {
            virtual_address: u32_at(raw, 0).unwrap_or(0),
            symbol_table_index: u32_at(raw, 4).unwrap_or(0),
            r_type: u16_at(raw, 8).unwrap_or(0),
        }
    }
}

/// The relocations of one section: a slice of raw records, decoded on
/// access.
#[derive(Clone, Copy, Debug, Default)]
pub struct Relocations<'a> {
    raw: &'a [[u8; RELOCATION_SIZE]],
    file_offset: u64,
}

impl<'a> Relocations<'a> {
    /// Wraps raw records found at `file_offset`.
    #[must_use]
    pub fn new(raw: &'a [[u8; RELOCATION_SIZE]], file_offset: u64) -> Self {
        Self { raw, file_offset }
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

    /// File offset of the first record.
    #[must_use]
    pub fn file_offset(&self) -> u64 {
        self.file_offset
    }

    /// The raw records.
    #[must_use]
    pub fn raw(&self) -> &'a [[u8; RELOCATION_SIZE]] {
        self.raw
    }

    /// Decodes relocation `index`.
    #[inline]
    #[must_use]
    pub fn get(&self, index: usize) -> Option<Relocation> {
        self.raw.get(index).map(Relocation::decode)
    }

    /// Iterates over the relocations in file order.
    pub fn iter(
        &self,
    ) -> impl ExactSizeIterator<Item = Relocation> + DoubleEndedIterator + Clone + use<'a> {
        self.raw.iter().map(Relocation::decode)
    }
}
