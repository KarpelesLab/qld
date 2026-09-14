//! Unit headers, abbreviations and attribute forms (`.debug_info`,
//! `.debug_abbrev`), read just far enough to get a compilation unit's name,
//! compilation directory and line table offset.

use super::context::{Context, read_relocated};
use super::reader::{DwarfResult, Reader};
use crate::elf::read::ElfFormat;

// Attribute forms (DWARF 5 section 7.5.6, plus GNU extensions).
pub(super) const DW_FORM_ADDR: u64 = 0x01;
pub(super) const DW_FORM_BLOCK2: u64 = 0x03;
pub(super) const DW_FORM_BLOCK4: u64 = 0x04;
pub(super) const DW_FORM_DATA2: u64 = 0x05;
pub(super) const DW_FORM_DATA4: u64 = 0x06;
pub(super) const DW_FORM_DATA8: u64 = 0x07;
pub(super) const DW_FORM_STRING: u64 = 0x08;
pub(super) const DW_FORM_BLOCK: u64 = 0x09;
pub(super) const DW_FORM_BLOCK1: u64 = 0x0a;
pub(super) const DW_FORM_DATA1: u64 = 0x0b;
pub(super) const DW_FORM_FLAG: u64 = 0x0c;
pub(super) const DW_FORM_SDATA: u64 = 0x0d;
pub(super) const DW_FORM_STRP: u64 = 0x0e;
pub(super) const DW_FORM_UDATA: u64 = 0x0f;
pub(super) const DW_FORM_REF_ADDR: u64 = 0x10;
pub(super) const DW_FORM_REF1: u64 = 0x11;
pub(super) const DW_FORM_REF2: u64 = 0x12;
pub(super) const DW_FORM_REF4: u64 = 0x13;
pub(super) const DW_FORM_REF8: u64 = 0x14;
pub(super) const DW_FORM_REF_UDATA: u64 = 0x15;
pub(super) const DW_FORM_INDIRECT: u64 = 0x16;
pub(super) const DW_FORM_SEC_OFFSET: u64 = 0x17;
pub(super) const DW_FORM_EXPRLOC: u64 = 0x18;
pub(super) const DW_FORM_FLAG_PRESENT: u64 = 0x19;
pub(super) const DW_FORM_STRX: u64 = 0x1a;
pub(super) const DW_FORM_ADDRX: u64 = 0x1b;
pub(super) const DW_FORM_REF_SUP4: u64 = 0x1c;
pub(super) const DW_FORM_STRP_SUP: u64 = 0x1d;
pub(super) const DW_FORM_DATA16: u64 = 0x1e;
pub(super) const DW_FORM_LINE_STRP: u64 = 0x1f;
pub(super) const DW_FORM_REF_SIG8: u64 = 0x20;
pub(super) const DW_FORM_IMPLICIT_CONST: u64 = 0x21;
pub(super) const DW_FORM_LOCLISTX: u64 = 0x22;
pub(super) const DW_FORM_RNGLISTX: u64 = 0x23;
pub(super) const DW_FORM_REF_SUP8: u64 = 0x24;
pub(super) const DW_FORM_STRX1: u64 = 0x25;
pub(super) const DW_FORM_STRX2: u64 = 0x26;
pub(super) const DW_FORM_STRX3: u64 = 0x27;
pub(super) const DW_FORM_STRX4: u64 = 0x28;
pub(super) const DW_FORM_ADDRX1: u64 = 0x29;
pub(super) const DW_FORM_ADDRX2: u64 = 0x2a;
pub(super) const DW_FORM_ADDRX3: u64 = 0x2b;
pub(super) const DW_FORM_ADDRX4: u64 = 0x2c;
pub(super) const DW_FORM_GNU_ADDR_INDEX: u64 = 0x1f01;
pub(super) const DW_FORM_GNU_STR_INDEX: u64 = 0x1f02;
pub(super) const DW_FORM_GNU_REF_ALT: u64 = 0x1f20;
pub(super) const DW_FORM_GNU_STRP_ALT: u64 = 0x1f21;

// Attributes.
const DW_AT_NAME: u64 = 0x03;
const DW_AT_STMT_LIST: u64 = 0x10;
const DW_AT_COMP_DIR: u64 = 0x1b;
const DW_AT_STR_OFFSETS_BASE: u64 = 0x72;
const DW_AT_DWO_NAME: u64 = 0x76;
const DW_AT_GNU_DWO_NAME: u64 = 0x2130;

