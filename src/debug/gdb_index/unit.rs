//! Units of `.debug_info`: headers, abbreviation tables, attribute values,
//! and the address ranges of a compilation unit (lld's
//! `DWARFUnit::collectAddressRanges`: the unit DIE's `DW_AT_low_pc` and
//! `DW_AT_high_pc`, or its `DW_AT_ranges` list in `.debug_ranges` or
//! `.debug_rnglists`).

use super::input::{DebugObject, Malformed, Parsed, Reader, Section, string_at};

// Attribute forms (DWARF 5 section 7.5.6, plus GNU extensions).
pub(crate) const DW_FORM_ADDR: u64 = 0x01;
const DW_FORM_BLOCK2: u64 = 0x03;
const DW_FORM_BLOCK4: u64 = 0x04;
const DW_FORM_DATA2: u64 = 0x05;
const DW_FORM_DATA4: u64 = 0x06;
const DW_FORM_DATA8: u64 = 0x07;
const DW_FORM_STRING: u64 = 0x08;
const DW_FORM_BLOCK: u64 = 0x09;
const DW_FORM_BLOCK1: u64 = 0x0a;
const DW_FORM_DATA1: u64 = 0x0b;
const DW_FORM_FLAG: u64 = 0x0c;
const DW_FORM_SDATA: u64 = 0x0d;
const DW_FORM_STRP: u64 = 0x0e;
const DW_FORM_UDATA: u64 = 0x0f;
const DW_FORM_REF_ADDR: u64 = 0x10;
const DW_FORM_REF1: u64 = 0x11;
const DW_FORM_REF2: u64 = 0x12;
const DW_FORM_REF4: u64 = 0x13;
const DW_FORM_REF8: u64 = 0x14;
const DW_FORM_REF_UDATA: u64 = 0x15;
const DW_FORM_INDIRECT: u64 = 0x16;
const DW_FORM_SEC_OFFSET: u64 = 0x17;
const DW_FORM_EXPRLOC: u64 = 0x18;
pub(crate) const DW_FORM_FLAG_PRESENT: u64 = 0x19;
const DW_FORM_STRX: u64 = 0x1a;
pub(crate) const DW_FORM_ADDRX: u64 = 0x1b;
const DW_FORM_REF_SUP4: u64 = 0x1c;
const DW_FORM_STRP_SUP: u64 = 0x1d;
const DW_FORM_DATA16: u64 = 0x1e;
const DW_FORM_LINE_STRP: u64 = 0x1f;
pub(crate) const DW_FORM_REF_SIG8: u64 = 0x20;
const DW_FORM_IMPLICIT_CONST: u64 = 0x21;
const DW_FORM_LOCLISTX: u64 = 0x22;
const DW_FORM_RNGLISTX: u64 = 0x23;
const DW_FORM_REF_SUP8: u64 = 0x24;
const DW_FORM_STRX1: u64 = 0x25;
const DW_FORM_STRX2: u64 = 0x26;
const DW_FORM_STRX3: u64 = 0x27;
const DW_FORM_STRX4: u64 = 0x28;
const DW_FORM_ADDRX1: u64 = 0x29;
const DW_FORM_ADDRX2: u64 = 0x2a;
const DW_FORM_ADDRX3: u64 = 0x2b;
const DW_FORM_ADDRX4: u64 = 0x2c;
const DW_FORM_GNU_ADDR_INDEX: u64 = 0x1f01;
const DW_FORM_GNU_STR_INDEX: u64 = 0x1f02;
const DW_FORM_GNU_REF_ALT: u64 = 0x1f20;
const DW_FORM_GNU_STRP_ALT: u64 = 0x1f21;

// Attributes.
pub(crate) const DW_AT_NAME: u64 = 0x03;
const DW_AT_LOW_PC: u64 = 0x11;
const DW_AT_HIGH_PC: u64 = 0x12;
pub(crate) const DW_AT_LANGUAGE: u64 = 0x13;
const DW_AT_ENTRY_PC: u64 = 0x52;
const DW_AT_RANGES: u64 = 0x55;
const DW_AT_STR_OFFSETS_BASE: u64 = 0x72;
const DW_AT_ADDR_BASE: u64 = 0x73;
const DW_AT_RNGLISTS_BASE: u64 = 0x74;
const DW_AT_GNU_ADDR_BASE: u64 = 0x2133;

// Unit types (DWARF 5).
const DW_UT_TYPE: u8 = 0x02;
const DW_UT_SKELETON: u8 = 0x04;
const DW_UT_SPLIT_COMPILE: u8 = 0x05;
const DW_UT_SPLIT_TYPE: u8 = 0x06;

