//! The STABS debug map.
//!
//! Mach-O executables do not carry DWARF: it stays in the object files, and
//! `dsymutil` (or the debugger) finds it through a debug map of STABS
//! entries in the symbol table. For each object with a compile unit, as lld
//! writes it:
//!
//! - `N_SO` with the source file path (compilation directory joined with the
//!   unit's name);
//! - `N_OSO` with the object's absolute path (`archive(member)` for archive
//!   members, minus `-oso_prefix`), `n_sect` = the CPU subtype, `n_desc` = 1
//!   and the object's modification time as the value;
//! - for each of its symbols in the output: `N_FUN` with the address, then
//!   `N_FUN` with the size, for code; `N_GSYM` (external) or `N_STSYM`
//!   (local) for data;
//! - `N_SO` with an empty name and `n_sect` = 1 to close the file.
//!
//! Only the compile unit's name and directory are read from the DWARF.

#![deny(clippy::arithmetic_side_effects)]

use crate::macho::read::bytes::{read_sleb, read_uleb};
use crate::macho::read::consts::{N_FUN, N_GSYM, N_OSO, N_SECT, N_SO, N_STSYM};

use super::addr::Addresses;
use super::buf::{get32, get64, to_u64, to_usize};
use super::object::LinkObject;
use super::reloc::Value;
use super::symtab::Nlist;
use super::unwind::is_code;

const DW_AT_NAME: u64 = 0x03;
const DW_AT_COMP_DIR: u64 = 0x1b;
const DW_AT_STR_OFFSETS_BASE: u64 = 0x72;

/// A string attribute before the string sections are consulted.
#[derive(Clone, Copy)]
enum StringValue {
    Inline(usize, usize),
    Strp(u64),
    LineStrp(u64),
    Strx(u64),
}

fn section<'a>(object: &LinkObject<'a>, name: &[u8]) -> Option<&'a [u8]> {
    let (index, _) = object.file.find_section(b"__DWARF", name)?;
    object.file.section_data(index).ok()
}

fn cstring(data: &[u8], offset: u64) -> Option<&[u8]> {
    let rest = data.get(to_usize(offset)..)?;
    rest.get(..rest.iter().position(|&b| b == 0)?)
}

/// Skips (or reads) one attribute value of `form` at `*pos`. Returns the
/// value for integer forms and the string for string forms.
#[allow(clippy::too_many_lines)]
fn read_form(
    info: &[u8],
    pos: &mut usize,
    form: u64,
    address_size: usize,
    offset_size: usize,
    implicit: i64,
) -> Option<(u64, Option<StringValue>)> {
    let take = |pos: &mut usize, n: usize| -> Option<usize> {
        let start = *pos;
        *pos = pos.checked_add(n)?;
        (*pos <= info.len()).then_some(start)
    };
    let uint = |at: usize, n: usize| -> Option<u64> {
        let bytes = info.get(at..at.checked_add(n)?)?;
        let mut value = 0u64;
        for (i, &b) in bytes.iter().enumerate() {
            value |= u64::from(b).checked_shl(u32::try_from(i.checked_mul(8)?).ok()?)?;
        }
        Some(value)
    };
    Some(match form {
        0x01 => (uint(take(pos, address_size)?, address_size)?, None), // addr
        0x0b | 0x11 | 0x0c | 0x29 => (uint(take(pos, 1)?, 1)?, None),  // data1 ref1 flag addrx1
        0x25 => {
            let v = uint(take(pos, 1)?, 1)?;
            (v, Some(StringValue::Strx(v)))
        }
        0x05 | 0x12 | 0x2a => (uint(take(pos, 2)?, 2)?, None), // data2 ref2 addrx2
        0x26 => {
            let v = uint(take(pos, 2)?, 2)?;
            (v, Some(StringValue::Strx(v)))
        }
        0x2b => (uint(take(pos, 3)?, 3)?, None), // addrx3
        0x27 => {
            let v = uint(take(pos, 3)?, 3)?;
            (v, Some(StringValue::Strx(v)))
        }
        0x06 | 0x13 | 0x2c | 0x1c => (uint(take(pos, 4)?, 4)?, None), // data4 ref4 addrx4 ref_sup4
        0x28 => {
            let v = uint(take(pos, 4)?, 4)?;
            (v, Some(StringValue::Strx(v)))
        }
        0x07 | 0x14 | 0x20 => (uint(take(pos, 8)?, 8)?, None), // data8 ref8 ref_sig8
        0x1e => {
            take(pos, 16)?;
            (0, None)
        }
        0x0f | 0x15 | 0x1b | 0x22 | 0x23 => (read_uleb(info, pos)?, None), // udata ref_udata addrx loclistx rnglistx
        0x0d => (read_sleb(info, pos)? as u64, None),
        0x1a => {
            let v = read_uleb(info, pos)?;
            (v, Some(StringValue::Strx(v)))
        }
        0x08 => {
            let start = *pos;
            let len = info.get(start..)?.iter().position(|&b| b == 0)?;
            *pos = start.checked_add(len)?.checked_add(1)?;
            (0, Some(StringValue::Inline(start, len)))
        }
        0x0e => {
            let v = uint(take(pos, offset_size)?, offset_size)?;
            (v, Some(StringValue::Strp(v)))
        }
        0x1f => {
            let v = uint(take(pos, offset_size)?, offset_size)?;
            (v, Some(StringValue::LineStrp(v)))
        }
        0x10 | 0x17 | 0x1d => (uint(take(pos, offset_size)?, offset_size)?, None), // ref_addr sec_offset strp_sup
        0x0a => {
            let n = usize::from(*info.get(take(pos, 1)?)?);
            take(pos, n)?;
            (0, None)
        }
        0x03 => {
            let n = to_usize(uint(take(pos, 2)?, 2)?);
            take(pos, n)?;
            (0, None)
        }
        0x04 => {
            let n = to_usize(uint(take(pos, 4)?, 4)?);
            take(pos, n)?;
            (0, None)
        }
        0x09 | 0x18 => {
            let n = to_usize(read_uleb(info, pos)?);
            take(pos, n)?;
            (0, None)
        }
        0x19 => (1, None),
        0x21 => (implicit as u64, None),
        0x16 => {
            let form = read_uleb(info, pos)?;
            return read_form(info, pos, form, address_size, offset_size, implicit);
        }
        _ => return None,
    })
}

