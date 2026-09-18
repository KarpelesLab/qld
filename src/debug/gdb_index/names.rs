//! The names an object contributes to `.gdb_index`: from its
//! `.debug_gnu_pubnames` and `.debug_gnu_pubtypes` sections when it has
//! them (as lld reads them), otherwise from its DIEs ([`super::scan`]).

use std::borrow::Cow;

use super::input::{DebugObject, Malformed, Reader, Section};

/// A name with its GDB hash and its CU vector value within its object:
/// `(kind and static bits) << 24 | CU index in the object`.
#[derive(Clone, Debug)]
pub(crate) struct NameEntry<'a> {
    /// The name (qualified names built by the DIE scan are owned).
    pub name: Cow<'a, [u8]>,
    /// [`gdb_hash`] of the name.
    pub hash: u32,
    /// The CU vector entry, with the CU index relative to the object.
    pub value: u32,
}

/// The hash `.gdb_index` uses for names (version 5 and later): the same
/// function as `mapped_index::hash` in GDB, lowercasing ASCII.
#[must_use]
pub(crate) fn gdb_hash(name: &[u8]) -> u32 {
    let mut h = 0u32;
    for &c in name {
        h = h
            .wrapping_mul(67)
            .wrapping_add(u32::from(c.to_ascii_lowercase()))
            .wrapping_sub(113);
    }
    h
}

/// Reads `.debug_gnu_pubnames` and `.debug_gnu_pubtypes` (LLVM's
/// `DWARFDebugPubTable` in GNU style). `cu_offsets` are the offsets of the
/// object's compilation units, in order; a set names its unit by offset.
///
/// A malformed set keeps the entries read before the problem and ends the
/// section, which is reported.
pub(crate) fn pub_sections<'a>(
    obj: &DebugObject<'_, 'a>,
    cu_offsets: &[u64],
    out: &mut Vec<NameEntry<'a>>,
    problems: &mut Vec<(u32, Malformed)>,
) {
    for section in [&obj.gnu_pubnames, &obj.gnu_pubtypes].into_iter().flatten() {
        if let Err(e) = pub_section(obj, section, cu_offsets, out) {
            problems.push((section.index, e));
        }
    }
}

fn pub_section<'a>(
    obj: &DebugObject<'_, 'a>,
    section: &Section<'a>,
    cu_offsets: &[u64],
    out: &mut Vec<NameEntry<'a>>,
) -> Result<(), Malformed> {
    let data = section.data;
    let mut offset = 0usize;
    while offset < data.len() {
        let mut r = Reader::at(data, offset);
        let (length, offset_size) = r.initial_length()?;
        let next = usize::try_from(length)
            .ok()
            .and_then(|l| r.pos().checked_add(l))
            .ok_or_else(|| r.error("name set length"))?;
        let set = data.get(..next.min(data.len())).unwrap_or_default();
        let mut r = Reader::at(set, r.pos());
        let _version = r.u16()?;
        let pos = r.pos();
        let raw = r.uint(offset_size)?;
        let (unit_offset, _) = obj.relocate(section, pos, raw);
        let _unit_size = r.uint(offset_size)?;
        let cu = cu_offsets.partition_point(|&o| o < unit_offset);
        let cu = u32::try_from(cu).unwrap_or(u32::MAX) & 0x00ff_ffff;
        loop {
            let die = r.uint(offset_size)?;
            if die == 0 {
                break;
            }
            let descriptor = r.u8()?;
            let name = r.cstr()?;
            out.push(NameEntry {
                name: Cow::Borrowed(name),
                hash: gdb_hash(name),
                value: (u32::from(descriptor & 0xf0) << 24) | cu,
            });
        }
        if next > data.len() {
            return Err(r.error("name set length (past the end of the section)"));
        }
        offset = next;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gdb_hash_matches_gdb() {
        // Values from GDB's `mapped_index` hash (version 5+).
        assert_eq!(gdb_hash(b""), 0);
        assert_eq!(gdb_hash(b"a"), 97u32.wrapping_sub(113));
        assert_eq!(gdb_hash(b"Main"), gdb_hash(b"main"));
    }
}