// Range list entries (DWARF 5).
const DW_RLE_END_OF_LIST: u8 = 0x00;
const DW_RLE_BASE_ADDRESSX: u8 = 0x01;
const DW_RLE_STARTX_ENDX: u8 = 0x02;
const DW_RLE_STARTX_LENGTH: u8 = 0x03;
const DW_RLE_OFFSET_PAIR: u8 = 0x04;
const DW_RLE_BASE_ADDRESS: u8 = 0x05;
const DW_RLE_START_END: u8 = 0x06;
const DW_RLE_START_LENGTH: u8 = 0x07;

/// A unit header of `.debug_info`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct UnitHeader {
    /// Offset of the unit in the section.
    pub offset: usize,
    /// The unit's `unit_length` field.
    pub length: u64,
    /// DWARF version.
    pub version: u16,
    /// Unit type (`DW_UT_*`; `DW_UT_compile` for DWARF 4 and older).
    pub unit_type: u8,
    /// 4 or 8.
    pub offset_size: usize,
    /// Address size.
    pub address_size: usize,
    /// `debug_abbrev_offset` and the section its relocation points to.
    pub abbrev: (u64, Option<u32>),
    /// For type units, the type signature.
    pub signature: u64,
    /// For type units, the offset of the type's DIE in the unit.
    pub type_offset: u64,
    /// Offset of the first DIE.
    pub first_die: usize,
    /// Offset of the end of the unit.
    pub end: usize,
}

impl UnitHeader {
    /// Whether this is a type unit (not listed as a compilation unit).
    pub(crate) fn is_type_unit(&self) -> bool {
        matches!(self.unit_type, DW_UT_TYPE | DW_UT_SPLIT_TYPE)
    }
}

/// Reads the unit header at `offset` of `section`; `types` is set for a
/// `.debug_types` section (DWARF 4 type units).
pub(crate) fn unit_header<F: crate::elf::read::ElfFormat>(
    obj: &DebugObject<'_, '_, F>,
    section: &Section<'_, F>,
    offset: usize,
    types: bool,
) -> Parsed<UnitHeader> {
    let mut r = Reader::at(section.data, offset);
    let (length, offset_size) = r.initial_length()?;
    let start = r.pos();
    let end = usize::try_from(length)
        .ok()
        .and_then(|l| start.checked_add(l))
        .filter(|&e| e <= section.data.len())
        .ok_or_else(|| r.error("unit length (past the end of the section)"))?;
    let version = r.u16()?;
    if !(2..=5).contains(&version) {
        return Err(r.error("unit version (unsupported)"));
    }
    let mut signature = 0u64;
    let mut type_offset = 0u64;
    let (unit_type, address_size, abbrev) = if version >= 5 {
        let unit_type = r.u8()?;
        let address_size = r.u8()?;
        let pos = r.pos();
        let raw = r.uint(offset_size)?;
        let abbrev = obj.relocate(section, pos, raw);
        match unit_type {
            DW_UT_SKELETON | DW_UT_SPLIT_COMPILE => r.skip(8)?,
            DW_UT_TYPE | DW_UT_SPLIT_TYPE => {
                signature = r.uint(8)?;
                type_offset = r.uint(offset_size)?;
            }
            1 | 3 => {}
            _ => return Err(r.error("unit type (unknown)")),
        }
        (unit_type, address_size, abbrev)
    } else {
        let pos = r.pos();
        let raw = r.uint(offset_size)?;
        let abbrev = obj.relocate(section, pos, raw);
        let address_size = r.u8()?;
        if types {
            signature = r.uint(8)?;
            type_offset = r.uint(offset_size)?;
            (DW_UT_TYPE, address_size, abbrev)
        } else {
            (1, address_size, abbrev)
        }
    };
    if !matches!(address_size, 1 | 2 | 4 | 8) {
        return Err(r.error("address size"));
    }
    Ok(UnitHeader {
        offset,
        length,
        version,
        unit_type,
        offset_size,
        address_size: usize::from(address_size),
        abbrev,
        signature,
        type_offset,
        first_die: r.pos(),
        end,
    })
}

/// Every unit header of `section`; a malformed unit ends the list (with
/// the error).
pub(crate) fn unit_headers<F: crate::elf::read::ElfFormat>(
    obj: &DebugObject<'_, '_, F>,
    section: &Section<'_, F>,
    types: bool,
) -> (Vec<UnitHeader>, Option<Malformed>) {
    let mut units = Vec::new();
    let mut offset = 0usize;
    while offset < section.data.len() {
        match unit_header(obj, section, offset, types) {
            Ok(unit) => {
                offset = unit.end;
                units.push(unit);
            }
            Err(e) => return (units, Some(e)),
        }
    }
    (units, None)
}

/// One abbreviation declaration.
#[derive(Clone, Debug, Default)]
pub(crate) struct Abbrev {
    /// Its tag.
    pub tag: u64,
    /// Whether DIEs using it have children.
    pub children: bool,
    /// (attribute, form, implicit constant).
    pub attrs: Vec<(u64, u64, i64)>,
}

