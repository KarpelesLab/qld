//! Section groups (`SHT_GROUP`), including COMDAT groups.

use core::marker::PhantomData;

use super::consts::GRP_COMDAT;
use super::format::{ElfFormat, Endian};
use super::section::SectionHeader;

/// A section group.
///
/// The signature is the name of symbol `signature_symbol` in symbol table
/// `symtab`; see [`ObjectFile::group_signature`](super::ObjectFile::group_signature).
#[derive(Debug)]
pub struct Group<'a, F: ElfFormat> {
    /// Index of the `SHT_GROUP` section.
    pub index: u32,
    /// Its header.
    pub header: SectionHeader,
    /// Group flags (`GRP_*`).
    pub flags: u32,
    /// Symbol table the signature symbol lives in (`sh_link`).
    pub symtab: u32,
    /// Index of the signature symbol (`sh_info`).
    pub signature_symbol: u32,
    members: &'a [[u8; 4]],
    _format: PhantomData<F>,
}

impl<F: ElfFormat> Clone for Group<'_, F> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<F: ElfFormat> Copy for Group<'_, F> {}

impl<'a, F: ElfFormat> Group<'a, F> {
    /// Builds a group from its section header and the section contents,
    /// which must be whole 4-byte words with at least the flags word.
    pub(crate) fn new(index: u32, header: SectionHeader, words: &'a [[u8; 4]]) -> Option<Self> {
        let (flags, members) = words.split_first()?;
        Some(Self {
            index,
            header,
            flags: F::Endian::u32(*flags),
            symtab: header.sh_link,
            signature_symbol: header.sh_info,
            members,
            _format: PhantomData,
        })
    }

    /// Whether this is a COMDAT group.
    #[must_use]
    pub fn is_comdat(&self) -> bool {
        self.flags & GRP_COMDAT != 0
    }

    /// Number of member sections.
    #[must_use]
    pub fn member_count(&self) -> usize {
        self.members.len()
    }

    /// Iterates over the member section indices, as stored (not validated).
    pub fn members(&self) -> impl ExactSizeIterator<Item = u32> + use<'a, F> {
        self.members.iter().map(|w| F::Endian::u32(*w))
    }
}
