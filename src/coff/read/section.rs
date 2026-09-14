//! Section headers (`IMAGE_SECTION_HEADER`), shared by objects and images.

use super::consts::{
    IMAGE_SCN_ALIGN_MASK, IMAGE_SCN_CNT_CODE, IMAGE_SCN_CNT_INITIALIZED_DATA,
    IMAGE_SCN_CNT_UNINITIALIZED_DATA, IMAGE_SCN_LNK_COMDAT, IMAGE_SCN_LNK_INFO,
    IMAGE_SCN_LNK_NRELOC_OVFL, IMAGE_SCN_LNK_REMOVE, IMAGE_SCN_MEM_DISCARDABLE,
    IMAGE_SCN_MEM_EXECUTE, IMAGE_SCN_MEM_READ, IMAGE_SCN_MEM_WRITE,
};
use super::source::{array, entry_offset, until_nul};
use super::strtab::StringTable;

/// Size of a section header.
pub const SECTION_HEADER_SIZE: usize = 40;

/// A decoded section header.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct SectionHeader {
    /// The raw 8-byte name field: an inline name padded with NULs, or a
    /// `/<decimal>` or `//<base64>` string table reference.
    pub raw_name: [u8; 8],
    /// `VirtualSize`: size in memory (images; 0 in objects).
    pub virtual_size: u32,
    /// `VirtualAddress`: RVA of the section (images; usually 0 in objects).
    pub virtual_address: u32,
    /// `SizeOfRawData`: size of the contents in the file.
    pub size_of_raw_data: u32,
    /// `PointerToRawData`: file offset of the contents (0 if none).
    pub pointer_to_raw_data: u32,
    /// `PointerToRelocations`: file offset of the relocations.
    pub pointer_to_relocations: u32,
    /// `PointerToLinenumbers`: file offset of legacy line numbers.
    pub pointer_to_linenumbers: u32,
    /// `NumberOfRelocations`, before the `IMAGE_SCN_LNK_NRELOC_OVFL` fixup.
    pub number_of_relocations: u16,
    /// `NumberOfLinenumbers`.
    pub number_of_linenumbers: u16,
    /// `Characteristics` (`IMAGE_SCN_*`).
    pub characteristics: u32,
}

impl SectionHeader {
    /// Decodes a 40-byte header record.
    #[must_use]
    pub fn decode(raw: &[u8; SECTION_HEADER_SIZE]) -> Self {
        let u32_at = |offset: usize| array::<4>(raw, offset).map_or(0, u32::from_le_bytes);
        let u16_at = |offset: usize| array::<2>(raw, offset).map_or(0, u16::from_le_bytes);
        Self {
            raw_name: array::<8>(raw, 0).unwrap_or_default(),
            virtual_size: u32_at(8),
            virtual_address: u32_at(12),
            size_of_raw_data: u32_at(16),
            pointer_to_raw_data: u32_at(20),
            pointer_to_relocations: u32_at(24),
            pointer_to_linenumbers: u32_at(28),
            number_of_relocations: u16_at(32),
            number_of_linenumbers: u16_at(34),
            characteristics: u32_at(36),
        }
    }

    /// Whether the name field refers to the string table.
    #[must_use]
    pub fn has_long_name(&self) -> bool {
        self.raw_name[0] == b'/'
    }

    /// Log2 of the `IMAGE_SCN_ALIGN_*` alignment, or `None` if the field is
    /// 0 (no alignment given) or the reserved value 15.
    #[inline]
    #[must_use]
    pub fn alignment_log2(&self) -> Option<u32> {
        match (self.characteristics & IMAGE_SCN_ALIGN_MASK) >> 20 {
            0 | 15 => None,
            n => Some(n.wrapping_sub(1)),
        }
    }

    /// The `IMAGE_SCN_ALIGN_*` alignment in bytes (1 to 8192), or `None`
    /// if no alignment is given.
    ///
    /// Linkers disagree on the default when it is absent: lld uses 1, while
    /// MSVC and GNU ld use 16 for code and data sections.
    #[inline]
    #[must_use]
    pub fn alignment(&self) -> Option<u32> {
        self.alignment_log2().map(|log2| 1u32 << log2)
    }

    /// Whether `flag` (one or more `IMAGE_SCN_*` bits) is fully set.
    #[inline]
    #[must_use]
    pub fn has(&self, flag: u32) -> bool {
        self.characteristics & flag == flag
    }

    /// `IMAGE_SCN_CNT_CODE`.
    #[inline]
    #[must_use]
    pub fn is_code(&self) -> bool {
        self.has(IMAGE_SCN_CNT_CODE)
    }