/// An abbreviation table.
#[derive(Clone, Debug, Default)]
pub(crate) struct AbbrevTable {
    /// Declarations by code, when codes are dense from 1.
    dense: Vec<Abbrev>,
    /// Other declarations, sorted by code.
    sparse: Vec<(u64, Abbrev)>,
}

impl AbbrevTable {
    /// Parses the table at `offset` of `data`.
    pub(crate) fn parse(data: &[u8], offset: u64) -> Parsed<Self> {
        let start = usize::try_from(offset).unwrap_or(usize::MAX);
        let mut r = Reader::at(data, start);
        if start > data.len() {
            return Err(r.error("abbreviation offset (out of range)"));
        }
        let mut table = Self::default();
        loop {
            let code = r.uleb()?;
            if code == 0 {
                break;
            }
            let tag = r.uleb()?;
            let children = r.u8()? != 0;
            let mut attrs = Vec::new();
            loop {
                let at = r.uleb()?;
                let form = r.uleb()?;
                if at == 0 && form == 0 {
                    break;
                }
                let implicit = if form == DW_FORM_IMPLICIT_CONST {
                    r.sleb()?
                } else {
                    0
                };
                attrs.push((at, form, implicit));
            }
            let abbrev = Abbrev {
                tag,
                children,
                attrs,
            };
            if table.sparse.is_empty() && code == (table.dense.len() as u64).saturating_add(1) {
                table.dense.push(abbrev);
            } else {
                table.sparse.push((code, abbrev));
            }
        }
        table.sparse.sort_by_key(|(code, _)| *code);
        Ok(table)
    }

    /// The declaration of `code`.
    pub(crate) fn get(&self, code: u64) -> Option<&Abbrev> {
        if let Some(index) = code.checked_sub(1).and_then(|i| usize::try_from(i).ok())
            && let Some(abbrev) = self.dense.get(index)
        {
            return Some(abbrev);
        }
        let at = self.sparse.binary_search_by_key(&code, |(c, _)| *c).ok()?;
        self.sparse.get(at).map(|(_, a)| a)
    }
}

/// Abbreviation tables of one object, by offset.
#[derive(Default)]
pub(crate) struct Abbrevs {
    tables: Vec<((u64, Option<u32>), AbbrevTable)>,
}

impl Abbrevs {
    /// The table unit `unit` uses, parsed on first use.
    pub(crate) fn get<F: crate::elf::read::ElfFormat>(
        &mut self,
        obj: &DebugObject<'_, '_, F>,
        unit: &UnitHeader,
    ) -> core::result::Result<&AbbrevTable, Malformed> {
        let key = unit.abbrev;
        let at = match self.tables.iter().position(|(k, _)| *k == key) {
            Some(at) => at,
            None => {
                let data = match key.1 {
                    Some(section) => obj.data_of(section).unwrap_or_default(),
                    None => obj.abbrev.as_ref().map_or(&[][..], |s| s.data),
                };
                self.tables.push((key, AbbrevTable::parse(data, key.0)?));
                self.tables.len().saturating_sub(1)
            }
        };
        self.tables.get(at).map(|(_, t)| t).ok_or(Malformed {
            offset: 0,
            what: "abbreviation table",
        })
    }
}

/// A decoded attribute value.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Value<'a> {
    /// A constant, address, reference or section offset, with the section
    /// its relocation points into.
    Unsigned(u64, Option<u32>),
    /// A signed constant.
    Signed(i64),
    /// A `DW_FORM_addr` address, with the section of its relocation.
    Addr(u64, Option<u32>),
    /// An inline string.
    Str(&'a [u8]),
    /// An offset into `.debug_str`, with its relocation's section.
    Strp(u64, Option<u32>),
    /// An offset into `.debug_line_str`.
    LineStrp(u64, Option<u32>),
    /// An index into `.debug_str_offsets`.
    Strx(u64),
    /// An index into `.debug_addr`.
    Addrx(u64),
    /// An index into the range list offsets.
    Rnglistx(u64),
    /// A unit-relative reference.
    Ref(u64),
    /// A `.debug_info`-relative reference.
    RefAddr(u64),
    /// A block or expression.
    Block(&'a [u8]),
    /// A type signature (`DW_FORM_ref_sig8`).
    Signature(u64),
    /// Anything else.
    Other,
}

impl Value<'_> {
    /// The value as an unsigned constant.
    pub(crate) fn unsigned(&self) -> Option<u64> {
        match *self {
            Value::Unsigned(v, _) => Some(v),
            Value::Signed(v) => Some(v as u64),
            _ => None,
        }
    }
}

