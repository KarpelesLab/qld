//! Symbol tables (`nlist` and `nlist_64`).

use super::bytes::{Endian, Source, cstr, to_u64};
use super::consts::{
    N_ABS, N_ALT_ENTRY, N_COLD_FUNC, N_EXT, N_INDR, N_NO_DEAD_STRIP, N_PBUD, N_PEXT, N_SECT,
    N_STAB, N_SYMBOL_RESOLVER, N_TYPE, N_UNDF, N_WEAK_DEF, N_WEAK_REF, REFERENCED_DYNAMICALLY,
};
use crate::error::Result;

/// The `N_TYPE` of a non-STABS symbol.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SymbolKind {
    /// `N_UNDF`: undefined, or common when external with a non-zero value.
    Undefined,
    /// `N_ABS`: absolute value.
    Absolute,
    /// `N_SECT`: defined in section `n_sect` (1-based).
    Section,
    /// `N_PBUD`: prebound undefined.
    PreboundUndefined,
    /// `N_INDR`: alias of another symbol, named by [`Symbol::indirect_name`].
    Indirect,
    /// A value not defined by the ABI.
    Unknown(u8),
}

/// A decoded symbol table entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Symbol<'a> {
    /// Index in the symbol table.
    pub index: u32,
    /// Name (empty when `n_strx` is 0).
    pub name: &'a [u8],
    /// `n_strx`.
    pub n_strx: u32,
    /// `n_type`.
    pub n_type: u8,
    /// `n_sect`: 1-based section ordinal, or `NO_SECT`.
    pub n_sect: u8,
    /// `n_desc`.
    pub n_desc: u16,
    /// `n_value`.
    pub n_value: u64,
}

impl Symbol<'_> {
    /// Whether this is a STABS debugging entry.
    #[inline]
    #[must_use]
    pub fn is_stab(&self) -> bool {
        self.n_type & N_STAB != 0
    }

    /// The symbol kind (meaningless for STABS entries).
    #[inline]
    #[must_use]
    pub fn kind(&self) -> SymbolKind {
        match self.n_type & N_TYPE {
            N_UNDF => SymbolKind::Undefined,
            N_ABS => SymbolKind::Absolute,
            N_SECT => SymbolKind::Section,
            N_PBUD => SymbolKind::PreboundUndefined,
            N_INDR => SymbolKind::Indirect,
            other => SymbolKind::Unknown(other),
        }
    }

    /// `N_EXT`: visible outside the object.
    #[inline]
    #[must_use]
    pub fn is_external(&self) -> bool {
        !self.is_stab() && self.n_type & N_EXT != 0
    }

    /// `N_PEXT`: private external (hidden after the link).
    #[inline]
    #[must_use]
    pub fn is_private_external(&self) -> bool {
        !self.is_stab() && self.n_type & N_PEXT != 0
    }

    /// Whether the symbol is defined here: in a section, absolute, or an
    /// indirect alias.
    #[inline]
    #[must_use]
    pub fn is_defined(&self) -> bool {
        !self.is_stab()
            && matches!(
                self.kind(),
                SymbolKind::Section | SymbolKind::Absolute | SymbolKind::Indirect
            )
    }

    /// Whether this is an undefined reference (not a common symbol).
    #[inline]
    #[must_use]
    pub fn is_undefined(&self) -> bool {
        !self.is_stab() && self.kind() == SymbolKind::Undefined && !self.is_common()
    }

    /// Whether this is a common symbol: external, `N_UNDF`, non-zero value
    /// (the size).
    #[inline]
    #[must_use]
    pub fn is_common(&self) -> bool {
        !self.is_stab()
            && self.n_type & N_EXT != 0
            && self.kind() == SymbolKind::Undefined
            && self.n_value != 0
    }

    /// Alignment of a common symbol, as a power of two
    /// (`GET_COMM_ALIGN`). Zero means unspecified.
    #[inline]
    #[must_use]
    pub fn common_align(&self) -> u8 {
        ((self.n_desc >> 8) & 0x0f) as u8
    }

    /// Two-level namespace library ordinal of an undefined symbol
    /// (`GET_LIBRARY_ORDINAL`).
    #[inline]
    #[must_use]
    pub fn library_ordinal(&self) -> u8 {
        (self.n_desc >> 8) as u8
    }

    /// `N_WEAK_DEF`: weak definition. For undefined symbols the same bit is
    /// `N_REF_TO_WEAK`.
    #[inline]
    #[must_use]
    pub fn is_weak_def(&self) -> bool {
        !self.is_stab() && self.n_desc & N_WEAK_DEF != 0
    }

    /// `N_WEAK_REF`: weak reference.
    #[inline]
    #[must_use]
    pub fn is_weak_ref(&self) -> bool {
        !self.is_stab() && self.n_desc & N_WEAK_REF != 0
    }

    /// `N_NO_DEAD_STRIP`.
    #[inline]
    #[must_use]
    pub fn is_no_dead_strip(&self) -> bool {
        !self.is_stab() && self.n_desc & N_NO_DEAD_STRIP != 0
    }

    /// `N_ALT_ENTRY`: does not start an atom.
    #[inline]
    #[must_use]
    pub fn is_alt_entry(&self) -> bool {
        !self.is_stab() && self.n_desc & N_ALT_ENTRY != 0
    }

    /// `N_COLD_FUNC`.
    #[inline]
    #[must_use]
    pub fn is_cold_func(&self) -> bool {
        !self.is_stab() && self.n_desc & N_COLD_FUNC != 0
    }

    /// `REFERENCED_DYNAMICALLY`.
    #[inline]
    #[must_use]
    pub fn is_referenced_dynamically(&self) -> bool {
        !self.is_stab() && self.n_desc & REFERENCED_DYNAMICALLY != 0
    }

    /// `N_SYMBOL_RESOLVER`.
    #[inline]
    #[must_use]
    pub fn is_symbol_resolver(&self) -> bool {
        !self.is_stab() && self.n_desc & N_SYMBOL_RESOLVER != 0
    }

    /// The STABS type (`n_type` itself), for STABS entries.
    #[inline]
    #[must_use]
    pub fn stab_type(&self) -> Option<u8> {
        self.is_stab().then_some(self.n_type)
    }
}