/// The source file of `object`'s first compile unit: its `DW_AT_name`,
/// joined to `DW_AT_comp_dir` unless absolute.
#[must_use]
pub fn source_file(object: &LinkObject<'_>) -> Option<Vec<u8>> {
    let info = section(object, b"__debug_info")?;
    let abbrev = section(object, b"__debug_abbrev")?;
    let mut pos = 0usize;
    let mut length = u64::from(get32(info, 0)?);
    let offset_size = if length == 0xffff_ffff {
        length = get64(info, 4)?;
        pos = 12;
        8
    } else {
        pos = pos.checked_add(4)?;
        4
    };
    let _ = length;
    let version = u16::from_le_bytes([*info.get(pos)?, *info.get(pos.checked_add(1)?)?]);
    pos = pos.checked_add(2)?;
    let read_offset = |at: usize| -> Option<u64> {
        if offset_size == 8 {
            get64(info, at)
        } else {
            get32(info, at).map(u64::from)
        }
    };
    let (abbrev_offset, address_size) = if version >= 5 {
        let _unit_type = *info.get(pos)?;
        let address_size = usize::from(*info.get(pos.checked_add(1)?)?);
        let abbrev_offset = read_offset(pos.checked_add(2)?)?;
        pos = pos.checked_add(2)?.checked_add(offset_size)?;
        (abbrev_offset, address_size)
    } else {
        let abbrev_offset = read_offset(pos)?;
        pos = pos.checked_add(offset_size)?;
        let address_size = usize::from(*info.get(pos)?);
        pos = pos.checked_add(1)?;
        (abbrev_offset, address_size)
    };
    let code = read_uleb(info, &mut pos)?;

    // Find the abbreviation.
    let mut apos = to_usize(abbrev_offset);
    let specs = loop {
        let this = read_uleb(abbrev, &mut apos)?;
        if this == 0 {
            return None;
        }
        let _tag = read_uleb(abbrev, &mut apos)?;
        apos = apos.checked_add(1)?;
        let mut specs = Vec::new();
        loop {
            let name = read_uleb(abbrev, &mut apos)?;
            let form = read_uleb(abbrev, &mut apos)?;
            if name == 0 && form == 0 {
                break;
            }
            let implicit = if form == 0x21 {
                read_sleb(abbrev, &mut apos)?
            } else {
                0
            };
            specs.push((name, form, implicit));
        }
        if this == code {
            break specs;
        }
    };

    let mut name = None;
    let mut dir = None;
    let mut str_offsets_base = None;
    for (attribute, form, implicit) in specs {
        let (value, string) = read_form(info, &mut pos, form, address_size, offset_size, implicit)?;
        match attribute {
            DW_AT_NAME => name = string,
            DW_AT_COMP_DIR => dir = string,
            DW_AT_STR_OFFSETS_BASE => str_offsets_base = Some(value),
            _ => {}
        }
    }
    let resolve = |value: StringValue| -> Option<Vec<u8>> {
        Some(match value {
            StringValue::Inline(start, len) => info.get(start..start.checked_add(len)?)?.to_vec(),
            StringValue::Strp(offset) => {
                cstring(section(object, b"__debug_str")?, offset)?.to_vec()
            }
            StringValue::LineStrp(offset) => {
                cstring(section(object, b"__debug_line_str")?, offset)?.to_vec()
            }
            StringValue::Strx(index) => {
                let offsets = section(object, b"__debug_str_offs")?;
                let base = str_offsets_base.unwrap_or(8);
                let at = base.checked_add(index.checked_mul(to_u64(offset_size))?)?;
                let offset = if offset_size == 8 {
                    get64(offsets, to_usize(at))?
                } else {
                    u64::from(get32(offsets, to_usize(at))?)
                };
                cstring(section(object, b"__debug_str")?, offset)?.to_vec()
            }
        })
    };
    let name = resolve(name?)?;
    if name.starts_with(b"/") {
        return Some(name);
    }
    let mut path = match dir {
        Some(dir) => resolve(dir)?,
        None => Vec::new(),
    };
    if !path.ends_with(b"/") {
        path.push(b'/');
    }
    path.extend_from_slice(&name);
    Some(path)
}

