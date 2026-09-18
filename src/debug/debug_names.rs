//! `--debug-names`: one `.debug_names` name index (DWARF 5 section 6.1.1)
//! merged from the objects' own, as lld 18 and later merge them.
//!
//! Each object's `.debug_names` holds one or more name indexes, usually
//! one per compilation unit. The merged index lists every unit of every
//! index; its names are the union of theirs, each with all its entries.
//!
//! 1. [`DebugNames::build`] parses the inputs in parallel. Abbreviations
//!    are then merged in input order into one table (identical ones share
//!    a code), with `DW_IDX_compile_unit` moved to the end of every
//!    abbreviation and widened to the size the merged unit count needs.
//!    Names are gathered in 32 shards by the top bits of their hash, each
//!    in first-seen order, and the entry pool is laid out: entries keep
//!    their order, their unit indexes are rebased, and `DW_IDX_parent`
//!    references are moved to the parent entries' new offsets. All sizes
//!    are known at this point.
//! 2. [`DebugNames::render`], after layout, relocates the unit offsets and
//!    the name string offsets (into the merged `.debug_str`).
//!
//! The name hashes are recomputed ([`hash`]). The input sections are not
//! copied to the output. This follows lld's `DebugNamesBaseSection`
//! (sharding and ordering included, so the section is lld's byte for
//! byte), except that local and foreign type unit lists are merged too
//! (lld drops them and warns); the `DW_IDX_type_unit` of an entry is
//! rebased like its unit index, and entries of type units get no
//! `DW_IDX_compile_unit`. Only DWARF 32 indexes are read, as in lld.

pub mod hash;

use rayon::prelude::*;

use crate::elf::object::ObjectInput;
use crate::error::{Error, Result};

use super::gdb_index::input::{DebugObject, Malformed, Reader, Section, string_at};

// Index attributes and forms.
const DW_IDX_COMPILE_UNIT: u64 = 1;
const DW_IDX_TYPE_UNIT: u64 = 2;
const DW_IDX_PARENT: u64 = 4;
const DW_FORM_DATA1: u64 = 0x0b;
const DW_FORM_DATA2: u64 = 0x05;
const DW_FORM_DATA4: u64 = 0x06;
const DW_FORM_DATA8: u64 = 0x07;
const DW_FORM_REF1: u64 = 0x11;
const DW_FORM_REF2: u64 = 0x12;
const DW_FORM_REF4: u64 = 0x13;
const DW_FORM_REF8: u64 = 0x14;
const DW_FORM_FLAG_PRESENT: u64 = 0x19;

/// Shards names are gathered in (lld's `numShards`).
const SHARDS: usize = 32;

/// A relocated 32-bit field of an input index: the section its relocation
/// points into, and the value (an offset in that section).
#[derive(Clone, Copy, Debug)]
struct Field {
    file: usize,
    section: Option<u32>,
    value: u64,
}

/// An abbreviation of an input index.
#[derive(Clone, Debug)]
struct InputAbbrev {
    code: u64,
    tag: u64,
    attrs: Vec<(u64, u64)>,
}

/// An entry of an input index.
#[derive(Clone, Debug)]
struct InputEntry {
    /// Global entry number (for parent references).
    id: u32,
    /// Offset in the input section.
    offset: u64,
    code: u64,
    /// Attribute values in abbreviation order, with their sizes; `None`
    /// for `DW_FORM_flag_present`.
    values: Vec<Option<(u64, u8)>>,
    /// `DW_IDX_parent` (ref4): the parent entry's offset in the section.
    parent: Option<u64>,
    /// The parent entry's id, once entries are numbered.
    parent_id: Option<u32>,
}

/// A name of an input index.
#[derive(Clone, Debug)]
struct InputName<'a> {
    name: &'a [u8],
    hash: u32,
    string: Field,
    entries: Vec<InputEntry>,
}

/// One input name index.
#[derive(Clone, Debug, Default)]
struct InputIndex<'a> {
    units: Vec<Field>,
    local_types: Vec<Field>,
    foreign_types: Vec<u64>,
    augmentation: &'a [u8],
    abbrevs: Vec<InputAbbrev>,
    names: Vec<InputName<'a>>,
}

