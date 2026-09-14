//! Sections (`section` and `section_64`).

use super::bytes::{Endian, Source, fixed_name, subslice, to_u64};
use super::consts::{
    S_4BYTE_LITERALS, S_8BYTE_LITERALS, S_16BYTE_LITERALS, S_ATTR_DEBUG, S_ATTR_LIVE_SUPPORT,
    S_ATTR_NO_DEAD_STRIP, S_ATTR_PURE_INSTRUCTIONS, S_ATTR_SOME_INSTRUCTIONS, S_CSTRING_LITERALS,
    S_GB_ZEROFILL, S_LITERAL_POINTERS, S_THREAD_LOCAL_ZEROFILL, S_ZEROFILL, SECTION_ATTRIBUTES,
    SECTION_TYPE,
};
use crate::error::Result;

/// Size of `section`.
pub const SECTION_SIZE: usize = 68;
/// Size of `section_64`.
pub const SECTION_64_SIZE: usize = 80;

/// A decoded section header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Section<'a> {
    /// `sectname`, without NUL padding.
    pub sectname: &'a [u8],
    /// `segname`, without NUL padding.
    pub segname: &'a [u8],
    /// `addr`.
    pub addr: u64,
    /// `size`.
    pub size: u64,
    /// `offset`: file offset of the contents.
    pub offset: u32,
    /// `align`, as a power of two.
    pub align: u32,
    /// `reloff`: file offset of the relocation entries.
    pub reloff: u32,
    /// `nreloc`: number of relocation entries.
    pub nreloc: u32,
    /// `flags`: type and attributes.
    pub flags: u32,
    /// `reserved1` (indirect symbol index for stubs and pointers).
    pub reserved1: u32,
    /// `reserved2` (stub size).
    pub reserved2: u32,
    /// `reserved3` (64-bit only).
    pub reserved3: u32,
}

/// What a section holds, for the purpose of splitting it into atoms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiteralKind {
    /// Not a literal section.
    None,
    /// NUL-terminated strings (`S_CSTRING_LITERALS`).
    CString,
    /// Fixed-size literals of this many bytes (`S_4BYTE_LITERALS`, …).
    Fixed(u8),
    /// Pointers to literals (`S_LITERAL_POINTERS`), one per word.
    Pointers,
}

impl<'a> Section<'a> {
    /// Decodes a section record.
    pub(crate) fn decode(record: &'a [u8], endian: Endian, is64: bool) -> Option<Self> {
        let sectname = fixed_name(record.get(0..16)?);
        let segname = fixed_name(record.get(16..32)?);
        let u32_at = |offset| endian.u32(record, offset);
        if is64 {
            Some(Self {
                sectname,
                segname,
                addr: endian.u64(record, 32)?,
                size: endian.u64(record, 40)?,
                offset: u32_at(48)?,
                align: u32_at(52)?,
                reloff: u32_at(56)?,
                nreloc: u32_at(60)?,
                flags: u32_at(64)?,
                reserved1: u32_at(68)?,
                reserved2: u32_at(72)?,
                reserved3: u32_at(76)?,
            })
        } else {
            Some(Self {
                sectname,
                segname,
                addr: u64::from(u32_at(32)?),
                size: u64::from(u32_at(36)?),
                offset: u32_at(40)?,
                align: u32_at(44)?,
                reloff: u32_at(48)?,
                nreloc: u32_at(52)?,
                flags: u32_at(56)?,
                reserved1: u32_at(60)?,
                reserved2: u32_at(64)?,
                reserved3: 0,
            })
        }
    }

    /// The section type (`flags & SECTION_TYPE`).
    #[inline]
    #[must_use]
    pub fn section_type(&self) -> u32 {
        self.flags & SECTION_TYPE
    }

    /// The attributes (`flags & SECTION_ATTRIBUTES`).
    #[inline]
    #[must_use]
    pub fn attributes(&self) -> u32 {
        self.flags & SECTION_ATTRIBUTES
    }

    /// Whether the section has no file contents (`S_ZEROFILL`,
    /// `S_GB_ZEROFILL`, `S_THREAD_LOCAL_ZEROFILL`).
    #[must_use]
    pub fn is_zerofill(&self) -> bool {
        matches!(
            self.section_type(),
            S_ZEROFILL | S_GB_ZEROFILL | S_THREAD_LOCAL_ZEROFILL
        )
    }

    /// Whether the section holds DWARF debug information (`S_ATTR_DEBUG`).
    #[must_use]
    pub fn is_debug(&self) -> bool {
        self.flags & S_ATTR_DEBUG != 0
    }

    /// Whether the section contains code.
    #[must_use]
    pub fn has_code(&self) -> bool {
        self.flags & (S_ATTR_PURE_INSTRUCTIONS | S_ATTR_SOME_INSTRUCTIONS) != 0
    }