/// The `N_OSO` path of file `file`.
fn object_path(addresses: &Addresses<'_, '_>, file: usize) -> Vec<u8> {
    let Some(input) = addresses.link.files.get(file).and_then(|f| f.file) else {
        return Vec::new();
    };
    let path = std::path::absolute(input.path()).unwrap_or_else(|_| input.path().to_path_buf());
    let mut bytes = path.as_os_str().as_encoded_bytes().to_vec();
    if let Some(member) = input.member() {
        bytes.push(b'(');
        bytes.extend_from_slice(member.as_bytes());
        bytes.push(b')');
    }
    if let Some(prefix) = &addresses.link.config.oso_prefix {
        let prefix = prefix.as_os_str().as_encoded_bytes();
        if !prefix.is_empty()
            && let Some(rest) = bytes.strip_prefix(prefix)
        {
            bytes = rest.to_vec();
        }
    }
    bytes
}

/// Builds the debug map for the object symbols `symbols` (file and symbol
/// table index) that the symbol table contains.
#[must_use]
pub fn build(addresses: &Addresses<'_, '_>, symbols: &[(usize, u32)]) -> Vec<Nlist> {
    let link = addresses.link;
    let mut ordered: Vec<(usize, u32)> = symbols.to_vec();
    ordered.sort_by_key(|&(file, _)| file);
    let cpu_subtype = u8::try_from(link.config.arch.cpu_subtype & 0xff).unwrap_or(0);
    let mut out = Vec::new();
    let mut current: Option<usize> = None;
    let mut sources: hashbrown::HashMap<usize, Option<Vec<u8>>> = hashbrown::HashMap::new();
    let stab = |n_type: u8, name: Vec<u8>, n_sect: u8, n_desc: u16, n_value: u64| Nlist {
        name,
        n_type,
        n_sect,
        n_desc,
        n_value,
    };
    for (file, symbol) in ordered {
        let Some(object) = link.object(file) else {
            continue;
        };
        let source = sources
            .entry(file)
            .or_insert_with(|| source_file(object))
            .clone();
        let Some(source) = source else {
            continue;
        };
        let Ok(entry) = object.file.symbols().get(symbol) else {
            continue;
        };
        if entry.n_type & 0x0e != N_SECT {
            continue;
        }
        let Some(Value::Address(address)) = addresses.object_symbol(file, symbol) else {
            continue;
        };
        if current != Some(file) {
            if current.is_some() {
                out.push(stab(N_SO, Vec::new(), 1, 0, 0));
            }
            current = Some(file);
            out.push(stab(N_SO, source, 0, 0, 0));
            let mtime = link.files.get(file).map_or(0, |f| f.mtime);
            out.push(stab(
                N_OSO,
                object_path(addresses, file),
                cpu_subtype,
                1,
                mtime,
            ));
        }
        let n_sect = addresses
            .layout
            .sections
            .iter()
            .position(|s| address >= s.addr && address < s.end().max(s.addr.saturating_add(1)))
            .and_then(|i| u8::try_from(i.saturating_add(1)).ok())
            .unwrap_or(0);
        let Some(section) = object.file.section_by_ordinal(u32::from(entry.n_sect)) else {
            continue;
        };
        if is_code(section.segname, section.sectname, section.flags) {
            out.push(stab(N_FUN, entry.name.to_vec(), n_sect, 0, address));
            out.push(stab(N_FUN, Vec::new(), 0, 0, symbol_size(object, symbol)));
        } else {
            let kind = if entry.is_external() && !entry.is_private_external() {
                N_GSYM
            } else {
                N_STSYM
            };
            out.push(stab(kind, entry.name.to_vec(), n_sect, 0, address));
        }
    }
    if current.is_some() {
        out.push(stab(N_SO, Vec::new(), 1, 0, 0));
    }
    out
}

/// The size of a symbol: up to the next symbol of its atom, or the atom's
/// end.
fn symbol_size(object: &LinkObject<'_>, symbol: u32) -> u64 {
    let Some(atom) = object.atoms.symbol_atom(symbol) else {
        return 0;
    };
    let Ok(entry) = object.file.symbols().get(symbol) else {
        return 0;
    };
    let Some(section) = object.file.section_by_ordinal(u32::from(entry.n_sect)) else {
        return 0;
    };
    let Some(info) = object.atoms.atoms().get(atom) else {
        return 0;
    };
    let end = section
        .addr
        .saturating_add(info.offset)
        .saturating_add(info.size);
    let next = object
        .atoms
        .atom_symbols(atom)
        .iter()
        .filter_map(|&s| object.file.symbols().get(s).ok())
        .map(|s| s.n_value)
        .filter(|&v| v > entry.n_value)
        .min()
        .unwrap_or(end);
    next.min(end).saturating_sub(entry.n_value)
}