/// An entry of the merged index.
#[derive(Clone, Debug)]
struct Entry {
    code: u32,
    values: Vec<(u64, u8)>,
    /// For `DW_IDX_parent` (ref4): the value slot and the parent's id.
    parent: Option<(usize, u32)>,
    id: u32,
}

/// A name of the merged index.
#[derive(Clone, Debug)]
struct Name {
    hash: u32,
    string: Field,
    entry_offset: u32,
    entries: Vec<Entry>,
}

/// A planned merged `.debug_names`.
#[derive(Debug, Default)]
pub struct DebugNames {
    units: Vec<Field>,
    local_types: Vec<Field>,
    foreign_types: Vec<u64>,
    augmentation: Vec<u8>,
    abbrev_table: Vec<u8>,
    /// Names in entry pool order.
    names: Vec<Name>,
    bucket_count: u32,
    pool_size: u64,
    /// The input sections merged, as (file, section index).
    pub inputs: Vec<(usize, u32)>,
}

/// The size in bytes and form of an index attribute holding values below
/// `count` (LLVM's `DIEInteger::BestForm` for unsigned values).
fn index_form(count: usize) -> (u8, u64) {
    if count > usize::from(u16::MAX) {
        (4, DW_FORM_DATA4)
    } else if count > usize::from(u8::MAX) {
        (2, DW_FORM_DATA2)
    } else {
        (1, DW_FORM_DATA1)
    }
}

/// The bucket count DWARF producers use (LLVM's
/// `getDebugNamesBucketCount`).
fn bucket_count(names: usize) -> u32 {
    let names = u32::try_from(names).unwrap_or(u32::MAX);
    if names > 1024 {
        names / 4
    } else if names > 16 {
        names / 2
    } else {
        names.max(1)
    }
}

fn uleb_len(mut value: u64) -> u64 {
    let mut len = 1u64;
    while value >= 0x80 {
        value >>= 7;
        len = len.saturating_add(1);
    }
    len
}