    /// Whether dead stripping must keep the whole section
    /// (`S_ATTR_NO_DEAD_STRIP`).
    #[must_use]
    pub fn is_no_dead_strip(&self) -> bool {
        self.flags & S_ATTR_NO_DEAD_STRIP != 0
    }

    /// Whether the section is live when anything it references is live
    /// (`S_ATTR_LIVE_SUPPORT`).
    #[must_use]
    pub fn is_live_support(&self) -> bool {
        self.flags & S_ATTR_LIVE_SUPPORT != 0
    }

    /// How the contents split into literals.
    #[must_use]
    pub fn literal_kind(&self) -> LiteralKind {
        match self.section_type() {
            S_CSTRING_LITERALS => LiteralKind::CString,
            S_4BYTE_LITERALS => LiteralKind::Fixed(4),
            S_8BYTE_LITERALS => LiteralKind::Fixed(8),
            S_16BYTE_LITERALS => LiteralKind::Fixed(16),
            S_LITERAL_POINTERS => LiteralKind::Pointers,
            _ => LiteralKind::None,
        }
    }

    /// Whether this is `segname,sectname`.
    #[must_use]
    pub fn is(&self, segname: &[u8], sectname: &[u8]) -> bool {
        self.segname == segname && self.sectname == sectname
    }

    /// Whether `addr` (an address in the object) lies within the section,
    /// counting the end address as inside.
    #[must_use]
    pub fn contains_address(&self, addr: u64) -> bool {
        addr >= self.addr && addr.wrapping_sub(self.addr) <= self.size
    }

    /// The section contents. Zero-fill sections return an empty slice.
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` if the contents lie outside `file`.
    pub fn data(&self, file: &'a [u8], source: Source<'_>) -> Result<&'a [u8]> {
        if self.is_zerofill() {
            return Ok(&[]);
        }
        subslice(file, u64::from(self.offset), self.size).ok_or_else(|| {
            source.malformed(
                u64::from(self.offset),
                format!(
                    "contents of section {},{} (extends past end of file)",
                    String::from_utf8_lossy(self.segname),
                    String::from_utf8_lossy(self.sectname)
                ),
            )
        })
    }

    /// The raw relocation entries.
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` if they lie outside `file`.
    pub fn relocation_bytes(&self, file: &'a [u8], source: Source<'_>) -> Result<&'a [u8]> {
        let size = u64::from(self.nreloc).saturating_mul(8);
        subslice(file, u64::from(self.reloff), size).ok_or_else(|| {
            source.malformed(
                u64::from(self.reloff),
                format!(
                    "relocations of section {},{} (extend past end of file)",
                    String::from_utf8_lossy(self.segname),
                    String::from_utf8_lossy(self.sectname)
                ),
            )
        })
    }

    /// Display name `segname,sectname`.
    #[must_use]
    pub fn display_name(&self) -> String {
        format!(
            "{},{}",
            String::from_utf8_lossy(self.segname),
            String::from_utf8_lossy(self.sectname)
        )
    }
}

/// The section records following a segment command.
#[derive(Clone, Copy, Debug)]
pub struct SectionTable<'a> {
    pub(crate) data: &'a [u8],
    pub(crate) file_offset: u64,
    pub(crate) endian: Endian,
    pub(crate) is64: bool,
}

impl<'a> SectionTable<'a> {
    /// Size of one record.
    #[must_use]
    pub fn entry_size(&self) -> usize {
        if self.is64 {
            SECTION_64_SIZE
        } else {
            SECTION_SIZE
        }
    }

    /// Number of sections.
    #[must_use]
    pub fn len(&self) -> usize {
        self.data.len().checked_div(self.entry_size()).unwrap_or(0)
    }

    /// Whether there are no sections.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// File offset of record `index`.
    #[must_use]
    pub fn record_offset(&self, index: usize) -> u64 {
        self.file_offset
            .saturating_add(to_u64(index).saturating_mul(to_u64(self.entry_size())))
    }

    /// Decodes section `index`.
    #[must_use]
    pub fn get(&self, index: usize) -> Option<Section<'a>> {
        let size = self.entry_size();
        let start = index.checked_mul(size)?;
        let record = self.data.get(start..)?.get(..size)?;
        Section::decode(record, self.endian, self.is64)
    }

    /// Iterates over the sections.
    pub fn iter(&self) -> impl Iterator<Item = Section<'a>> + use<'a> {
        let (endian, is64) = (self.endian, self.is64);
        self.data
            .chunks_exact(self.entry_size())
            .filter_map(move |record| Section::decode(record, endian, is64))
    }
}