    /// `IMAGE_SCN_CNT_INITIALIZED_DATA`.
    #[inline]
    #[must_use]
    pub fn is_initialized_data(&self) -> bool {
        self.has(IMAGE_SCN_CNT_INITIALIZED_DATA)
    }

    /// `IMAGE_SCN_CNT_UNINITIALIZED_DATA`: the section has no file contents.
    #[inline]
    #[must_use]
    pub fn is_uninitialized_data(&self) -> bool {
        self.has(IMAGE_SCN_CNT_UNINITIALIZED_DATA)
    }

    /// `IMAGE_SCN_LNK_COMDAT`.
    #[inline]
    #[must_use]
    pub fn is_comdat(&self) -> bool {
        self.has(IMAGE_SCN_LNK_COMDAT)
    }

    /// `IMAGE_SCN_LNK_REMOVE`: the section is not part of the image.
    #[inline]
    #[must_use]
    pub fn is_remove(&self) -> bool {
        self.has(IMAGE_SCN_LNK_REMOVE)
    }

    /// `IMAGE_SCN_LNK_INFO`: linker information such as `.drectve`.
    #[inline]
    #[must_use]
    pub fn is_info(&self) -> bool {
        self.has(IMAGE_SCN_LNK_INFO)
    }

    /// `IMAGE_SCN_MEM_DISCARDABLE`.
    #[inline]
    #[must_use]
    pub fn is_discardable(&self) -> bool {
        self.has(IMAGE_SCN_MEM_DISCARDABLE)
    }

    /// `IMAGE_SCN_MEM_EXECUTE`.
    #[inline]
    #[must_use]
    pub fn is_executable(&self) -> bool {
        self.has(IMAGE_SCN_MEM_EXECUTE)
    }

    /// `IMAGE_SCN_MEM_READ`.
    #[inline]
    #[must_use]
    pub fn is_readable(&self) -> bool {
        self.has(IMAGE_SCN_MEM_READ)
    }

    /// `IMAGE_SCN_MEM_WRITE`.
    #[inline]
    #[must_use]
    pub fn is_writable(&self) -> bool {
        self.has(IMAGE_SCN_MEM_WRITE)
    }

    /// Whether the relocation count overflowed 16 bits
    /// (`IMAGE_SCN_LNK_NRELOC_OVFL` with a count of `0xFFFF`), so the real
    /// count is stored in the first relocation record.
    #[inline]
    #[must_use]
    pub fn has_extended_relocations(&self) -> bool {
        self.has(IMAGE_SCN_LNK_NRELOC_OVFL) && self.number_of_relocations == u16::MAX
    }
}

/// Resolves a raw 8-byte section name field.
///
/// Inline names are returned without their NUL padding. `/<decimal>` and
/// `//<base64>` (the form `/bigobj` objects and LLVM use for large offsets)
/// are looked up in `strings`. Returns `None` if a reference is invalid.
#[must_use]
pub fn resolve_name<'a>(raw_name: &'a [u8; 8], strings: &StringTable<'a>) -> Option<&'a [u8]> {
    let name = until_nul(raw_name);
    match string_table_offset(name) {
        NameRef::Inline => Some(name),
        NameRef::Offset(offset) => strings.get(offset),
        NameRef::Invalid => None,
    }
}

/// What a section name field refers to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NameRef {
    Inline,
    Offset(u32),
    Invalid,
}

fn string_table_offset(name: &[u8]) -> NameRef {
    let Some(rest) = name.strip_prefix(b"/") else {
        return NameRef::Inline;
    };
    if let Some(encoded) = rest.strip_prefix(b"/") {
        return decode_base64_offset(encoded).map_or(NameRef::Invalid, NameRef::Offset);
    }
    if rest.is_empty() {
        // A bare "/" is an inline name (unusual, but not a reference).
        return NameRef::Inline;
    }
    let mut value: u32 = 0;
    for &b in rest {
        if !b.is_ascii_digit() {
            return NameRef::Invalid;
        }
        let Some(next) = value
            .checked_mul(10)
            .and_then(|v| v.checked_add(u32::from(b.wrapping_sub(b'0'))))
        else {
            return NameRef::Invalid;
        };
        value = next;
    }
    NameRef::Offset(value)
}