/// Reads one attribute value of `form` at the reader's position in
/// `section` (a `.debug_info`).
pub(crate) fn read_value<'a, F: crate::elf::read::ElfFormat>(
    obj: &DebugObject<'_, '_, F>,
    section: &Section<'_, F>,
    r: &mut Reader<'a>,
    form: u64,
    implicit: i64,
    unit: &UnitHeader,
) -> Parsed<Value<'a>> {
    let mut form = form;
    for _ in 0..4 {
        if form != DW_FORM_INDIRECT {
            break;
        }
        form = r.uleb()?;
    }
    let relocated = |r: &mut Reader<'a>, size: usize| -> Parsed<(u64, Option<u32>)> {
        let pos = r.pos();
        let raw = r.uint(size)?;
        Ok(obj.relocate(section, pos, raw))
    };
    Ok(match form {
        DW_FORM_ADDR => {
            let (v, t) = relocated(r, unit.address_size)?;
            Value::Addr(v, t)
        }
        DW_FORM_DATA1 | DW_FORM_FLAG => Value::Unsigned(u64::from(r.u8()?), None),
        DW_FORM_REF1 => Value::Ref(u64::from(r.u8()?)),
        DW_FORM_STRX1 => Value::Strx(u64::from(r.u8()?)),
        DW_FORM_ADDRX1 => Value::Addrx(u64::from(r.u8()?)),
        DW_FORM_DATA2 => Value::Unsigned(u64::from(r.u16()?), None),
        DW_FORM_REF2 => Value::Ref(u64::from(r.u16()?)),
        DW_FORM_STRX2 => Value::Strx(u64::from(r.u16()?)),
        DW_FORM_ADDRX2 => Value::Addrx(u64::from(r.u16()?)),
        DW_FORM_STRX3 => Value::Strx(r.uint(3)?),
        DW_FORM_ADDRX3 => Value::Addrx(r.uint(3)?),
        DW_FORM_DATA4 => {
            let (v, t) = relocated(r, 4)?;
            Value::Unsigned(v, t)
        }
        DW_FORM_REF4 => Value::Ref(r.uint(4)?),
        DW_FORM_REF_SUP4 => {
            r.skip(4)?;
            Value::Other
        }
        DW_FORM_STRX4 => Value::Strx(r.uint(4)?),
        DW_FORM_ADDRX4 => Value::Addrx(r.uint(4)?),
        DW_FORM_DATA8 => {
            let (v, t) = relocated(r, 8)?;
            Value::Unsigned(v, t)
        }
        DW_FORM_REF8 => Value::Ref(r.uint(8)?),
        DW_FORM_REF_SIG8 => Value::Signature(r.uint(8)?),
        DW_FORM_REF_SUP8 => {
            r.skip(8)?;
            Value::Other
        }
        DW_FORM_DATA16 => {
            r.skip(16)?;
            Value::Other
        }
        DW_FORM_STRING => Value::Str(r.cstr()?),
        DW_FORM_BLOCK | DW_FORM_EXPRLOC => {
            let len = usize::try_from(r.uleb()?).map_err(|_| r.error("block length"))?;
            Value::Block(r.bytes(len)?)
        }
        DW_FORM_BLOCK1 => {
            let len = usize::from(r.u8()?);
            Value::Block(r.bytes(len)?)
        }
        DW_FORM_BLOCK2 => {
            let len = usize::from(r.u16()?);
            Value::Block(r.bytes(len)?)
        }
        DW_FORM_BLOCK4 => {
            let len = usize::try_from(r.u32()?).map_err(|_| r.error("block length"))?;
            Value::Block(r.bytes(len)?)
        }
        DW_FORM_SDATA => Value::Signed(r.sleb()?),
        DW_FORM_UDATA | DW_FORM_LOCLISTX => Value::Unsigned(r.uleb()?, None),
        DW_FORM_REF_UDATA => Value::Ref(r.uleb()?),
        DW_FORM_ADDRX | DW_FORM_GNU_ADDR_INDEX => Value::Addrx(r.uleb()?),
        DW_FORM_RNGLISTX => Value::Rnglistx(r.uleb()?),
        DW_FORM_STRX | DW_FORM_GNU_STR_INDEX => Value::Strx(r.uleb()?),
        DW_FORM_STRP => {
            let (v, t) = relocated(r, unit.offset_size)?;
            Value::Strp(v, t)
        }
        DW_FORM_LINE_STRP => {
            let (v, t) = relocated(r, unit.offset_size)?;
            Value::LineStrp(v, t)
        }
        DW_FORM_SEC_OFFSET => {
            let (v, t) = relocated(r, unit.offset_size)?;
            Value::Unsigned(v, t)
        }
        DW_FORM_REF_ADDR => {
            let size = if unit.version <= 2 {
                unit.address_size
            } else {
                unit.offset_size
            };
            let (v, _) = relocated(r, size)?;
            Value::RefAddr(v)
        }
        DW_FORM_STRP_SUP | DW_FORM_GNU_REF_ALT | DW_FORM_GNU_STRP_ALT => {
            r.skip(unit.offset_size as u64)?;
            Value::Other
        }
        DW_FORM_FLAG_PRESENT => Value::Unsigned(1, None),
        DW_FORM_IMPLICIT_CONST => Value::Signed(implicit),
        _ => return Err(r.error("attribute form (unknown)")),
    })
}