/// A symbol table: the `nlist` records and the string table.
#[derive(Clone, Copy, Debug)]
pub struct SymbolTable<'a> {
    records: &'a [u8],
    strtab: &'a [u8],
    file_offset: u64,
    endian: Endian,
    is64: bool,
    source: Source<'a>,
}

impl<'a> SymbolTable<'a> {
    /// An empty table.
    #[must_use]
    pub fn empty(endian: Endian, is64: bool, source: Source<'a>) -> Self {
        Self {
            records: &[],
            strtab: &[],
            file_offset: 0,
            endian,
            is64,
            source,
        }
    }

    /// Wraps `nsyms` records and a string table.
    pub(crate) fn new(
        records: &'a [u8],
        strtab: &'a [u8],
        file_offset: u64,
        endian: Endian,
        is64: bool,
        source: Source<'a>,
    ) -> Self {
        Self {
            records,
            strtab,
            file_offset,
            endian,
            is64,
            source,
        }
    }

    /// Size of one `nlist` record: 12, or 16 for 64-bit.
    #[must_use]
    pub fn entry_size(&self) -> usize {
        if self.is64 { 16 } else { 12 }
    }

    /// Number of symbols, including STABS entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.records
            .len()
            .checked_div(self.entry_size())
            .unwrap_or(0)
    }

    /// Whether the table has no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The string table.
    #[must_use]
    pub fn strtab(&self) -> &'a [u8] {
        self.strtab
    }

    /// Looks up a string by offset.
    #[must_use]
    pub fn string(&self, offset: u32) -> Option<&'a [u8]> {
        cstr(self.strtab.get(usize::try_from(offset).ok()?..)?)
    }

    /// Decodes symbol `index`.
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` if `index` is out of range or the name is
    /// not in the string table.
    #[inline]
    pub fn get(&self, index: u32) -> Result<Symbol<'a>> {
        let size = self.entry_size();
        let record = usize::try_from(index)
            .ok()
            .and_then(|i| i.checked_mul(size))
            .and_then(|start| self.records.get(start..)?.get(..size))
            .ok_or_else(|| {
                self.source.malformed(
                    self.file_offset,
                    format!("symbol index {index} (out of range)"),
                )
            })?;
        let e = self.endian;
        let n_strx = e.u32(record, 0).unwrap_or(0);
        let n_value = if self.is64 {
            e.u64(record, 8).unwrap_or(0)
        } else {
            u64::from(e.u32(record, 8).unwrap_or(0))
        };
        let name = if n_strx == 0 {
            &[][..]
        } else {
            self.string(n_strx).ok_or_else(|| {
                self.source.malformed(
                    self.file_offset
                        .saturating_add(u64::from(index).saturating_mul(to_u64(size))),
                    format!("symbol name offset {n_strx:#x} (symbol {index})"),
                )
            })?
        };
        Ok(Symbol {
            index,
            name,
            n_strx,
            n_type: *record.get(4).unwrap_or(&0),
            n_sect: *record.get(5).unwrap_or(&0),
            n_desc: e.u16(record, 6).unwrap_or(0),
            n_value,
        })
    }

    /// The name of an `N_INDR` symbol's target.
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` if the string offset is invalid or the
    /// symbol is not indirect.
    pub fn indirect_name(&self, symbol: &Symbol<'_>) -> Result<&'a [u8]> {
        if symbol.is_stab() || symbol.kind() != SymbolKind::Indirect {
            return Err(self.source.malformed(
                self.file_offset,
                format!("symbol {} (not an indirect symbol)", symbol.index),
            ));
        }
        u32::try_from(symbol.n_value)
            .ok()
            .and_then(|offset| self.string(offset))
            .ok_or_else(|| {
                self.source.malformed(
                    self.file_offset,
                    format!("indirect symbol name (symbol {})", symbol.index),
                )
            })
    }

    /// Iterates over all entries, STABS included.
    #[must_use]
    pub fn iter_all(&self) -> SymbolIter<'a> {
        SymbolIter {
            table: *self,
            index: 0,
            skip_stabs: false,
        }
    }

    /// Iterates over the entries that are not STABS: the default view.
    #[must_use]
    pub fn iter(&self) -> SymbolIter<'a> {
        SymbolIter {
            table: *self,
            index: 0,
            skip_stabs: true,
        }
    }
}