/// Decodes the `//<base64>` form: up to six base-64 digits, most significant
/// first, with the alphabet `A-Za-z0-9+/`.
fn decode_base64_offset(encoded: &[u8]) -> Option<u32> {
    if encoded.is_empty() || encoded.len() > 6 {
        return None;
    }
    let mut value: u64 = 0;
    for &b in encoded {
        let digit = match b {
            b'A'..=b'Z' => b.wrapping_sub(b'A'),
            b'a'..=b'z' => b.wrapping_sub(b'a').wrapping_add(26),
            b'0'..=b'9' => b.wrapping_sub(b'0').wrapping_add(52),
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        value = value.checked_mul(64)?.checked_add(u64::from(digit))?;
    }
    u32::try_from(value).ok()
}

/// A section header table: a lazily decoded array of 40-byte records.
#[derive(Clone, Copy, Debug, Default)]
pub struct SectionTable<'a> {
    raw: &'a [[u8; SECTION_HEADER_SIZE]],
    file_offset: u64,
}

impl<'a> SectionTable<'a> {
    /// Wraps raw records found at `file_offset`.
    #[must_use]
    pub fn new(raw: &'a [[u8; SECTION_HEADER_SIZE]], file_offset: u64) -> Self {
        Self { raw, file_offset }
    }

    /// Splits `data` into records, ignoring a trailing partial record.
    #[must_use]
    pub fn from_bytes(data: &'a [u8], file_offset: u64) -> Self {
        let (records, _) = data.as_chunks::<SECTION_HEADER_SIZE>();
        Self::new(records, file_offset)
    }

    /// Number of sections.
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.raw.len()
    }

    /// Whether there are no sections.
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

    /// File offset of the header of section `number` (1-based), saturating.
    #[must_use]
    pub fn header_offset(&self, number: u32) -> u64 {
        entry_offset(
            self.file_offset,
            u64::from(number.saturating_sub(1)),
            SECTION_HEADER_SIZE,
        )
    }

    /// Decodes the header of section `number`. Section numbers are 1-based,
    /// as in symbol records; 0 and numbers past the end return `None`.
    #[inline]
    #[must_use]
    pub fn get(&self, number: u32) -> Option<SectionHeader> {
        let index = usize::try_from(number.checked_sub(1)?).ok()?;
        self.raw.get(index).map(SectionHeader::decode)
    }

    /// Resolves the name of section `number` (1-based); see [`resolve_name`].
    #[must_use]
    pub fn name(&self, number: u32, strings: &StringTable<'a>) -> Option<&'a [u8]> {
        let index = usize::try_from(number.checked_sub(1)?).ok()?;
        let raw: &'a [u8; SECTION_HEADER_SIZE] = self.raw.get(index)?;
        let (name, _) = raw.split_first_chunk::<8>()?;
        resolve_name(name, strings)
    }

    /// The raw 40-byte records.
    #[must_use]
    pub fn raw(&self) -> &'a [[u8; SECTION_HEADER_SIZE]] {
        self.raw
    }

    /// Iterates over the headers in table order (section 1 first).
    pub fn iter(&self) -> impl ExactSizeIterator<Item = SectionHeader> + use<'a> {
        self.raw.iter().map(SectionHeader::decode)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_names() {
        assert_eq!(decode_base64_offset(b"A"), Some(0));
        assert_eq!(decode_base64_offset(b"BA"), Some(64));
        assert_eq!(decode_base64_offset(b"////"), Some(64u32.pow(4) - 1));
        assert_eq!(decode_base64_offset(b"D/////"), Some(u32::MAX));
        assert_eq!(decode_base64_offset(b"E/////"), None);
        assert_eq!(decode_base64_offset(b"AAAAAAA"), None);
        assert_eq!(decode_base64_offset(b""), None);
        assert_eq!(decode_base64_offset(b"A-"), None);
    }

    #[test]
    fn names() {
        let strings = StringTable::new(b"\x10\0\0\0.debug_info\0", 0);
        assert_eq!(resolve_name(b".text\0\0\0", &strings), Some(&b".text"[..]));
        assert_eq!(
            resolve_name(b"/4\0\0\0\0\0\0", &strings),
            Some(&b".debug_info"[..])
        );
        assert_eq!(
            resolve_name(b"//AAAAE\0", &strings),
            Some(&b".debug_info"[..])
        );
        assert_eq!(resolve_name(b"/4x\0\0\0\0\0", &strings), None);
        assert_eq!(resolve_name(b"/9999999", &strings), None);
        assert_eq!(resolve_name(b".abcdefg", &strings), Some(&b".abcdefg"[..]));
    }

    #[test]
    fn alignment() {
        let mut header = SectionHeader::default();
        assert_eq!(header.alignment(), None);
        header.characteristics = 0x0050_0000;
        assert_eq!(header.alignment(), Some(16));
        header.characteristics = 0x00e0_0000;
        assert_eq!(header.alignment(), Some(8192));
        header.characteristics = 0x00f0_0000;
        assert_eq!(header.alignment(), None);
    }
}
