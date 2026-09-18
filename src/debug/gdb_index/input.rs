//! The debug sections of one relocatable input object, read with their
//! relocations applied, for the index builders ([`super`] and
//! `.debug_names`).
//!
//! In an `ET_REL` object, offsets into other debug sections and code
//! addresses are relocation targets: the field holds zero (RELA) or the
//! addend (REL) and the relocation names a symbol, usually a section
//! symbol. [`DebugObject::relocate`] turns a field into `symbol value +
//! addend` and reports the section the symbol is defined in, as lld's
//! `LLDDwarfObj` does, so that an address becomes (section index, offset).

use std::cell::Cell;

use crate::elf::object::ObjectInput;
use crate::elf::read::consts::SHF_GROUP;
use crate::elf::read::{Elf64Le, Relocation, Relocations, SectionIndex};
use crate::error::Result;

/// A debug section with its relocations.
#[derive(Clone, Debug)]
pub struct Section<'a> {
    /// Section index in the object.
    pub index: u32,
    /// Contents (decompressed).
    pub data: &'a [u8],
    relocs: Relocs<'a>,
    /// Where the last lookup ended: readers mostly go forward, so the next
    /// relocation is usually at or just after it.
    hint: Cell<usize>,
}

impl Section<'_> {
    /// Index of the first relocation at or after `offset`: a few steps
    /// forward from the last lookup, else a binary search.
    fn find(&self, offset: u64) -> usize {
        let hint = self.hint.get();
        let before = |i: usize| self.relocs.get(i).is_some_and(|r| r.offset < offset);
        let at = if hint == 0 || before(hint.saturating_sub(1)) {
            let mut at = hint;
            let mut steps = 0u32;
            while steps < 8 && before(at) {
                at = at.saturating_add(1);
                steps = steps.saturating_add(1);
            }
            if before(at) {
                self.relocs.lower_bound(offset)
            } else {
                at
            }
        } else {
            self.relocs.lower_bound(offset)
        };
        self.hint.set(at);
        at
    }
}

/// The relocations of a section, searchable by offset.
#[derive(Clone, Debug)]
enum Relocs<'a> {
    None,
    /// Already sorted by offset in the file (the usual case).
    Sorted(Relocations<'a, Elf64Le>),
    /// Sorted copies.
    Copied(Vec<Relocation>, bool),
}

impl Relocs<'_> {
    fn len(&self) -> usize {
        match self {
            Self::None => 0,
            Self::Sorted(r) => r.len(),
            Self::Copied(r, _) => r.len(),
        }
    }

    fn get(&self, index: usize) -> Option<Relocation> {
        match self {
            Self::None => None,
            Self::Sorted(r) => r.get(index),
            Self::Copied(r, _) => r.get(index).copied(),
        }
    }

    fn is_rela(&self) -> bool {
        match self {
            Self::None => true,
            Self::Sorted(r) => r.is_rela(),
            Self::Copied(_, rela) => *rela,
        }
    }

    /// Index of the first relocation at or after `offset`.
    fn lower_bound(&self, offset: u64) -> usize {
        let (mut low, mut high) = (0usize, self.len());
        while low < high {
            let mid = low.midpoint(high);
            match self.get(mid) {
                Some(r) if r.offset < offset => low = mid.saturating_add(1),
                _ => high = mid,
            }
        }
        low
    }
}

