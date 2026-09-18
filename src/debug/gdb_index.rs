//! `--gdb-index`: the `.gdb_index` section (format version 8), built from
//! the input objects' DWARF as lld builds it.
//!
//! The index has five parts: the compilation unit list (offset and size of
//! every unit in the output `.debug_info`), an empty type unit list, the
//! address area (address ranges and the unit covering each), a hash table
//! of names, and a constant pool of names and CU vectors (for each name,
//! the units that define it, with its kind and whether it is static). See
//! GDB's "Index Section Format".
//!
//! # How it is built
//!
//! 1. [`GdbIndex::build`], before layout, in parallel per object: the
//!    compilation units of the object's `.debug_info` (the one outside
//!    section groups), the address ranges of each unit DIE
//!    (`unit::unit_info`) that fall in live sections, and the names of
//!    `.debug_gnu_pubnames` and `.debug_gnu_pubtypes`. An object without
//!    those sections has its DIEs scanned instead ([`scan`]), which gives the
//!    names a compiler would have put there. Names are then gathered in 32
//!    shards by the high bits of their GDB hash, each in first-seen order,
//!    which is lld's order: the section is byte for byte lld's, apart from
//!    the version and the scanned names. The size is known at this point.
//! 2. [`GdbIndex::render`], after layout: the unit offsets and the
//!    addresses of the ranges' sections are filled in.
//!
//! Like lld, the index keeps address ranges of sections that ICF folded
//! (at the address of the section they fold into) and drops those of
//! sections `--gc-sections` removed, and the linker drops the input
//! `.debug_gnu_pubnames` and `.debug_gnu_pubtypes` sections from the
//! output ([`is_consumed`]).

pub mod elf;
pub mod input;
pub mod names;
pub mod scan;
pub mod unit;

use rayon::prelude::*;

use crate::elf::object::ObjectInput;
use crate::error::{Error, Result};

use input::{DebugObject, Malformed};
use names::NameEntry;
use unit::{Abbrevs, UnitHeader};

/// The version written in the header. Version 8 has the format of version
/// 7 (lld writes 7); it tells GDB that symbols of type units refer to the
/// type unit, which does not matter here as the type unit list is empty.
pub const VERSION: u32 = 8;

/// Header size: six 32-bit words.
const HEADER_SIZE: u64 = 24;

/// Whether the input section `name` is consumed by the index and not
/// copied to the output.
#[must_use]
pub fn is_consumed(name: &[u8]) -> bool {
    name == b".debug_gnu_pubnames" || name == b".debug_gnu_pubtypes"
}

/// One object's contribution.
#[derive(Debug, Default)]
struct Chunk {
    /// The object, by index in the caller's file list.
    file: usize,
    /// Its `.debug_info` section.
    info: Option<u32>,
    /// Compilation units: offset in the section and size.
    units: Vec<(u64, u64)>,
    /// Address areas: section, low and high offsets in it, and unit index
    /// in the object.
    areas: Vec<(u32, u64, u64, u32)>,
}

/// A name of the symbol table.
#[derive(Debug)]
struct Symbol<'a> {
    name: std::borrow::Cow<'a, [u8]>,
    hash: u32,
    cu_vector: Vec<u32>,
    cu_vector_offset: u32,
    name_offset: u32,
}

/// A planned `.gdb_index`.
#[derive(Debug, Default)]
pub struct GdbIndex<'a> {
    chunks: Vec<Chunk>,
    symbols: Vec<Symbol<'a>>,
    symtab_slots: u64,
    pool_size: u64,
    /// Problems found in the input, as `(file index, section index,
    /// problem)`, in input order. The objects are indexed anyway, without
    /// the parts that could not be read.
    pub problems: Vec<(usize, u32, Malformed)>,
}