/// The size of a value of `form` in `unit`, when it does not depend on the
/// value.
pub(crate) fn fixed_size(form: u64, unit: &UnitHeader) -> Option<u64> {
    Some(match form {
        DW_FORM_FLAG_PRESENT | DW_FORM_IMPLICIT_CONST => 0,
        DW_FORM_DATA1 | DW_FORM_REF1 | DW_FORM_FLAG | DW_FORM_STRX1 | DW_FORM_ADDRX1 => 1,
        DW_FORM_DATA2 | DW_FORM_REF2 | DW_FORM_STRX2 | DW_FORM_ADDRX2 => 2,
        DW_FORM_STRX3 | DW_FORM_ADDRX3 => 3,
        DW_FORM_DATA4 | DW_FORM_REF4 | DW_FORM_REF_SUP4 | DW_FORM_STRX4 | DW_FORM_ADDRX4 => 4,
        DW_FORM_DATA8 | DW_FORM_REF8 | DW_FORM_REF_SIG8 | DW_FORM_REF_SUP8 => 8,
        DW_FORM_DATA16 => 16,
        DW_FORM_ADDR => unit.address_size as u64,
        DW_FORM_STRP | DW_FORM_LINE_STRP | DW_FORM_SEC_OFFSET | DW_FORM_STRP_SUP
        | DW_FORM_GNU_REF_ALT | DW_FORM_GNU_STRP_ALT => unit.offset_size as u64,
        DW_FORM_REF_ADDR => {
            if unit.version <= 2 {
                unit.address_size as u64
            } else {
                unit.offset_size as u64
            }
        }
        _ => return None,
    })
}

/// Skips one attribute value (faster than reading it).
pub(crate) fn skip_value(r: &mut Reader<'_>, form: u64, unit: &UnitHeader) -> Parsed<()> {
    let n: u64 = match form {
        DW_FORM_FLAG_PRESENT | DW_FORM_IMPLICIT_CONST => 0,
        DW_FORM_DATA1 | DW_FORM_REF1 | DW_FORM_FLAG | DW_FORM_STRX1 | DW_FORM_ADDRX1 => 1,
        DW_FORM_DATA2 | DW_FORM_REF2 | DW_FORM_STRX2 | DW_FORM_ADDRX2 => 2,
        DW_FORM_STRX3 | DW_FORM_ADDRX3 => 3,
        DW_FORM_DATA4 | DW_FORM_REF4 | DW_FORM_REF_SUP4 | DW_FORM_STRX4 | DW_FORM_ADDRX4 => 4,
        DW_FORM_DATA8 | DW_FORM_REF8 | DW_FORM_REF_SIG8 | DW_FORM_REF_SUP8 => 8,
        DW_FORM_DATA16 => 16,
        DW_FORM_ADDR => unit.address_size as u64,
        DW_FORM_STRP | DW_FORM_LINE_STRP | DW_FORM_SEC_OFFSET | DW_FORM_STRP_SUP
        | DW_FORM_GNU_REF_ALT | DW_FORM_GNU_STRP_ALT => unit.offset_size as u64,
        DW_FORM_REF_ADDR => {
            if unit.version <= 2 {
                unit.address_size as u64
            } else {
                unit.offset_size as u64
            }
        }
        DW_FORM_UDATA
        | DW_FORM_SDATA
        | DW_FORM_REF_UDATA
        | DW_FORM_ADDRX
        | DW_FORM_GNU_ADDR_INDEX
        | DW_FORM_LOCLISTX
        | DW_FORM_RNGLISTX
        | DW_FORM_STRX
        | DW_FORM_GNU_STR_INDEX => {
            r.uleb()?;
            0
        }
        DW_FORM_STRING => {
            r.cstr()?;
            0
        }
        DW_FORM_BLOCK | DW_FORM_EXPRLOC => r.uleb()?,
        DW_FORM_BLOCK1 => u64::from(r.u8()?),
        DW_FORM_BLOCK2 => u64::from(r.u16()?),
        DW_FORM_BLOCK4 => u64::from(r.u32()?),
        DW_FORM_INDIRECT => {
            let form = r.uleb()?;
            if form == DW_FORM_INDIRECT {
                return Err(r.error("attribute form (indirect chain)"));
            }
            return skip_value(r, form, unit);
        }
        _ => return Err(r.error("attribute form (unknown)")),
    };
    r.skip(n)
}

/// The bases a unit's DIE sets for indexed forms.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Bases {
    /// `DW_AT_str_offsets_base` (offset, relocation target section).
    pub str_offsets: Option<(u64, Option<u32>)>,
    /// `DW_AT_addr_base`.
    pub addr: Option<(u64, Option<u32>)>,
    /// `DW_AT_rnglists_base`.
    pub rnglists: Option<(u64, Option<u32>)>,
}