// Unit types (DWARF 5).
const DW_UT_COMPILE: u8 = 0x01;
const DW_UT_TYPE: u8 = 0x02;
const DW_UT_PARTIAL: u8 = 0x03;
const DW_UT_SKELETON: u8 = 0x04;
const DW_UT_SPLIT_COMPILE: u8 = 0x05;
const DW_UT_SPLIT_TYPE: u8 = 0x06;

/// The encoding parameters attribute forms depend on.
#[derive(Clone, Copy, Debug)]
pub(super) struct Encoding {
    pub(super) version: u16,
    pub(super) offset_size: usize,
    pub(super) address_size: usize,
}

/// A decoded attribute value, as far as line lookup cares.
#[derive(Clone, Copy, Debug)]
pub(super) enum Value<'a> {
    /// A constant, address, reference or section offset, with the section
    /// its relocation points into.
    Unsigned(u64, Option<u32>),
    /// An inline string.
    Str(&'a [u8]),
    /// An offset into `.debug_str`.
    Strp(u64, Option<u32>),
    /// An offset into `.debug_line_str`.
    LineStrp(u64, Option<u32>),
    /// An index into `.debug_str_offsets`.
    Strx(u64),
    /// Anything else (blocks, signed data, supplementary references).
    Other,
}

/// Reads one attribute value of `form` from section `section`.
pub(super) fn read_value<'a, F: ElfFormat>(
    ctx: &Context<'_, F>,
    section: u32,
    r: &mut Reader<'a>,
    form: u64,
    implicit_const: i64,
    enc: Encoding,
) -> DwarfResult<Value<'a>> {
    let mut form = form;
    // DW_FORM_indirect names the real form inline; bound the chain.
    for _ in 0..4 {
        if form != DW_FORM_INDIRECT {
            break;
        }
        form = r.uleb()?;
    }
    let offset = |r: &mut Reader<'a>| read_relocated(ctx, section, r, enc.offset_size);
    Ok(match form {
        DW_FORM_ADDR => {
            let (v, t) = read_relocated(ctx, section, r, enc.address_size)?;
            Value::Unsigned(v, t)
        }
        DW_FORM_DATA1 | DW_FORM_REF1 | DW_FORM_FLAG | DW_FORM_STRX1 | DW_FORM_ADDRX1 => {
            let v = u64::from(r.u8()?);
            if form == DW_FORM_STRX1 {
                Value::Strx(v)
            } else {
                Value::Unsigned(v, None)
            }
        }
        DW_FORM_DATA2 | DW_FORM_REF2 | DW_FORM_STRX2 | DW_FORM_ADDRX2 => {
            let v = u64::from(r.u16()?);
            if form == DW_FORM_STRX2 {
                Value::Strx(v)
            } else {
                Value::Unsigned(v, None)
            }
        }
        DW_FORM_STRX3 | DW_FORM_ADDRX3 => {
            let v = r.uint(3)?;
            if form == DW_FORM_STRX3 {
                Value::Strx(v)
            } else {
                Value::Unsigned(v, None)
            }
        }
        DW_FORM_DATA4 | DW_FORM_REF4 | DW_FORM_REF_SUP4 | DW_FORM_STRX4 | DW_FORM_ADDRX4 => {
            let (v, t) = read_relocated(ctx, section, r, 4)?;
            if form == DW_FORM_STRX4 {
                Value::Strx(v)
            } else {
                Value::Unsigned(v, t)
            }
        }
        DW_FORM_DATA8 | DW_FORM_REF8 | DW_FORM_REF_SIG8 | DW_FORM_REF_SUP8 => {
            let (v, t) = read_relocated(ctx, section, r, 8)?;
            Value::Unsigned(v, t)
        }
        DW_FORM_DATA16 => {
            r.skip(16)?;
            Value::Other
        }
        DW_FORM_STRING => Value::Str(r.cstr()?),
        DW_FORM_BLOCK | DW_FORM_EXPRLOC => {
            let len = r.uleb()?;
            r.skip(len)?;
            Value::Other
        }
        DW_FORM_BLOCK1 => {
            let len = r.u8()?;
            r.skip(u64::from(len))?;
            Value::Other
        }
        DW_FORM_BLOCK2 => {
            let len = r.u16()?;
            r.skip(u64::from(len))?;
            Value::Other
        }
        DW_FORM_BLOCK4 => {
            let len = r.u32()?;
            r.skip(u64::from(len))?;
            Value::Other
        }
        DW_FORM_SDATA => {
            r.sleb()?;
            Value::Other
        }
        DW_FORM_UDATA
        | DW_FORM_REF_UDATA
        | DW_FORM_ADDRX
        | DW_FORM_LOCLISTX
        | DW_FORM_RNGLISTX
        | DW_FORM_GNU_ADDR_INDEX => Value::Unsigned(r.uleb()?, None),
        DW_FORM_STRX | DW_FORM_GNU_STR_INDEX => Value::Strx(r.uleb()?),
        DW_FORM_STRP => {
            let (v, t) = offset(r)?;
            Value::Strp(v, t)
        }
        DW_FORM_LINE_STRP => {
            let (v, t) = offset(r)?;
            Value::LineStrp(v, t)
        }
        DW_FORM_SEC_OFFSET => {
            let (v, t) = offset(r)?;
            Value::Unsigned(v, t)
        }
        DW_FORM_REF_ADDR => {
            // DWARF 2 made this address-sized.
            let size = if enc.version <= 2 {
                enc.address_size
            } else {
                enc.offset_size
            };
            let (v, t) = read_relocated(ctx, section, r, size)?;
            Value::Unsigned(v, t)
        }
        DW_FORM_STRP_SUP | DW_FORM_GNU_REF_ALT | DW_FORM_GNU_STRP_ALT => {
            r.skip(enc.offset_size as u64)?;
            Value::Other
        }
        DW_FORM_FLAG_PRESENT => Value::Unsigned(1, None),
        DW_FORM_IMPLICIT_CONST => Value::Unsigned(implicit_const as u64, None),
        _ => return Err(r.error("DWARF attribute form (unknown)")),
    })
}