fn push_uleb(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

impl DebugNames {
    /// Reads and merges the `.debug_names` sections of `objects` (the live
    /// objects as (index, object), in input order); `live` tells whether a
    /// section of a file is in the output.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Malformed`] for indexes that cannot be read (DWARF
    /// 64 and versions other than 5 included), and [`Error::Limit`] for
    /// indexes past 4 GiB.
    pub fn build<'a>(
        objects: &[(usize, &ObjectInput<'a>)],
        live: &(dyn Fn(usize, u32) -> bool + Sync),
    ) -> Result<Self> {
        // Parse every object's last `.debug_names`, as lld picks it.
        type Parsed<'a> = Result<Option<(u32, Vec<InputIndex<'a>>)>>;
        let parsed: Vec<Parsed<'a>> = objects
            .par_iter()
            .map(|&(file, object)| {
                let obj = DebugObject::new(object, &|s| live(file, s))?;
                let Some(section) = obj.names.last() else {
                    return Ok(None);
                };
                let indexes = parse_section(&obj, file, section)
                    .map_err(|e| object.malformed(section_offset(object, section, e), e.what))?;
                Ok(Some((section.index, indexes)))
            })
            .collect();
        let mut chunks: Vec<(usize, Vec<InputIndex<'a>>)> = Vec::new();
        let mut this = Self::default();
        for (result, &(file, _)) in parsed.into_iter().zip(objects) {
            if let Some((section, indexes)) = result? {
                this.inputs.push((file, section));
                chunks.push((file, indexes));
            }
        }
        // Number the entries, and find each entry's parent (an entry of
        // the same index, by offset).
        let mut next_id = 0u32;
        for (_, indexes) in &mut chunks {
            for index in indexes {
                let mut by_offset: Vec<(u64, u32)> = Vec::new();
                for name in &mut index.names {
                    for entry in &mut name.entries {
                        entry.id = next_id;
                        by_offset.push((entry.offset, next_id));
                        next_id = next_id.checked_add(1).ok_or_else(|| {
                            Error::Limit("--debug-names: too many entries".into())
                        })?;
                    }
                }
                by_offset.sort_unstable();
                for name in &mut index.names {
                    for entry in &mut name.entries {
                        entry.parent_id = entry.parent.and_then(|offset| {
                            let at = by_offset.binary_search_by_key(&offset, |&(o, _)| o).ok()?;
                            by_offset.get(at).map(|&(_, id)| id)
                        });
                    }
                }
            }
        }
        this.merge(&chunks)?;
        Ok(this)
    }

    /// Whether no input had an index.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inputs.is_empty()
    }

    fn merge(&mut self, chunks: &[(usize, Vec<InputIndex<'_>>)]) -> Result<()> {
        let all = || chunks.iter().flat_map(|(_, indexes)| indexes.iter());
        for index in all() {
            self.units.extend_from_slice(&index.units);
            self.local_types.extend_from_slice(&index.local_types);
            self.foreign_types.extend_from_slice(&index.foreign_types);
        }
        // The augmentation string, if all indexes agree on it.
        let mut augmentation: Option<&[u8]> = None;
        for index in all() {
            match augmentation {
                None => augmentation = Some(index.augmentation),
                Some(a) if a != index.augmentation => {
                    augmentation = Some(&[]);
                    break;
                }
                Some(_) => {}
            }
        }
        self.augmentation = augmentation.unwrap_or_default().to_vec();

        let (cu_size, cu_form) = index_form(self.units.len());
        let type_units = self
            .local_types
            .len()
            .saturating_add(self.foreign_types.len());
        let (tu_size, tu_form) = index_form(type_units);

        // Merged abbreviations, with the old codes of each index mapped.
        type Attrs = Vec<(u64, u64)>;
        let mut table: Vec<(u64, Attrs)> = Vec::new();
        let mut code_maps: Vec<Vec<(u64, u32)>> = Vec::new();
        for index in all() {
            let mut map = Vec::with_capacity(index.abbrevs.len());
            // lld walks each index's abbreviations in the order of LLVM's
            // hash set of them, which decides the merged codes.
            for abbrev in dense_set_order(&index.abbrevs, |a| a.code) {
                let mut attrs: Attrs = Vec::with_capacity(abbrev.attrs.len().saturating_add(1));
                let mut type_unit = false;
                for &(idx, form) in &abbrev.attrs {
                    match idx {
                        DW_IDX_COMPILE_UNIT => {}
                        DW_IDX_TYPE_UNIT => {
                            type_unit = true;
                            attrs.push((idx, tu_form));
                        }
                        _ => attrs.push((idx, form)),
                    }
                }
                if !type_unit {
                    attrs.push((DW_IDX_COMPILE_UNIT, cu_form));
                }
                let key = (abbrev.tag, attrs);
                let code = match table.iter().position(|t| *t == key) {
                    Some(at) => at,
                    None => {
                        table.push(key);
                        table.len().saturating_sub(1)
                    }
                };
                let code = u32::try_from(code.saturating_add(1)).unwrap_or(u32::MAX);
                map.push((abbrev.code, code));
            }
            code_maps.push(map);
        }
        let mut abbrev_table = Vec::new();
        for (code, (tag, attrs)) in table.iter().enumerate() {
            push_uleb(&mut abbrev_table, (code as u64).saturating_add(1));
            push_uleb(&mut abbrev_table, *tag);
            for &(idx, form) in attrs {
                push_uleb(&mut abbrev_table, idx);
                push_uleb(&mut abbrev_table, form);
            }
            abbrev_table.extend_from_slice(&[0, 0]);
        }
        abbrev_table.push(0);
        self.abbrev_table = abbrev_table;

        // Each index's first unit and type unit in the merged lists.
        let mut bases = Vec::new();
        let (mut units, mut locals, mut foreigns) = (0u64, 0u64, 0u64);
        for index in all() {
            bases.push((units, locals, foreigns));
            units = units.saturating_add(index.units.len() as u64);
            locals = locals.saturating_add(index.local_types.len() as u64);
            foreigns = foreigns.saturating_add(index.foreign_types.len() as u64);
        }
        let total_locals = locals;

        // Rewrite the entries and gather the names in shards.
        let indexes: Vec<&InputIndex<'_>> = all().collect();
        let shards: Vec<Vec<Name>> = (0..SHARDS)
            .into_par_iter()
            .map(|shard| {
                let mut names: Vec<Name> = Vec::new();
                let mut seen: hashbrown::HashMap<&[u8], usize, foldhash::fast::FixedState> =
                    hashbrown::HashMap::with_hasher(foldhash::fast::FixedState::default());
                for (i, index) in indexes.iter().enumerate() {
                    let map = code_maps.get(i).map_or(&[][..], Vec::as_slice);
                    let (unit_base, local_base, foreign_base) =
                        bases.get(i).copied().unwrap_or_default();
                    for name in &index.names {
                        if (name.hash >> 27) as usize != shard {
                            continue;
                        }
                        let entries = name.entries.iter().map(|entry| {
                            rewrite(
                                entry,
                                index,
                                map,
                                cu_size,
                                tu_size,
                                unit_base,
                                (local_base, total_locals, foreign_base),
                            )
                        });
                        match seen.get(name.name) {
                            Some(&at) => {
                                if let Some(merged) = names.get_mut(at) {
                                    merged.entries.extend(entries);
                                }
                            }
                            None => {
                                seen.insert(name.name, names.len());
                                names.push(Name {
                                    hash: name.hash,
                                    string: name.string,
                                    entry_offset: 0,
                                    entries: entries.collect(),
                                });
                            }
                        }
                    }
                }
                names
            })
            .collect();
        self.names = shards.into_iter().flatten().collect();

        // Lay out the entry pool, then point parents at their new offsets.
        let too_big = || Error::Limit("--debug-names: entry pool exceeds 4 GiB".into());
        let mut offsets = vec![0u32; usize::try_from(self.entry_count()).unwrap_or(0)];
        let mut offset = 0u64;
        for name in &mut self.names {
            name.entry_offset = u32::try_from(offset).map_err(|_| too_big())?;
            for entry in &name.entries {
                if let Some(slot) = offsets.get_mut(entry.id as usize) {
                    *slot = u32::try_from(offset).map_err(|_| too_big())?;
                }
                offset = offset.saturating_add(uleb_len(u64::from(entry.code)));
                for &(_, size) in &entry.values {
                    offset = offset.saturating_add(u64::from(size));
                }
            }
            offset = offset.saturating_add(1);
        }
        u32::try_from(offset).map_err(|_| too_big())?;
        self.pool_size = offset;
        for name in &mut self.names {
            for entry in &mut name.entries {
                if let Some((slot, parent)) = entry.parent
                    && let (Some(value), Some(&new)) =
                        (entry.values.get_mut(slot), offsets.get(parent as usize))
                {
                    value.0 = u64::from(new);
                }
            }
        }
        self.bucket_count = bucket_count(self.names.len());
        Ok(())
    }

    fn entry_count(&self) -> u64 {
        self.names
            .iter()
            .map(|n| n.entries.len() as u64)
            .sum::<u64>()
    }

    /// The size of the section.
    #[must_use]
    pub fn size(&self) -> u64 {
        let names = self.names.len() as u64;
        let header = 36u64.saturating_add(self.augmentation.len() as u64);
        header
            .saturating_add((self.units.len() as u64).saturating_mul(4))
            .saturating_add((self.local_types.len() as u64).saturating_mul(4))
            .saturating_add((self.foreign_types.len() as u64).saturating_mul(8))
            .saturating_add(u64::from(self.bucket_count).saturating_mul(4))
            .saturating_add(names.saturating_mul(12))
            .saturating_add(self.abbrev_table.len() as u64)
            .saturating_add(self.pool_size)
    }

    /// Writes the section. `offset(file, section, value)` relocates a field:
    /// the output offset of offset `value` of input section `section`
    /// (unit offsets into `.debug_info`, names into `.debug_str`).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Limit`] for sections past 4 GiB.
    pub fn render(
        &self,
        offset: &(dyn Fn(usize, u32, u64) -> Option<u64> + Sync),
    ) -> Result<Vec<u8>> {
        let size = self.size();
        let length = u32::try_from(size.saturating_sub(4))
            .ok()
            .filter(|&l| l < 0xffff_fff0)
            .ok_or_else(|| Error::Limit("--debug-names: index exceeds 4 GiB".into()))?;
        let mut out = Vec::with_capacity(usize::try_from(size).unwrap_or(0));
        let w32 = |out: &mut Vec<u8>, v: u32| out.extend_from_slice(&v.to_le_bytes());
        let count = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
        let field = |f: &Field| -> u32 {
            f.section
                .and_then(|s| offset(f.file, s, f.value))
                .map_or(0, |v| u32::try_from(v).unwrap_or(u32::MAX))
        };
        w32(&mut out, length);
        out.extend_from_slice(&5u16.to_le_bytes());
        out.extend_from_slice(&[0, 0]);
        w32(&mut out, count(self.units.len()));
        w32(&mut out, count(self.local_types.len()));
        w32(&mut out, count(self.foreign_types.len()));
        w32(&mut out, self.bucket_count);
        w32(&mut out, count(self.names.len()));
        w32(&mut out, count(self.abbrev_table.len()));
        w32(&mut out, count(self.augmentation.len()));
        out.extend_from_slice(&self.augmentation);
        for unit in &self.units {
            w32(&mut out, field(unit));
        }
        for unit in &self.local_types {
            w32(&mut out, field(unit));
        }
        for &signature in &self.foreign_types {
            out.extend_from_slice(&signature.to_le_bytes());
        }
        // Buckets: names by hash modulo the bucket count, in pool order.
        let buckets = self.bucket_count.max(1);
        let mut by_bucket: Vec<Vec<&Name>> = vec![Vec::new(); buckets as usize];
        for name in &self.names {
            let at = name.hash.checked_rem(buckets).unwrap_or(0) as usize;
            if let Some(bucket) = by_bucket.get_mut(at) {
                bucket.push(name);
            }
        }
        let mut first = 1u32;
        for bucket in &by_bucket {
            w32(&mut out, if bucket.is_empty() { 0 } else { first });
            first = first.saturating_add(count(bucket.len()));
        }
        let ordered = || by_bucket.iter().flatten();
        for name in ordered() {
            w32(&mut out, name.hash);
        }
        for name in ordered() {
            w32(&mut out, field(&name.string));
        }
        for name in ordered() {
            w32(&mut out, name.entry_offset);
        }
        out.extend_from_slice(&self.abbrev_table);
        for name in &self.names {
            for entry in &name.entries {
                push_uleb(&mut out, u64::from(entry.code));
                for &(value, size) in &entry.values {
                    let bytes = value.to_le_bytes();
                    out.extend_from_slice(bytes.get(..usize::from(size)).unwrap_or_default());
                }
            }
            out.push(0);
        }
        if out.len() as u64 != size {
            return Err(Error::Internal(format!(
                ".debug_names is {} bytes, {size} planned",
                out.len()
            )));
        }
        Ok(out)
    }
}

/// `items` in the iteration order of an LLVM 23 `DenseSet` they were
/// inserted into in order, keyed by the 32-bit `key` (hash `key * 37`,
/// linear probing, at least 64 buckets, doubled when 3/4 full).
fn dense_set_order<T>(items: &[T], key: impl Fn(&T) -> u64) -> Vec<&T> {
    let mut buckets: Vec<Option<usize>> = vec![None; 64];
    let place = |buckets: &mut Vec<Option<usize>>, item: usize, k: u32| {
        let mask = buckets.len().saturating_sub(1);
        let mut at = (k.wrapping_mul(37) as usize) & mask;
        while buckets.get(at).is_some_and(Option::is_some) {
            at = at.wrapping_add(1) & mask;
        }
        if let Some(slot) = buckets.get_mut(at) {
            *slot = Some(item);
        }
    };
    let key32 = |item: usize| items.get(item).map_or(0, |i| key(i) as u32);
    for (count, item) in (1usize..).zip(0..items.len()) {
        if count.saturating_mul(4) >= buckets.len().saturating_mul(3) {
            let grown = vec![None; buckets.len().saturating_mul(2)];
            let old = core::mem::replace(&mut buckets, grown);
            for moved in old.into_iter().flatten() {
                place(&mut buckets, moved, key32(moved));
            }
        }
        place(&mut buckets, item, key32(item));
    }
    buckets
        .into_iter()
        .flatten()
        .filter_map(|i| items.get(i))
        .collect()
}

/// Rewrites an input entry for the merged index: new abbreviation code,
/// the unit index moved last and rebased, the type unit index rebased.
fn rewrite(
    entry: &InputEntry,
    index: &InputIndex<'_>,
    map: &[(u64, u32)],
    cu_size: u8,
    tu_size: u8,
    unit_base: u64,
    (local_base, total_locals, foreign_base): (u64, u64, u64),
) -> Entry {
    let code = map
        .iter()
        .find(|(old, _)| *old == entry.code)
        .map_or(0, |&(_, new)| new);
    let abbrev = index.abbrevs.iter().find(|a| a.code == entry.code);
    let mut values: Vec<(u64, u8)> = Vec::with_capacity(entry.values.len().saturating_add(1));
    let mut unit = 0u64;
    let mut type_unit = false;
    let mut parent = None;
    for (&(idx, _), value) in abbrev
        .map_or(&[][..], |a| a.attrs.as_slice())
        .iter()
        .zip(&entry.values)
    {
        let Some((value, size)) = *value else {
            continue;
        };
        match idx {
            DW_IDX_COMPILE_UNIT => unit = value,
            DW_IDX_TYPE_UNIT => {
                type_unit = true;
                let locals = index.local_types.len() as u64;
                let new = if value < locals {
                    local_base.saturating_add(value)
                } else {
                    total_locals
                        .saturating_add(foreign_base)
                        .saturating_add(value.saturating_sub(locals))
                };
                values.push((new, tu_size));
            }
            DW_IDX_PARENT => {
                if let Some(parent_id) = entry.parent_id {
                    parent = Some((values.len(), parent_id));
                }
                values.push((value, size));
            }
            _ => values.push((value, size)),
        }
    }
    if !type_unit {
        values.push((unit.saturating_add(unit_base), cu_size));
    }
    Entry {
        code,
        values,
        parent,
        id: entry.id,
    }
}

/// The file offset of a problem in `section`, for diagnostics.
fn section_offset(object: &ObjectInput<'_>, section: &Section<'_>, e: Malformed) -> u64 {
    object
        .section(section.index)
        .map_or(0, |s| s.header.sh_offset)
        .saturating_add(e.offset as u64)
}

/// Parses every name index of `section`.
fn parse_section<'a>(
    obj: &DebugObject<'_, 'a>,
    file: usize,
    section: &Section<'a>,
) -> core::result::Result<Vec<InputIndex<'a>>, Malformed> {
    let mut indexes = Vec::new();
    let mut offset = 0usize;
    while offset < section.data.len() {
        let (index, next) = parse_index(obj, file, section, offset)?;
        indexes.push(index);
        offset = next;
    }
    Ok(indexes)
}

/// Parses the name index at `start`; returns it and the offset after it.
fn parse_index<'a>(
    obj: &DebugObject<'_, 'a>,
    file: usize,
    section: &Section<'a>,
    start: usize,
) -> core::result::Result<(InputIndex<'a>, usize), Malformed> {
    let data = section.data;
    let mut r = Reader::at(data, start);
    let length = r.u32()?;
    if length >= 0xffff_fff0 {
        return Err(r.error("DWARF64 name index (unsupported)"));
    }
    let end = r
        .pos()
        .checked_add(length as usize)
        .filter(|&e| e <= data.len())
        .ok_or_else(|| r.error("name index length"))?;
    let data = data.get(..end).unwrap_or_default();
    let mut r = Reader::at(data, r.pos());
    let version = r.u16()?;
    if version != 5 {
        return Err(r.error("name index version (not 5)"));
    }
    r.skip(2)?;
    let mut counts = [0usize; 7];
    for count in &mut counts {
        *count = r.u32()? as usize;
    }
    let [
        units,
        locals,
        foreigns,
        buckets,
        names,
        abbrev_size,
        augmentation_size,
    ] = counts;
    let augmentation = r.bytes(augmentation_size)?;
    let field = |r: &mut Reader<'a>| -> core::result::Result<Field, Malformed> {
        let pos = r.pos();
        let raw = r.uint(4)?;
        let (value, section_index) = obj.relocate(section, pos, raw);
        Ok(Field {
            file,
            section: section_index,
            value,
        })
    };
    let bounded = |n: usize, size: usize, r: &Reader<'_>| {
        if n.saturating_mul(size) > data.len().saturating_sub(r.pos()) {
            Err(r.error("name index count (past the end)"))
        } else {
            Ok(())
        }
    };
    let mut index = InputIndex {
        augmentation,
        ..InputIndex::default()
    };
    bounded(units, 4, &r)?;
    for _ in 0..units {
        index.units.push(field(&mut r)?);
    }
    bounded(locals, 4, &r)?;
    for _ in 0..locals {
        index.local_types.push(field(&mut r)?);
    }
    bounded(foreigns, 8, &r)?;
    for _ in 0..foreigns {
        index.foreign_types.push(r.uint(8)?);
    }
    r.skip((buckets as u64).saturating_mul(4))?;
    if buckets > 0 {
        r.skip((names as u64).saturating_mul(4))?;
    }
    bounded(names, 8, &r)?;
    let mut strings = Vec::with_capacity(names);
    for _ in 0..names {
        strings.push(field(&mut r)?);
    }
    let mut entry_offsets = Vec::with_capacity(names);
    for _ in 0..names {
        entry_offsets.push(r.u32()?);
    }
    let abbrev_start = r.pos();
    let abbrev_end = abbrev_start
        .checked_add(abbrev_size)
        .filter(|&e| e <= data.len())
        .ok_or_else(|| r.error("abbreviation table size"))?;
    let mut a = Reader::at(data.get(..abbrev_end).unwrap_or_default(), abbrev_start);
    loop {
        let code = a.uleb()?;
        if code == 0 {
            break;
        }
        let tag = a.uleb()?;
        let mut attrs = Vec::new();
        loop {
            let idx = a.uleb()?;
            let form = a.uleb()?;
            if idx == 0 && form == 0 {
                break;
            }
            attrs.push((idx, form));
        }
        index.abbrevs.push(InputAbbrev { code, tag, attrs });
    }
    let pool = abbrev_end;
    // Every name has its own entries: a list read twice would make the
    // work quadratic in the section size.
    let mut sorted_offsets = entry_offsets.clone();
    sorted_offsets.sort_unstable();
    if sorted_offsets.windows(2).any(|w| w.first() == w.get(1)) {
        return Err(r.error("entry offset (shared by two names)"));
    }
    for (string, entry_offset) in strings.into_iter().zip(entry_offsets) {
        let name = string
            .section
            .and_then(|s| obj.data_of(s))
            .or_else(|| obj.str.as_ref().map(|s| s.data))
            .and_then(|d| string_at(d, string.value))
            .unwrap_or_default();
        let mut e = Reader::at(
            data,
            pool.checked_add(entry_offset as usize)
                .ok_or_else(|| r.error("entry offset"))?,
        );
        let mut entries = Vec::new();
        loop {
            let offset = e.pos() as u64;
            let code = e.uleb()?;
            if code == 0 {
                break;
            }
            let abbrev = index
                .abbrevs
                .iter()
                .find(|a| a.code == code)
                .ok_or_else(|| e.error("entry abbreviation code (not found)"))?;
            let mut values = Vec::with_capacity(abbrev.attrs.len());
            let mut parent = None;
            for &(idx, form) in &abbrev.attrs {
                let size: u8 = match form {
                    DW_FORM_FLAG_PRESENT => {
                        values.push(None);
                        continue;
                    }
                    DW_FORM_DATA1 | DW_FORM_REF1 => 1,
                    DW_FORM_DATA2 | DW_FORM_REF2 => 2,
                    DW_FORM_DATA4 | DW_FORM_REF4 => 4,
                    DW_FORM_DATA8 | DW_FORM_REF8 => 8,
                    _ => return Err(e.error("index attribute form (unsupported)")),
                };
                let value = e.uint(usize::from(size))?;
                if idx == DW_IDX_PARENT && form == DW_FORM_REF4 {
                    parent = Some((pool as u64).saturating_add(value));
                }
                values.push(Some((value, size)));
            }
            entries.push(InputEntry {
                id: 0,
                offset,
                code,
                values,
                parent,
                parent_id: None,
            });
        }
        index.names.push(InputName {
            name,
            hash: hash::hash(name),
            string,
            entries,
        });
    }
    Ok((index, end))
}