/// Resolves a string attribute value to its bytes.
pub(crate) fn string<'a, F: crate::elf::read::ElfFormat>(
    obj: &DebugObject<'_, 'a, F>,
    value: Value<'a>,
    unit: &UnitHeader,
    bases: &Bases,
) -> Option<&'a [u8]> {
    match value {
        Value::Str(s) => Some(s),
        Value::Strp(offset, target) => {
            let data = match target {
                Some(t) => obj.data_of(t)?,
                None => obj.str.as_ref()?.data,
            };
            string_at(data, offset)
        }
        Value::LineStrp(offset, target) => {
            let data = match target {
                Some(t) => obj.data_of(t)?,
                None => obj.line_str.as_ref()?.data,
            };
            string_at(data, offset)
        }
        Value::Strx(index) => {
            let (offset, target) = str_offset(obj, index, unit, bases)?;
            let data = match target {
                Some(t) => obj.data_of(t)?,
                None => obj.str.as_ref()?.data,
            };
            string_at(data, offset)
        }
        _ => None,
    }
}

/// The `.debug_str` offset (and relocation target) of string index `index`.
pub(crate) fn str_offset<F: crate::elf::read::ElfFormat>(
    obj: &DebugObject<'_, '_, F>,
    index: u64,
    unit: &UnitHeader,
    bases: &Bases,
) -> Option<(u64, Option<u32>)> {
    let section = obj.str_offsets.as_ref()?;
    // Without DW_AT_str_offsets_base, the table starts after its header.
    let base = bases
        .str_offsets
        .map_or(if unit.offset_size == 8 { 16 } else { 8 }, |(b, _)| b);
    let entry = index
        .checked_mul(unit.offset_size as u64)
        .and_then(|o| o.checked_add(base))?;
    let pos = usize::try_from(entry).ok()?;
    let mut r = Reader::at(section.data, pos);
    let raw = r.uint(unit.offset_size).ok()?;
    Some(obj.relocate(section, pos, raw))
}

/// Entry `index` of `.debug_addr`: (address, section).
fn addrx<F: crate::elf::read::ElfFormat>(
    obj: &DebugObject<'_, '_, F>,
    index: u64,
    unit: &UnitHeader,
    bases: &Bases,
) -> Option<(u64, Option<u32>)> {
    let section = obj.addr.as_ref()?;
    let base = bases.addr.map_or(8, |(b, _)| b);
    let entry = index
        .checked_mul(unit.address_size as u64)
        .and_then(|o| o.checked_add(base))?;
    let pos = usize::try_from(entry).ok()?;
    let mut r = Reader::at(section.data, pos);
    let raw = r.uint(unit.address_size).ok()?;
    Some(obj.relocate(section, pos, raw))
}

/// An address range of a unit: (section index, low, high), low and high
/// relative to the section.
pub(crate) type AddressRange = (u32, u64, u64);

/// The unit DIE's attributes that ranges need.
#[derive(Default)]
struct UnitDie<'a> {
    low_pc: Option<Value<'a>>,
    high_pc: Option<Value<'a>>,
    entry_pc: Option<Value<'a>>,
    ranges: Option<Value<'a>>,
    bases: Bases,
}

/// Reads the attributes of the DIE at the reader's position whose
/// abbreviation is `abbrev`, calling `each` with every attribute and value.
pub(crate) fn read_attrs<'a, F: crate::elf::read::ElfFormat>(
    obj: &DebugObject<'_, '_, F>,
    section: &Section<'_, F>,
    r: &mut Reader<'a>,
    abbrev: &Abbrev,
    unit: &UnitHeader,
    mut each: impl FnMut(u64, Value<'a>),
) -> Parsed<()> {
    for &(at, form, implicit) in &abbrev.attrs {
        let value = read_value(obj, section, r, form, implicit, unit)?;
        each(at, value);
    }
    Ok(())
}

/// The address ranges, bases and language of a unit DIE, and where its
/// children start.
pub(crate) struct UnitInfo {
    /// The address ranges of the unit.
    pub ranges: Vec<AddressRange>,
    /// Bases for indexed forms.
    pub bases: Bases,
    /// `DW_AT_language`.
    pub language: Option<u64>,
    /// Position after the unit DIE's attributes.
    pub children_at: usize,
    /// Whether the unit DIE has children.
    pub children: bool,
}