/// Iterator over a [`SymbolTable`]. Yields an error for entries whose name
/// cannot be read, and continues after them.
#[derive(Clone, Debug)]
pub struct SymbolIter<'a> {
    table: SymbolTable<'a>,
    index: u32,
    skip_stabs: bool,
}

impl<'a> Iterator for SymbolIter<'a> {
    type Item = Result<Symbol<'a>>;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let index = self.index;
            if usize::try_from(index).ok()? >= self.table.len() {
                return None;
            }
            self.index = index.checked_add(1)?;
            if self.skip_stabs {
                let n_type = usize::try_from(index)
                    .ok()
                    .and_then(|i| i.checked_mul(self.table.entry_size()))
                    .and_then(|at| at.checked_add(4))
                    .and_then(|at| self.table.records.get(at))
                    .copied()
                    .unwrap_or(0);
                if n_type & N_STAB != 0 {
                    continue;
                }
            }
            return Some(self.table.get(index));
        }
    }
}

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn decodes_and_skips_stabs() {
        let src = Source::new(Path::new("t.o"));
        let strtab = b"\0_a\0_b\0/tmp/x.o\0";
        let mut records = Vec::new();
        let mut push = |strx: u32, n_type: u8, sect: u8, desc: u16, value: u64| {
            records.extend_from_slice(&strx.to_le_bytes());
            records.push(n_type);
            records.push(sect);
            records.extend_from_slice(&desc.to_le_bytes());
            records.extend_from_slice(&value.to_le_bytes());
        };
        push(7, 0x66, 3, 1, 0); // N_OSO
        push(1, N_SECT | N_EXT, 1, N_WEAK_DEF | N_ALT_ENTRY, 0x10);
        push(4, N_UNDF | N_EXT, 0, 3 << 8, 8); // common, align 2^3
        push(100, N_UNDF, 0, 0, 0); // bad name
        let table = SymbolTable::new(&records, strtab, 0, Endian::LITTLE, true, src);
        assert_eq!(table.len(), 4);
        let all: Vec<_> = table.iter_all().collect();
        assert_eq!(all.len(), 4);
        assert!(all[0].as_ref().unwrap().is_stab());
        assert!(all[3].is_err());
        let syms: Vec<_> = table.iter().filter_map(Result::ok).collect();
        assert_eq!(syms.len(), 2);
        assert_eq!(syms[0].name, b"_a");
        assert!(syms[0].is_weak_def() && syms[0].is_alt_entry() && syms[0].is_defined());
        assert!(syms[1].is_common() && !syms[1].is_undefined());
        assert_eq!(syms[1].common_align(), 3);
        assert!(table.get(4).is_err());
    }
}