/// The debug sections of an object that the index builders read.
pub struct DebugObject<'o, 'a> {
    /// The object.
    pub object: &'o ObjectInput<'a>,
    /// The `.debug_info` section outside section groups (the last one, as
    /// lld picks it); groups hold DWARF 5 type units.
    pub info: Option<Section<'a>>,
    /// `.debug_abbrev`.
    pub abbrev: Option<Section<'a>>,
    /// `.debug_str`.
    pub str: Option<Section<'a>>,
    /// `.debug_line_str`.
    pub line_str: Option<Section<'a>>,
    /// `.debug_str_offsets`.
    pub str_offsets: Option<Section<'a>>,
    /// `.debug_addr`.
    pub addr: Option<Section<'a>>,
    /// `.debug_ranges`.
    pub ranges: Option<Section<'a>>,
    /// `.debug_rnglists`.
    pub rnglists: Option<Section<'a>>,
    /// `.debug_gnu_pubnames`.
    pub gnu_pubnames: Option<Section<'a>>,
    /// `.debug_gnu_pubtypes`.
    pub gnu_pubtypes: Option<Section<'a>>,
    /// `.debug_names` sections (live ones), in section order.
    pub names: Vec<Section<'a>>,
    /// Sections that hold type units: live `.debug_info` sections in
    /// groups (DWARF 5) and live `.debug_types` sections (DWARF 4), with
    /// whether they are `.debug_types`.
    pub type_units: Vec<(Section<'a>, bool)>,
    /// Whether the object has any live `.debug_info` section (in a group
    /// or not).
    pub has_info: bool,
}

impl<'o, 'a> DebugObject<'o, 'a> {
    /// Collects the debug sections of `object`; `live` tells whether a
    /// section (by index) is in the output.
    ///
    /// # Errors
    ///
    /// Returns [`crate::Error::Malformed`] if a relocation section cannot be
    /// read.
    pub fn new(object: &'o ObjectInput<'a>, live: &dyn Fn(u32) -> bool) -> Result<Self> {
        let mut this = Self {
            object,
            info: None,
            abbrev: None,
            str: None,
            line_str: None,
            str_offsets: None,
            addr: None,
            ranges: None,
            rnglists: None,
            gnu_pubnames: None,
            gnu_pubtypes: None,
            names: Vec::new(),
            type_units: Vec::new(),
            has_info: false,
        };
        for (index, section) in object.sections.iter().enumerate() {
            let Ok(index) = u32::try_from(index) else {
                break;
            };
            if !section.name.starts_with(b".debug_") || section.is_alloc() {
                continue;
            }
            let slot = match section.name {
                b".debug_info" => {
                    if !live(index) {
                        continue;
                    }
                    this.has_info = true;
                    if section.header.sh_flags & SHF_GROUP != 0 {
                        let loaded = this.load(index)?;
                        this.type_units.push((loaded, false));
                        continue;
                    }
                    &mut this.info
                }
                b".debug_types" => {
                    if live(index) {
                        let loaded = this.load(index)?;
                        this.type_units.push((loaded, true));
                    }
                    continue;
                }
                b".debug_abbrev" => &mut this.abbrev,
                b".debug_str" => &mut this.str,
                b".debug_line_str" => &mut this.line_str,
                b".debug_str_offsets" => &mut this.str_offsets,
                b".debug_addr" => &mut this.addr,
                b".debug_ranges" => &mut this.ranges,
                b".debug_rnglists" => &mut this.rnglists,
                b".debug_gnu_pubnames" => &mut this.gnu_pubnames,
                b".debug_gnu_pubtypes" => &mut this.gnu_pubtypes,
                b".debug_names" => {
                    if live(index) {
                        let loaded = this.load(index)?;
                        this.names.push(loaded);
                    }
                    continue;
                }
                _ => continue,
            };
            *slot = None;
            let loaded = Self::load_from(object, index)?;
            match section.name {
                b".debug_info" => this.info = Some(loaded),
                b".debug_abbrev" => this.abbrev = Some(loaded),
                b".debug_str" => this.str = Some(loaded),
                b".debug_line_str" => this.line_str = Some(loaded),
                b".debug_str_offsets" => this.str_offsets = Some(loaded),
                b".debug_addr" => this.addr = Some(loaded),
                b".debug_ranges" => this.ranges = Some(loaded),
                b".debug_rnglists" => this.rnglists = Some(loaded),
                b".debug_gnu_pubnames" => this.gnu_pubnames = Some(loaded),
                _ => this.gnu_pubtypes = Some(loaded),
            }
        }
        Ok(this)
    }

    fn load(&self, index: u32) -> Result<Section<'a>> {
        Self::load_from(self.object, index)
    }