/// Reads the unit DIE of `unit`: its address ranges (lld's
/// `collectAddressRanges`) and what the name scanner needs.
pub(crate) fn unit_info<'a, F: crate::elf::read::ElfFormat>(
    obj: &DebugObject<'_, 'a, F>,
    section: &Section<'a, F>,
    unit: &UnitHeader,
    abbrevs: &AbbrevTable,
) -> Parsed<Option<UnitInfo>> {
    let mut r = Reader::at(
        section.data.get(..unit.end).unwrap_or_default(),
        unit.first_die,
    );
    let code = r.uleb()?;
    if code == 0 {
        return Ok(None);
    }
    let abbrev = abbrevs
        .get(code)
        .ok_or_else(|| r.error("abbreviation code (not found)"))?;
    let mut die = UnitDie::default();
    let mut language = None;
    read_attrs(obj, section, &mut r, abbrev, unit, |at, value| match at {
        DW_AT_LOW_PC => die.low_pc = Some(value),
        DW_AT_HIGH_PC => die.high_pc = Some(value),
        DW_AT_ENTRY_PC => die.entry_pc = Some(value),
        DW_AT_RANGES => die.ranges = Some(value),
        DW_AT_LANGUAGE => language = value.unsigned(),
        DW_AT_STR_OFFSETS_BASE => {
            if let Value::Unsigned(v, t) = value {
                die.bases.str_offsets = Some((v, t));
            }
        }
        DW_AT_ADDR_BASE | DW_AT_GNU_ADDR_BASE => {
            if let Value::Unsigned(v, t) = value {
                die.bases.addr = Some((v, t));
            }
        }
        DW_AT_RNGLISTS_BASE => {
            if let Value::Unsigned(v, t) = value {
                die.bases.rnglists = Some((v, t));
            }
        }
        _ => {}
    })?;
    let ranges = address_ranges(obj, unit, &die);
    Ok(Some(UnitInfo {
        ranges,
        bases: die.bases,
        language,
        children_at: r.pos(),
        children: abbrev.children,
    }))
}

/// An address attribute as (address, section).
fn address<F: crate::elf::read::ElfFormat>(
    obj: &DebugObject<'_, '_, F>,
    value: Value<'_>,
    unit: &UnitHeader,
    bases: &Bases,
) -> Option<(u64, Option<u32>)> {
    match value {
        Value::Addr(v, t) => Some((v, t)),
        Value::Addrx(index) => addrx(obj, index, unit, bases),
        _ => None,
    }
}

/// The unit's address ranges, as LLVM's `DWARFDie::getAddressRanges`
/// computes them for the unit DIE. Ranges whose section is unknown are
/// dropped, as lld drops them.
fn address_ranges<F: crate::elf::read::ElfFormat>(
    obj: &DebugObject<'_, '_, F>,
    unit: &UnitHeader,
    die: &UnitDie<'_>,
) -> Vec<AddressRange> {
    let bases = &die.bases;
    let low = die.low_pc.and_then(|v| address(obj, v, unit, bases));
    if let (Some((low, section)), Some(high)) = (low, die.high_pc) {
        // An address form is absolute; a constant is an offset from
        // low_pc (LLVM's `getHighPC`).
        let high = match high {
            Value::Addr(..) | Value::Addrx(_) => address(obj, high, unit, bases).map(|(v, _)| v),
            other => other.unsigned().map(|offset| low.wrapping_add(offset)),
        };
        return match (section, high) {
            (Some(section), Some(high)) => vec![(section, low, high)],
            _ => Vec::new(),
        };
    }
    let Some(ranges) = die.ranges else {
        return Vec::new();
    };
    let base = die
        .low_pc
        .or(die.entry_pc)
        .and_then(|v| address(obj, v, unit, bases));
    let mut out = Vec::new();
    if unit.version <= 4 {
        if let Value::Unsigned(offset, _) = ranges {
            debug_ranges(obj, unit, offset, base, &mut out);
        }
        return out;
    }
    let offset = match ranges {
        Value::Unsigned(offset, _) => Some(offset),
        Value::Rnglistx(index) => rnglist_offset(obj, index, unit, bases),
        _ => None,
    };
    if let Some(offset) = offset {
        rnglist(obj, unit, bases, offset, base, &mut out);
    }
    out
}

/// A `.debug_ranges` list (DWARF 4 and older), LLVM's
/// `DWARFDebugRangeList::getAbsoluteRanges`.
fn debug_ranges<F: crate::elf::read::ElfFormat>(
    obj: &DebugObject<'_, '_, F>,
    unit: &UnitHeader,
    offset: u64,
    mut base: Option<(u64, Option<u32>)>,
    out: &mut Vec<AddressRange>,
) {
    let Some(section) = obj.ranges.as_ref() else {
        return;
    };
    let size = unit.address_size;
    let max = max_address(size);
    let tombstone = max.wrapping_sub(1);
    let Ok(start) = usize::try_from(offset) else {
        return;
    };
    let mut r = Reader::at(section.data, start);
    loop {
        let pos = r.pos();
        let Ok(raw) = r.uint(size) else { return };
        let (low, low_section) = obj.relocate(section, pos, raw);
        let pos = r.pos();
        let Ok(raw) = r.uint(size) else { return };
        let (high, _) = obj.relocate(section, pos, raw);
        if low == 0 && high == 0 {
            return;
        }
        if low == max {
            base = Some((high, low_section));
            continue;
        }
        if low == tombstone {
            continue;
        }
        let (mut low, mut high, mut section_index) = (low, high, low_section);
        if let Some((base_address, base_section)) = base {
            if base_address == tombstone {
                continue;
            }
            low = low.wrapping_add(base_address);
            high = high.wrapping_add(base_address);
            if section_index.is_none() {
                section_index = base_section;
            }
        }
        if let Some(s) = section_index {
            out.push((s, low, high));
        }
    }
}

