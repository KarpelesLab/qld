//! Splitting sections into atoms, the unit of dead stripping.
//!
//! The rules follow lld's Mach-O port (which follows ld64):
//!
//! - **Regular sections** of an object with `MH_SUBSECTIONS_VIA_SYMBOLS`
//!   are split at the addresses of the symbols defined in them (`N_SECT`,
//!   not STABS). Several symbols at one address start a single atom;
//!   `N_ALT_ENTRY` symbols never start one. The bytes before the first
//!   symbol form an atom of their own, with no symbols. Without the flag, a
//!   section is one atom.
//! - **Literal sections** are split by content whatever the flag says:
//!   `S_CSTRING_LITERALS` into NUL-terminated strings, `S_4BYTE_LITERALS`,
//!   `S_8BYTE_LITERALS` and `S_16BYTE_LITERALS` into fixed-size literals, and
//!   `S_LITERAL_POINTERS` into pointers.
//! - **`__LD,__compact_unwind`** is split into its records: 32 bytes for
//!   64-bit objects, 20 for 32-bit.
//! - **Debug sections** (`S_ATTR_DEBUG`) are never split.
//!
//! An atom's alignment is the section's, reduced to the alignment of the
//! atom's offset within the section (`MinAlign` in lld), so an atom keeps its
//! position modulo the alignment it was assembled with.

use core::ops::Range;

use super::bytes::to_u64;
use super::consts::N_SECT;
use super::object::ObjectFile;
use super::reloc::PairedRelocation;
use super::section::{LiteralKind, Section};
use crate::error::Result;

/// How an atom was delimited.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AtomKind {
    /// Part of a regular section, delimited by symbols (or the whole
    /// section).
    Regular,
    /// One NUL-terminated string, terminator included.
    CString,
    /// One fixed-size literal (4, 8 or 16 bytes) or literal pointer.
    Literal,
    /// One `__compact_unwind` record.
    CompactUnwind,
    /// A whole debug section.
    Debug,
}

/// A contiguous piece of a section.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Atom {
    /// 0-based index of the section in [`ObjectFile::sections`].
    pub section: u32,
    /// Offset of the atom within the section.
    pub offset: u64,
    /// Size in bytes (possibly zero).
    pub size: u64,
    /// Alignment, as a power of two.
    pub align: u32,
    /// How the atom was delimited.
    pub kind: AtomKind,
    symbols: Range<u32>,
}

impl Atom {
    /// The offset range within the section.
    #[must_use]
    pub fn range(&self) -> Range<u64> {
        self.offset..self.offset.saturating_add(self.size)
    }
}

/// The atoms of an object.
#[derive(Clone, Debug, Default)]
pub struct Atomization {
    atoms: Vec<Atom>,
    /// Symbol indices, grouped by atom, in address then index order.
    symbols: Vec<u32>,
    /// Atom index range of each section.
    sections: Vec<Range<u32>>,
    /// Atom index of each symbol, or `u32::MAX`.
    symbol_atoms: Vec<u32>,
}

/// A relocation and the atom it applies to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AtomRelocation {
    /// Index of the atom containing the fixup.
    pub atom: usize,
    /// The relocation.
    pub relocation: PairedRelocation,
}

const NO_ATOM: u32 = u32::MAX;

/// Alignment of an atom at `offset` in a section aligned to `2^align`.
fn min_align(align: u32, offset: u64) -> u32 {
    if offset == 0 {
        align
    } else {
        align.min(offset.trailing_zeros())
    }
}

fn index_u32(value: usize, object: &ObjectFile<'_>) -> Result<u32> {
    u32::try_from(value).map_err(|_| object.source().malformed(0, "atom count (too many atoms)"))
}