/// What line lookup needs from a compilation unit.
#[derive(Clone, Debug, Default)]
pub(super) struct Unit {
    /// `DW_AT_name`: the primary source file, used when a line table row
    /// names no valid file.
    pub(super) name: Option<Vec<u8>>,
    /// `DW_AT_comp_dir`.
    pub(super) comp_dir: Option<Vec<u8>>,
    /// `DW_AT_stmt_list`: the line table's section (from its relocation)
    /// and offset.
    pub(super) stmt_list: Option<(Option<u32>, u64)>,
    /// A split-DWARF skeleton unit.
    pub(super) skeleton: bool,
}

/// Parses every unit of `.debug_info` section `section`. A malformed unit
/// ends the scan; the units before it are returned with the error.
pub(super) fn parse_units<F: ElfFormat>(
    ctx: &Context<'_, F>,
    section: u32,
) -> (Vec<Unit>, Option<super::reader::DwarfError>) {
    let mut units = Vec::new();
    let Some(loaded) = ctx.section(section) else {
        return (units, None);
    };
    let mut r = Reader::new(&loaded.data, ctx.big_endian);
    while !r.is_empty() {
        match parse_unit(ctx, section, &mut r) {
            Ok(Some(unit)) => units.push(unit),
            Ok(None) => {}
            Err(error) => return (units, Some(error)),
        }
    }
    (units, None)
}