/// The largest address of `size` bytes.
fn max_address(size: usize) -> u64 {
    match size {
        1 => 0xff,
        2 => 0xffff,
        4 => 0xffff_ffff,
        _ => u64::MAX,
    }
}

/// The offset of range list `index` (`DW_FORM_rnglistx`).
fn rnglist_offset<F: crate::elf::read::ElfFormat>(
    obj: &DebugObject<'_, '_, F>,
    index: u64,
    unit: &UnitHeader,
    bases: &Bases,
) -> Option<u64> {
    let section = obj.rnglists.as_ref()?;
    let base = bases.rnglists.map_or(0, |(b, _)| b);
    let entry = index
        .checked_mul(unit.offset_size as u64)
        .and_then(|o| o.checked_add(base))?;
    let mut r = Reader::at(section.data, usize::try_from(entry).ok()?);
    let relative = r.uint(unit.offset_size).ok()?;
    relative.checked_add(base)
}

/// A `.debug_rnglists` list, LLVM's `DWARFDebugRnglist::getAbsoluteRanges`.
fn rnglist<F: crate::elf::read::ElfFormat>(
    obj: &DebugObject<'_, '_, F>,
    unit: &UnitHeader,
    bases: &Bases,
    offset: u64,
    mut base: Option<(u64, Option<u32>)>,
    out: &mut Vec<AddressRange>,
) {
    let Some(section) = obj.rnglists.as_ref() else {
        return;
    };
    let size = unit.address_size;
    let tombstone = max_address(size);
    let Ok(start) = usize::try_from(offset) else {
        return;
    };
    let mut r = Reader::at(section.data, start);
    let address = |r: &mut Reader<'_>| -> Option<(u64, Option<u32>)> {
        let pos = r.pos();
        let raw = r.uint(size).ok()?;
        Some(obj.relocate(section, pos, raw))
    };
    // At most one entry per byte: bounds the loop on malformed input.
    for _ in 0..section.data.len() {
        let Ok(kind) = r.u8() else { return };
        let (low, high, entry_section) = match kind {
            DW_RLE_END_OF_LIST => return,
            DW_RLE_BASE_ADDRESSX => {
                let Ok(index) = r.uleb() else { return };
                base = Some(addrx(obj, index, unit, bases).unwrap_or((index, None)));
                continue;
            }
            DW_RLE_BASE_ADDRESS => {
                let Some(value) = address(&mut r) else { return };
                base = Some(value);
                continue;
            }
            DW_RLE_OFFSET_PAIR => {
                let (Ok(a), Ok(b)) = (r.uleb(), r.uleb()) else {
                    return;
                };
                let section_index = base.and_then(|(_, s)| s);
                match base {
                    Some((base_address, _)) => {
                        if base_address == tombstone {
                            continue;
                        }
                        (
                            a.wrapping_add(base_address),
                            b.wrapping_add(base_address),
                            section_index,
                        )
                    }
                    None => (a, b, None),
                }
            }
            DW_RLE_START_END => {
                let (Some((a, s)), Some((b, _))) = (address(&mut r), address(&mut r)) else {
                    return;
                };
                (a, b, s.or(base.and_then(|(_, s)| s)))
            }
            DW_RLE_START_LENGTH => {
                let Some((a, s)) = address(&mut r) else {
                    return;
                };
                let Ok(length) = r.uleb() else { return };
                (a, a.wrapping_add(length), s.or(base.and_then(|(_, s)| s)))
            }
            DW_RLE_STARTX_LENGTH => {
                let Ok(index) = r.uleb() else { return };
                let Ok(length) = r.uleb() else { return };
                let (a, s) = addrx(obj, index, unit, bases).unwrap_or((0, None));
                (a, a.wrapping_add(length), s)
            }
            DW_RLE_STARTX_ENDX => {
                let (Ok(i), Ok(j)) = (r.uleb(), r.uleb()) else {
                    return;
                };
                let (a, s) = addrx(obj, i, unit, bases).unwrap_or((0, None));
                let (b, _) = addrx(obj, j, unit, bases).unwrap_or((0, None));
                (a, b, s)
            }
            _ => return,
        };
        if low == tombstone {
            continue;
        }
        if let Some(s) = entry_section {
            out.push((s, low, high));
        }
    }
}