    /// Loads section `index` of `object` with its relocations.
    fn load_from(object: &ObjectInput<'a>, index: u32) -> Result<Section<'a>> {
        let Some(section) = object.section(index) else {
            return Ok(Section {
                index,
                data: &[],
                relocs: Relocs::None,
                hint: Cell::new(0),
            });
        };
        let data = object.section_data(section)?;
        let mut relocs = Relocs::None;
        if section.relocs != 0 {
            let header = object.elf.section_header(section.relocs)?;
            if let Some(rel) = object.elf.relocation_section(section.relocs, &header)? {
                let list = rel.relocations;
                let mut sorted = true;
                let mut previous = 0u64;
                for i in 0..list.len() {
                    let Some(r) = list.get(i) else { break };
                    if r.offset < previous {
                        sorted = false;
                        break;
                    }
                    previous = r.offset;
                }
                relocs = if sorted {
                    Relocs::Sorted(list)
                } else {
                    let mut copied: Vec<Relocation> =
                        (0..list.len()).filter_map(|i| list.get(i)).collect();
                    // Stable: relocations at one offset keep their order.
                    copied.sort_by_key(|r| r.offset);
                    Relocs::Copied(copied, list.is_rela())
                };
            }
        }
        Ok(Section {
            index,
            data,
            relocs,
            hint: Cell::new(0),
        })
    }

    /// The contents of section `index`, for relocation targets outside the
    /// sections found by name.
    #[must_use]
    pub fn data_of(&self, index: u32) -> Option<&'a [u8]> {
        let section = self.object.section(index)?;
        self.object.section_data(section).ok()
    }

    /// Applies the relocation at `offset` of `section` to the raw field
    /// value `raw`: returns `symbol value + addend` and the index of the
    /// section the symbol is defined in, or `raw` and `None` without a
    /// relocation.
    #[must_use]
    pub fn relocate(&self, section: &Section<'_>, offset: usize, raw: u64) -> (u64, Option<u32>) {
        let Ok(offset) = u64::try_from(offset) else {
            return (raw, None);
        };
        let at = section.find(offset);
        let Some(reloc) = section.relocs.get(at).filter(|r| r.offset == offset) else {
            return (raw, None);
        };
        let symbols = self.object.elf.symbols();
        let Some(sym) = symbols.get_raw(reloc.symbol as usize) else {
            return (raw, None);
        };
        let addend = if section.relocs.is_rela() {
            reloc.addend as u64
        } else {
            raw
        };
        match symbols.section_of(reloc.symbol as usize, &sym) {
            Some(SectionIndex::Section(target)) => {
                (sym.st_value.wrapping_add(addend), Some(target))
            }
            Some(SectionIndex::Absolute) => (sym.st_value.wrapping_add(addend), None),
            // An undefined symbol (lld uses the resolved definition's value
            // with no section, which callers then skip).
            _ => (addend, None),
        }
    }
}

/// A bounds-checked little-endian cursor over DWARF data.
#[derive(Clone, Copy, Debug)]
pub struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

/// A decoding failure: the offset where it happened, and what was wrong.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Malformed {
    /// Offset within the section.
    pub offset: usize,
    /// What was wrong.
    pub what: &'static str,
}

/// Result of the readers.
pub type Parsed<T> = core::result::Result<T, Malformed>;

impl<'a> Reader<'a> {
    /// A reader at `pos` of `data` (reads fail if it is out of range).
    #[must_use]
    pub fn at(data: &'a [u8], pos: usize) -> Self {
        Self { data, pos }
    }

    /// The current position.
    #[must_use]
    pub fn pos(&self) -> usize {
        self.pos
    }