impl<'a> GdbIndex<'a> {
    /// Reads the inputs: `objects` are the live objects as (index, object),
    /// in input order; `live` tells whether section `section` of file
    /// `file` is in the output (sections ICF folds count as live).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Malformed`] for unreadable relocation sections, and
    /// [`Error::Limit`] when the constant pool passes 4 GiB.
    pub fn build(
        objects: &[(usize, &ObjectInput<'a>)],
        live: &(dyn Fn(usize, u32) -> bool + Sync),
    ) -> Result<Self> {
        let read: Vec<Result<ObjectRead<'a>>> = objects
            .par_iter()
            .map(|&(file, object)| read_object(file, object, live))
            .collect();
        let mut chunks = Vec::new();
        let mut names = Vec::new();
        let mut problems = Vec::new();
        for (result, &(file, _)) in read.into_iter().zip(objects) {
            let (chunk, entries, found) = result?;
            problems.extend(found.into_iter().map(|(s, p)| (file, s, p)));
            if let Some(chunk) = chunk {
                chunks.push(chunk);
                names.push(entries);
            }
        }
        let (symbols, pool_size) = create_symbols(&chunks, &names)?;
        let symtab_slots = symtab_slots(symbols.len());
        Ok(Self {
            chunks,
            symbols,
            symtab_slots,
            pool_size,
            problems,
        })
    }

    /// Whether there is nothing to index (no object has `.debug_info`): lld
    /// then writes no section.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }

    /// The size of the section.
    #[must_use]
    pub fn size(&self) -> u64 {
        let units: u64 = self.chunks.iter().map(|c| c.units.len() as u64).sum();
        let areas: u64 = self.chunks.iter().map(|c| c.areas.len() as u64).sum();
        HEADER_SIZE
            .saturating_add(units.saturating_mul(16))
            .saturating_add(areas.saturating_mul(20))
            .saturating_add(self.symtab_slots.saturating_mul(8))
            .saturating_add(self.pool_size)
    }

    /// Writes the section. `info_offset(file, section)` is the offset of an
    /// input `.debug_info` section in the output one; `address(file,
    /// section)` is the address of a code section (of the section it was
    /// folded into).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Limit`] if the section does not fit in memory.
    pub fn render(
        &self,
        info_offset: &dyn Fn(usize, u32) -> Option<u64>,
        address: &dyn Fn(usize, u32) -> Option<u64>,
    ) -> Result<Vec<u8>> {
        let size = usize::try_from(self.size())
            .map_err(|_| Error::Limit(".gdb_index larger than memory".into()))?;
        let mut out = Vec::with_capacity(size);
        let words = |out: &mut Vec<u8>, v: u32| out.extend_from_slice(&v.to_le_bytes());
        let offset32 = |v: usize| u32::try_from(v).unwrap_or(u32::MAX);

        // Header; offsets are filled in as the parts are written.
        out.resize(HEADER_SIZE as usize, 0);
        let cu_list = out.len();
        for chunk in &self.chunks {
            let base = chunk
                .info
                .and_then(|s| info_offset(chunk.file, s))
                .unwrap_or(0);
            for &(offset, length) in &chunk.units {
                out.extend_from_slice(&base.wrapping_add(offset).to_le_bytes());
                out.extend_from_slice(&length.to_le_bytes());
            }
        }
        let areas = out.len();
        let mut preceding = 0u32;
        for chunk in &self.chunks {
            for &(section, low, high, unit) in &chunk.areas {
                let base = address(chunk.file, section).unwrap_or(0);
                out.extend_from_slice(&base.wrapping_add(low).to_le_bytes());
                out.extend_from_slice(&base.wrapping_add(high).to_le_bytes());
                words(&mut out, unit.wrapping_add(preceding));
            }
            preceding =
                preceding.wrapping_add(u32::try_from(chunk.units.len()).unwrap_or(u32::MAX));
        }
        let symtab = out.len();
        let slots = usize::try_from(self.symtab_slots).unwrap_or(0);
        let mut table = vec![(0u32, 0u32); slots];
        let mask = u32::try_from(slots.saturating_sub(1)).unwrap_or(u32::MAX);
        for symbol in &self.symbols {
            let mut i = symbol.hash & mask;
            let step = (symbol.hash.wrapping_mul(17) & mask) | 1;
            // The table has room: at most 3/4 full.
            while table.get(i as usize).is_some_and(|slot| slot.0 != 0) {
                i = i.wrapping_add(step) & mask;
            }
            if let Some(slot) = table.get_mut(i as usize) {
                *slot = (symbol.name_offset, symbol.cu_vector_offset);
            }
        }
        for (name, vector) in table {
            words(&mut out, name);
            words(&mut out, vector);
        }
        let pool = out.len();
        for symbol in &self.symbols {
            words(&mut out, u32::try_from(symbol.cu_vector.len()).unwrap_or(0));
            for &value in &symbol.cu_vector {
                words(&mut out, value);
            }
        }
        for symbol in &self.symbols {
            out.extend_from_slice(&symbol.name);
            out.push(0);
        }
        let header = [
            VERSION,
            offset32(cu_list),
            offset32(areas),
            offset32(areas),
            offset32(symtab),
            offset32(pool),
        ];
        for (slot, value) in out.as_chunks_mut::<4>().0.iter_mut().zip(header) {
            *slot = value.to_le_bytes();
        }
        if out.len() != size {
            return Err(Error::Internal(format!(
                ".gdb_index is {} bytes, {size} planned",
                out.len()
            )));
        }
        Ok(out)
    }
}

/// lld's hash table size: 4/3 of the names rounded up past a power of
/// two, at least 1024 slots.
fn symtab_slots(names: usize) -> u64 {
    let wanted = (names as u64).saturating_mul(4) / 3;
    // LLVM's NextPowerOf2: the next power of two strictly greater.
    let next = wanted
        .checked_add(1)
        .and_then(u64::checked_next_power_of_two)
        .unwrap_or(u64::MAX);
    next.max(1024)
}

/// What [`read_object`] finds in one object.
type ObjectRead<'a> = (Option<Chunk>, Vec<NameEntry<'a>>, Vec<(u32, Malformed)>);

/// Reads one object: its chunk (if it has a live `.debug_info`), its
/// names, and the problems found.
fn read_object<'a>(
    file: usize,
    object: &ObjectInput<'a>,
    live: &(dyn Fn(usize, u32) -> bool + Sync),
) -> Result<ObjectRead<'a>> {
    let obj = DebugObject::new(object, &|section| live(file, section))?;
    let mut problems = Vec::new();
    if !obj.has_info {
        return Ok((None, Vec::new(), problems));
    }
    let mut chunk = Chunk {
        file,
        info: obj.info.as_ref().map(|s| s.index),
        ..Chunk::default()
    };
    let mut names = Vec::new();
    let Some(info) = obj.info.as_ref() else {
        return Ok((Some(chunk), names, problems));
    };
    let (headers, error) = unit::unit_headers(&obj, info, false);
    if let Some(error) = error {
        problems.push((info.index, error));
    }
    let units: Vec<&UnitHeader> = headers.iter().filter(|u| !u.is_type_unit()).collect();
    chunk.units = units
        .iter()
        // lld records the unit length plus 4 (the DWARF 32 size).
        .map(|u| (u.offset as u64, u.length.saturating_add(4)))
        .collect();
    let mut abbrevs = Abbrevs::default();
    let scan = obj.gnu_pubnames.is_none() && obj.gnu_pubtypes.is_none();
    let type_units = if scan {
        scan::TypeUnits::read(&obj, &mut abbrevs, &mut problems)
    } else {
        scan::TypeUnits::default()
    };
    let mut failed = false;
    for (index, unit) in units.iter().enumerate() {
        let index = u32::try_from(index).unwrap_or(u32::MAX);
        let table = match abbrevs.get(&obj, unit) {
            Ok(table) => table,
            Err(e) => {
                problems.push((info.index, e));
                failed = true;
                break;
            }
        };
        let info_die = match unit::unit_info(&obj, info, unit, table) {
            Ok(Some(die)) => die,
            Ok(None) => continue,
            Err(e) => {
                problems.push((info.index, e));
                failed = true;
                break;
            }
        };
        for &(section, low, high) in &info_die.ranges {
            // Empty ranges have no effect; ranges of removed sections go.
            if low != high && live(file, section) {
                chunk.areas.push((section, low, high, index));
            }
        }
        if scan
            && let Err(e) = scan::names(
                &obj,
                info,
                unit,
                table,
                &info_die,
                index,
                &type_units,
                &mut names,
            )
        {
            problems.push((info.index, e));
        }
    }
    if failed {
        // lld gives up on the address areas of an object whose units it
        // cannot read.
        chunk.areas.clear();
    }
    if !scan {
        let offsets: Vec<u64> = chunk.units.iter().map(|&(o, _)| o).collect();
        names::pub_sections(&obj, &offsets, &mut names, &mut problems);
    }
    Ok((Some(chunk), names, problems))
}