/// Parses the unit at the reader's position and moves past it. Returns
/// `None` for type units.
fn parse_unit<F: ElfFormat>(
    ctx: &Context<'_, F>,
    section: u32,
    r: &mut Reader<'_>,
) -> DwarfResult<Option<Unit>> {
    let (length, offset_size) = r.initial_length()?;
    let mut u = r.sub(length)?;
    let version = u.u16()?;
    if !(2..=5).contains(&version) {
        return Err(u.error("DWARF unit version (unsupported)"));
    }
    let (unit_type, address_size, abbrev) = if version >= 5 {
        let unit_type = u.u8()?;
        let address_size = u.u8()?;
        let abbrev = read_relocated(ctx, section, &mut u, offset_size)?;
        match unit_type {
            DW_UT_COMPILE | DW_UT_PARTIAL => {}
            DW_UT_SKELETON | DW_UT_SPLIT_COMPILE => {
                u.skip(8)?; // dwo_id
            }
            DW_UT_TYPE | DW_UT_SPLIT_TYPE => return Ok(None),
            _ => return Err(u.error("DWARF unit type (unknown)")),
        }
        (unit_type, address_size, abbrev)
    } else {
        let abbrev = read_relocated(ctx, section, &mut u, offset_size)?;
        let address_size = u.u8()?;
        (DW_UT_COMPILE, address_size, abbrev)
    };
    let enc = Encoding {
        version,
        offset_size,
        address_size: usize::from(address_size),
    };
    if !matches!(enc.address_size, 1 | 2 | 4 | 8) {
        return Err(u.error("DWARF address size"));
    }

    let code = u.uleb()?;
    if code == 0 {
        return Ok(Some(Unit::default()));
    }
    let abbrev_section = ctx
        .abbrev_section(abbrev.1)
        .ok_or_else(|| u.error("DWARF abbreviations (no .debug_abbrev)"))?;
    let abbrev_data = ctx
        .section(abbrev_section)
        .map(|s| &*s.data)
        .unwrap_or_default();
    let attrs = find_abbrev(abbrev_data, abbrev.0, code, ctx.big_endian)?;

    let mut unit = Unit {
        skeleton: unit_type == DW_UT_SKELETON,
        ..Unit::default()
    };
    let mut name_value = None;
    let mut comp_dir_value = None;
    let mut str_offsets_base = None;
    for (at, form, implicit) in attrs {
        let value = read_value(ctx, section, &mut u, form, implicit, enc)?;
        match at {
            DW_AT_NAME => name_value = Some(value),
            DW_AT_COMP_DIR => comp_dir_value = Some(value),
            DW_AT_STMT_LIST => {
                if let Value::Unsigned(offset, target) = value {
                    unit.stmt_list = Some((target, offset));
                }
            }
            DW_AT_STR_OFFSETS_BASE => {
                if let Value::Unsigned(offset, target) = value {
                    str_offsets_base = Some((target, offset));
                }
            }
            DW_AT_DWO_NAME | DW_AT_GNU_DWO_NAME => unit.skeleton = true,
            _ => {}
        }
    }
    unit.name = name_value
        .and_then(|v| resolve_string(ctx, v, str_offsets_base, offset_size))
        .map(<[u8]>::to_vec);
    unit.comp_dir = comp_dir_value
        .and_then(|v| resolve_string(ctx, v, str_offsets_base, offset_size))
        .map(<[u8]>::to_vec);
    Ok(Some(unit))
}

/// Looks up abbreviation `code` in the table at `offset`. Returns its
/// attribute specifications: (attribute, form, implicit constant).
fn find_abbrev(
    data: &[u8],
    offset: u64,
    code: u64,
    big_endian: bool,
) -> DwarfResult<Vec<(u64, u64, i64)>> {
    let start = usize::try_from(offset).unwrap_or(usize::MAX);
    let mut r = Reader::at(data, start, big_endian);
    if start > data.len() {
        return Err(r.error("DWARF abbreviation offset (out of range)"));
    }
    loop {
        let this = r.uleb()?;
        if this == 0 {
            return Err(r.error("DWARF abbreviation code (not found)"));
        }
        let _tag = r.uleb()?;
        let _children = r.u8()?;
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
            if this == code {
                attrs.push((at, form, implicit));
            }
        }
        if this == code {
            return Ok(attrs);
        }
    }
}

/// Resolves a string attribute value.
pub(super) fn resolve_string<'c, F: ElfFormat>(
    ctx: &'c Context<'_, F>,
    value: Value<'c>,
    str_offsets_base: Option<(Option<u32>, u64)>,
    offset_size: usize,
) -> Option<&'c [u8]> {
    match value {
        Value::Str(s) => Some(s),
        Value::Strp(offset, target) => ctx.string_at(ctx.str_section(target)?, offset),
        Value::LineStrp(offset, target) => ctx.string_at(ctx.line_str_section(target)?, offset),
        Value::Strx(index) => {
            // Without DW_AT_str_offsets_base, the table starts after its
            // header (the base split units use).
            let (target, base) =
                str_offsets_base.unwrap_or((None, if offset_size == 8 { 16 } else { 8 }));
            let section = target.or(ctx.str_offsets)?;
            let entry = index
                .checked_mul(offset_size as u64)
                .and_then(|o| o.checked_add(base))?;
            let data = &ctx.section(section)?.data;
            let mut r = Reader::at(data, usize::try_from(entry).ok()?, ctx.big_endian);
            let (offset, target) = read_relocated(ctx, section, &mut r, offset_size).ok()?;
            ctx.string_at(ctx.str_section(target)?, offset)
        }
        Value::Unsigned(..) | Value::Other => None,
    }
}