    /// The data (the whole section).
    #[must_use]
    pub fn data(&self) -> &'a [u8] {
        self.data
    }

    /// Whether the position is at or past the end.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pos >= self.data.len()
    }

    /// An error at the current position.
    #[cold]
    #[must_use]
    pub fn error(&self, what: &'static str) -> Malformed {
        Malformed {
            offset: self.pos,
            what,
        }
    }

    /// Reads `n` bytes.
    ///
    /// # Errors
    ///
    /// Fails past the end of the data.
    pub fn bytes(&mut self, n: usize) -> Parsed<&'a [u8]> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or_else(|| self.error("truncated"))?;
        let bytes = self
            .data
            .get(self.pos..end)
            .ok_or_else(|| self.error("truncated"))?;
        self.pos = end;
        Ok(bytes)
    }

    /// Skips `n` bytes.
    ///
    /// # Errors
    ///
    /// Fails past the end of the data.
    pub fn skip(&mut self, n: u64) -> Parsed<()> {
        let n = usize::try_from(n).map_err(|_| self.error("truncated"))?;
        self.bytes(n).map(|_| ())
    }

    /// Reads a byte.
    ///
    /// # Errors
    ///
    /// Fails past the end of the data.
    pub fn u8(&mut self) -> Parsed<u8> {
        let b = *self
            .data
            .get(self.pos)
            .ok_or_else(|| self.error("truncated"))?;
        self.pos = self.pos.saturating_add(1);
        Ok(b)
    }

    /// Reads a little-endian unsigned integer of 1 to 8 bytes.
    ///
    /// # Errors
    ///
    /// Fails past the end of the data or for other sizes.
    pub fn uint(&mut self, size: usize) -> Parsed<u64> {
        if !(1..=8).contains(&size) {
            return Err(self.error("integer size"));
        }
        let bytes = self.bytes(size)?;
        let mut buf = [0u8; 8];
        if let Some(dst) = buf.get_mut(..size) {
            dst.copy_from_slice(bytes);
        }
        Ok(u64::from_le_bytes(buf))
    }

    /// Reads a `u16`.
    ///
    /// # Errors
    ///
    /// Fails past the end of the data.
    pub fn u16(&mut self) -> Parsed<u16> {
        self.uint(2).map(|v| v as u16)
    }

    /// Reads a `u32`.
    ///
    /// # Errors
    ///
    /// Fails past the end of the data.
    pub fn u32(&mut self) -> Parsed<u32> {
        self.uint(4).map(|v| v as u32)
    }

    /// Reads an unsigned LEB128 number (bits past 64 are dropped).
    ///
    /// # Errors
    ///
    /// Fails past the end of the data.
    pub fn uleb(&mut self) -> Parsed<u64> {
        let mut value = 0u64;
        let mut shift = 0u32;
        loop {
            let byte = self.u8()?;
            if shift < 64 {
                value |= u64::from(byte & 0x7f) << shift;
            }
            if byte & 0x80 == 0 {
                return Ok(value);
            }
            shift = shift.saturating_add(7);
        }
    }

    /// Reads a signed LEB128 number.
    ///
    /// # Errors
    ///
    /// Fails past the end of the data.
    pub fn sleb(&mut self) -> Parsed<i64> {
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
    ///
    /// # Errors
    ///
    /// Fails if the string is not terminated.
    pub fn cstr(&mut self) -> Parsed<&'a [u8]> {
        let rest = self.data.get(self.pos..).unwrap_or_default();
        let len = rest
            .iter()
            .position(|&b| b == 0)
            .ok_or_else(|| self.error("unterminated string"))?;
        let s = self.bytes(len)?;
        self.pos = self.pos.saturating_add(1);
        Ok(s)
    }

    /// Reads an initial length field: the length and the offset size (4
    /// or 8).
    ///
    /// # Errors
    ///
    /// Fails past the end of the data or for reserved values.
    pub fn initial_length(&mut self) -> Parsed<(u64, usize)> {
        let length = self.u32()?;
        match length {
            0xffff_ffff => Ok((self.uint(8)?, 8)),
            0xffff_fff0..=0xffff_fffe => Err(self.error("unit length (reserved value)")),
            _ => Ok((u64::from(length), 4)),
        }
    }
}

/// The NUL-terminated string at `offset` of `data`.
#[must_use]
pub fn string_at(data: &[u8], offset: u64) -> Option<&[u8]> {
    let rest = data.get(usize::try_from(offset).ok()?..)?;
    let len = rest.iter().position(|&b| b == 0)?;
    rest.get(..len)
}