impl Atomization {
    /// Splits every section of `object` into atoms.
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` if a symbol names a section that does not
    /// exist or lies outside its section, a C string section does not end
    /// with a NUL, or a literal or record section's size is not a multiple
    /// of its entry size.
    pub fn new(object: &ObjectFile<'_>) -> Result<Self> {
        let sections = object.sections();
        let source = object.source();
        let symtab = object.symbols();

        // Symbols defined in each section, as (offset, index, alt entry).
        let mut by_section: Vec<Vec<(u64, u32, bool)>> = vec![Vec::new(); sections.len()];
        for symbol in symtab.iter() {
            let symbol = symbol?;
            if symbol.n_type & 0x0e != N_SECT {
                continue;
            }
            let section = usize::from(symbol.n_sect)
                .checked_sub(1)
                .and_then(|i| sections.get(i).map(|s| (i, s)));
            let Some((section_index, section)) = section else {
                return Err(source.malformed(
                    0,
                    format!(
                        "symbol {} (section ordinal {} does not exist)",
                        String::from_utf8_lossy(symbol.name),
                        symbol.n_sect
                    ),
                ));
            };
            if !section.contains_address(symbol.n_value) {
                return Err(source.malformed(
                    0,
                    format!(
                        "symbol {} (address {:#x} outside section {})",
                        String::from_utf8_lossy(symbol.name),
                        symbol.n_value,
                        section.display_name()
                    ),
                ));
            }
            let offset = symbol.n_value.wrapping_sub(section.addr);
            if let Some(list) = by_section.get_mut(section_index) {
                list.push((offset, symbol.index, symbol.is_alt_entry()));
            }
        }

        let mut result = Self {
            atoms: Vec::with_capacity(sections.len()),
            symbols: Vec::new(),
            sections: Vec::with_capacity(sections.len()),
            symbol_atoms: vec![NO_ATOM; symtab.len()],
        };
        let subsections = object.subsections_via_symbols();
        let record_size = if object.file().is64() { 32 } else { 20 };
        let mut pending: Vec<(u32, u64, u32)> = Vec::new();
        for (index, (section, symbols)) in sections.iter().zip(by_section.iter_mut()).enumerate() {
            let section_index = index_u32(index, object)?;
            let first_atom = index_u32(result.atoms.len(), object)?;
            symbols.sort_by_key(|&(offset, symbol, _)| (offset, symbol));
            let data = object.section_data(index)?;
            // `__compact_unwind` carries `S_ATTR_DEBUG`: test it first.
            let kind = if section.is(b"__LD", b"__compact_unwind") {
                Some(AtomKind::CompactUnwind)
            } else if section.is_debug() {
                None
            } else {
                match section.literal_kind() {
                    LiteralKind::None => None,
                    LiteralKind::CString => Some(AtomKind::CString),
                    LiteralKind::Fixed(_) | LiteralKind::Pointers => Some(AtomKind::Literal),
                }
            };
            match kind {
                Some(AtomKind::CString) => {
                    result.split_cstrings(section, section_index, data, object)?;
                }
                Some(kind) => {
                    let entry = match (kind, section.literal_kind()) {
                        (AtomKind::CompactUnwind, _) => record_size,
                        (_, LiteralKind::Fixed(n)) => u64::from(n),
                        _ => object.word_size(),
                    };
                    result.split_fixed(section, section_index, entry, kind, object)?;
                }
                None => {
                    let kind = if section.is_debug() {
                        AtomKind::Debug
                    } else {
                        AtomKind::Regular
                    };
                    let mut start = 0u64;
                    if subsections && kind == AtomKind::Regular {
                        for &(offset, _, alt_entry) in symbols.iter() {
                            if offset == start || alt_entry {
                                continue;
                            }
                            result.push(
                                section_index,
                                start,
                                offset.saturating_sub(start),
                                section.align,
                                kind,
                            );
                            start = offset;
                        }
                    }
                    result.push(
                        section_index,
                        start,
                        section.size.saturating_sub(start),
                        section.align,
                        kind,
                    );
                }
            }
            let end_atom = index_u32(result.atoms.len(), object)?;
            result.sections.push(first_atom..end_atom);

            // Attach the symbols.
            pending.clear();
            for &(offset, symbol, _) in symbols.iter() {
                if let Some(atom) = result.find(first_atom..end_atom, offset, true) {
                    pending.push((index_u32(atom, object)?, offset, symbol));
                }
            }
            pending.sort_unstable();
            let mut cursor = 0usize;
            for atom_index in first_atom..end_atom {
                let begin = index_u32(result.symbols.len(), object)?;
                while let Some(&(atom, _, symbol)) = pending.get(cursor) {
                    if atom != atom_index {
                        break;
                    }
                    result.symbols.push(symbol);
                    if let Some(slot) = usize::try_from(symbol)
                        .ok()
                        .and_then(|s| result.symbol_atoms.get_mut(s))
                    {
                        *slot = atom_index;
                    }
                    cursor = cursor.saturating_add(1);
                }
                let end = index_u32(result.symbols.len(), object)?;
                if let Some(atom) = usize::try_from(atom_index)
                    .ok()
                    .and_then(|a| result.atoms.get_mut(a))
                {
                    atom.symbols = begin..end;
                }
            }
        }
        Ok(result)
    }

    fn push(&mut self, section: u32, offset: u64, size: u64, align: u32, kind: AtomKind) {
        self.atoms.push(Atom {
            section,
            offset,
            size,
            align: min_align(align, offset),
            kind,
            symbols: 0..0,
        });
    }

    fn split_cstrings(
        &mut self,
        section: &Section<'_>,
        section_index: u32,
        data: &[u8],
        object: &ObjectFile<'_>,
    ) -> Result<()> {
        let mut start = 0usize;
        while start < data.len() {
            let tail = data.get(start..).unwrap_or(&[]);
            let Some(len) = tail.iter().position(|&b| b == 0) else {
                return Err(object.source().malformed(
                    u64::from(section.offset).saturating_add(to_u64(start)),
                    format!(
                        "C string section {} (string is not NUL terminated)",
                        section.display_name()
                    ),
                ));
            };
            let size = len.saturating_add(1);
            self.push(
                section_index,
                to_u64(start),
                to_u64(size),
                section.align,
                AtomKind::CString,
            );
            start = start.saturating_add(size);
        }
        Ok(())
    }

    fn split_fixed(
        &mut self,
        section: &Section<'_>,
        section_index: u32,
        entry: u64,
        kind: AtomKind,
        object: &ObjectFile<'_>,
    ) -> Result<()> {
        if entry == 0 || section.size.checked_rem(entry) != Some(0) {
            return Err(object.source().malformed(
                u64::from(section.offset),
                format!(
                    "section {} (size {:#x} is not a multiple of {entry})",
                    section.display_name(),
                    section.size
                ),
            ));
        }
        let mut offset = 0u64;
        while offset < section.size {
            self.push(section_index, offset, entry, section.align, kind);
            offset = offset.saturating_add(entry);
        }
        Ok(())
    }

    /// Finds the atom in `range` containing `offset`. With `at_end`, an
    /// offset equal to the end of the last atom belongs to it.
    fn find(&self, range: Range<u32>, offset: u64, at_end: bool) -> Option<usize> {
        let start = usize::try_from(range.start).ok()?;
        let end = usize::try_from(range.end).ok()?;
        let atoms = self.atoms.get(start..end)?;
        let position = atoms.partition_point(|atom| atom.offset <= offset);
        let candidate = position.checked_sub(1)?;
        let atom = atoms.get(candidate)?;
        let atom_end = atom.offset.saturating_add(atom.size);
        let last = candidate.checked_add(1) == Some(atoms.len());
        if offset < atom_end || (at_end && last && offset == atom_end) {
            start.checked_add(candidate)
        } else {
            None
        }
    }

    /// All atoms, grouped by section in section order, each group in offset
    /// order.
    #[must_use]
    pub fn atoms(&self) -> &[Atom] {
        &self.atoms
    }

    /// The atoms of section `section` (0-based).
    #[must_use]
    pub fn section_atoms(&self, section: usize) -> &[Atom] {
        self.section_range(section)
            .and_then(|r| self.atoms.get(r))
            .unwrap_or(&[])
    }

    /// The atom index range of section `section` (0-based).
    #[must_use]
    pub fn section_range(&self, section: usize) -> Option<Range<usize>> {
        let r = self.sections.get(section)?;
        Some(usize::try_from(r.start).ok()?..usize::try_from(r.end).ok()?)
    }

    /// The symbols starting in or aliasing into atom `atom`, in address
    /// order.
    #[must_use]
    pub fn atom_symbols(&self, atom: usize) -> &[u32] {
        self.atoms
            .get(atom)
            .and_then(|a| {
                self.symbols.get(
                    usize::try_from(a.symbols.start).ok()?..usize::try_from(a.symbols.end).ok()?,
                )
            })
            .unwrap_or(&[])
    }

    /// The atom containing the definition of symbol `symbol`.
    #[must_use]
    pub fn symbol_atom(&self, symbol: u32) -> Option<usize> {
        let atom = *self.symbol_atoms.get(usize::try_from(symbol).ok()?)?;
        (atom != NO_ATOM)
            .then(|| usize::try_from(atom).ok())
            .flatten()
    }

    /// The atom of section `section` (0-based) containing `offset`.
    #[must_use]
    pub fn atom_at(&self, section: usize, offset: u64) -> Option<usize> {
        let range = self.sections.get(section)?.clone();
        self.find(range, offset, false)
    }

    /// Maps the relocations of section `section` to the atoms containing
    /// their fixups, in relocation table order.
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` for a malformed relocation, or one whose
    /// address is outside every atom of the section.
    pub fn relocations(
        &self,
        object: &ObjectFile<'_>,
        section: usize,
    ) -> Result<Vec<AtomRelocation>> {
        let table = object.relocations(section)?;
        let mut out = Vec::with_capacity(table.len());
        for relocation in table.paired(object.source()) {
            let relocation = relocation?;
            let address = u64::from(relocation.relocation.address);
            let Some(atom) = self.atom_at(section, address) else {
                return Err(object.source().malformed(
                    table
                        .file_offset()
                        .saturating_add(to_u64(relocation.index).saturating_mul(8)),
                    format!("relocation (address {address:#x} outside its section's atoms)"),
                ));
            };
            out.push(AtomRelocation { atom, relocation });
        }
        Ok(out)
    }
}