/// Gathers the names into symbols as lld does: 32 shards by the top five
/// bits of the hash, each in first-seen order, their CU vectors in the
/// same order (duplicates kept). Returns the symbols with their offsets in
/// the constant pool, and the pool's size.
fn create_symbols<'a>(
    chunks: &[Chunk],
    names: &[Vec<NameEntry<'a>>],
) -> Result<(Vec<Symbol<'a>>, u64)> {
    const SHARDS: usize = 32;
    let mut first_unit = Vec::with_capacity(chunks.len());
    let mut units = 0u32;
    for chunk in chunks {
        first_unit.push(units);
        units = units.wrapping_add(u32::try_from(chunk.units.len()).unwrap_or(u32::MAX));
    }
    let shards: Vec<Vec<Symbol<'a>>> = (0..SHARDS)
        .into_par_iter()
        .map(|shard| {
            let mut symbols: Vec<Symbol<'a>> = Vec::new();
            let mut index: hashbrown::HashMap<&[u8], usize, foldhash::fast::FixedState> =
                hashbrown::HashMap::with_hasher(foldhash::fast::FixedState::default());
            for (entries, &first) in names.iter().zip(&first_unit) {
                for entry in entries {
                    if (entry.hash >> 27) as usize != shard {
                        continue;
                    }
                    let value = entry.value.wrapping_add(first);
                    match index.get(&*entry.name) {
                        Some(&at) => {
                            if let Some(symbol) = symbols.get_mut(at) {
                                symbol.cu_vector.push(value);
                            }
                        }
                        None => {
                            index.insert(&entry.name, symbols.len());
                            symbols.push(Symbol {
                                name: entry.name.clone(),
                                hash: entry.hash,
                                cu_vector: vec![value],
                                cu_vector_offset: 0,
                                name_offset: 0,
                            });
                        }
                    }
                }
            }
            symbols
        })
        .collect();
    let mut symbols: Vec<Symbol<'a>> = shards.into_iter().flatten().collect();
    let too_big = || Error::Limit("--gdb-index: constant pool exceeds 4 GiB".into());
    let mut offset = 0u64;
    for symbol in &mut symbols {
        symbol.cu_vector_offset = u32::try_from(offset).map_err(|_| too_big())?;
        offset = offset.saturating_add(
            (symbol.cu_vector.len() as u64)
                .saturating_add(1)
                .saturating_mul(4),
        );
    }
    for symbol in &mut symbols {
        symbol.name_offset = u32::try_from(offset).map_err(|_| too_big())?;
        offset = offset.saturating_add((symbol.name.len() as u64).saturating_add(1));
    }
    if u32::try_from(offset).is_err() {
        return Err(too_big());
    }
    Ok((symbols, offset))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symtab_slots_follow_lld() {
        assert_eq!(symtab_slots(0), 1024);
        assert_eq!(symtab_slots(768), 2048);
        assert_eq!(symtab_slots(767), 1024);
        assert_eq!(symtab_slots(3000), 4096);
    }
}
