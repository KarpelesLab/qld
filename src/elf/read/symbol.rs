//! Symbol tables (`SHT_SYMTAB`, `SHT_DYNSYM`) and their extended section
//! index tables (`SHT_SYMTAB_SHNDX`).

use super::consts::{
    SHN_ABS, SHN_COMMON, SHN_LORESERVE, SHN_UNDEF, SHN_XINDEX, STB_GLOBAL, STB_GNU_UNIQUE,
    STB_LOCAL, STB_WEAK, STT_COMMON, STT_FILE, STT_FUNC, STT_GNU_IFUNC, STT_SECTION, STT_TLS,
};
use super::format::{ElfFormat, Endian, RawRecord};
use super::source::{Source, entry_offset};
use super::strtab::StringTable;
use crate::error::Result;

/// A decoded symbol table entry, before name lookup and extended section
/// index resolution.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RawSymbol {
    /// Offset of the name in the linked string table.
    pub st_name: u32,
    /// Binding (high 4 bits) and type (low 4 bits).
    pub st_info: u8,
    /// Visibility (low 2 bits) and other flags.
    pub st_other: u8,
    /// Section index, possibly `SHN_XINDEX` or another reserved value.
    pub st_shndx: u16,
    /// Value (usually an address or section offset).
    pub st_value: u64,
    /// Size.
    pub st_size: u64,
}

impl RawSymbol {
    /// Binding (`STB_*`).
    #[inline]
    #[must_use]
    pub fn binding(&self) -> u8 {
        self.st_info >> 4
    }

    /// Type (`STT_*`).
    #[inline]
    #[must_use]
    pub fn kind(&self) -> u8 {
        self.st_info & 0xf
    }

    /// Visibility (`STV_*`).
    #[inline]
    #[must_use]
    pub fn visibility(&self) -> u8 {
        self.st_other & 0x3
    }
}

/// Where a symbol is defined, with extended indices already resolved.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SectionIndex {
    /// `SHN_UNDEF`: the symbol is undefined.
    Undefined,
    /// `SHN_ABS`: the value is absolute.
    Absolute,
    /// `SHN_COMMON`: a common symbol; the value is its alignment.
    Common,
    /// A regular section index (from `st_shndx` or `SHT_SYMTAB_SHNDX`).
    Section(u32),
    /// Another reserved index (processor- or OS-specific).
    Reserved(u16),
}

impl SectionIndex {
    /// The regular section index, if any.
    #[inline]
    #[must_use]
    pub fn section(self) -> Option<u32> {
        match self {
            Self::Section(index) => Some(index),
            _ => None,
        }
    }
}

/// A symbol with its name looked up and its section index resolved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Symbol<'a> {
    /// Name bytes, without the terminator. Not necessarily UTF-8.
    pub name: &'a [u8],
    /// `st_value`.
    pub value: u64,
    /// `st_size`.
    pub size: u64,
    /// `st_info`: binding and type.
    pub info: u8,
    /// `st_other`: visibility.
    pub other: u8,
    /// Resolved section index.
    pub section: SectionIndex,
}

impl Symbol<'_> {
    /// Binding (`STB_*`).
    #[inline]
    #[must_use]
    pub fn binding(&self) -> u8 {
        self.info >> 4
    }

    /// Type (`STT_*`).
    #[inline]
    #[must_use]
    pub fn kind(&self) -> u8 {
        self.info & 0xf
    }

    /// Visibility (`STV_*`).
    #[inline]
    #[must_use]
    pub fn visibility(&self) -> u8 {
        self.other & 0x3
    }

    /// Whether the binding is `STB_LOCAL`.
    #[inline]
    #[must_use]
    pub fn is_local(&self) -> bool {
        self.binding() == STB_LOCAL
    }

    /// Whether the binding is `STB_GLOBAL` or `STB_GNU_UNIQUE`.
    #[inline]
    #[must_use]
    pub fn is_global(&self) -> bool {
        matches!(self.binding(), STB_GLOBAL | STB_GNU_UNIQUE)
    }

    /// Whether the binding is `STB_WEAK`.
    #[inline]
    #[must_use]
    pub fn is_weak(&self) -> bool {
        self.binding() == STB_WEAK
    }

    /// Whether the symbol is undefined.
    #[inline]
    #[must_use]
    pub fn is_undefined(&self) -> bool {
        self.section == SectionIndex::Undefined
    }

    /// Whether the symbol is a common symbol (`SHN_COMMON` or `STT_COMMON`).
    #[inline]
    #[must_use]
    pub fn is_common(&self) -> bool {
        self.section == SectionIndex::Common || self.kind() == STT_COMMON
    }

    /// Whether the symbol is a function or indirect function.
    #[inline]
    #[must_use]
    pub fn is_function(&self) -> bool {
        matches!(self.kind(), STT_FUNC | STT_GNU_IFUNC)
    }

    /// Whether the symbol is thread-local.
    #[inline]
    #[must_use]
    pub fn is_tls(&self) -> bool {
        self.kind() == STT_TLS
    }

    /// Whether the symbol is a section symbol.
    #[inline]
    #[must_use]
    pub fn is_section(&self) -> bool {
        self.kind() == STT_SECTION
    }

    /// Whether the symbol is a file symbol.
    #[inline]
    #[must_use]
    pub fn is_file(&self) -> bool {
        self.kind() == STT_FILE
    }
}

/// A symbol table with its string table and extended section index table.
///
/// Entries are decoded on access; the table itself is three slices and a few
/// integers, and is `Copy`.
#[derive(Debug)]
pub struct SymbolTable<'a, F: ElfFormat> {
    raw: &'a [F::Sym],
    strtab: StringTable<'a>,
    shndx: &'a [[u8; 4]],
    first_global: usize,
    section_index: u32,
    file_offset: u64,
    source: Source<'a>,
}

impl<F: ElfFormat> Clone for SymbolTable<'_, F> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<F: ElfFormat> Copy for SymbolTable<'_, F> {}

impl<'a, F: ElfFormat> SymbolTable<'a, F> {
    /// An empty table (for files without a symbol table).
    #[must_use]
    pub fn empty(source: Source<'a>) -> Self {
        Self {
            raw: &[],
            strtab: StringTable::default(),
            shndx: &[],
            first_global: 0,
            section_index: 0,
            file_offset: 0,
            source,
        }
    }

    /// Assembles a table from its parts.
    ///
    /// `first_global` is the symbol section's `sh_info`; it is clamped to the
    /// number of symbols. `shndx` is the contents of the
    /// `SHT_SYMTAB_SHNDX` section, or empty.
    #[must_use]
    pub fn new(
        raw: &'a [F::Sym],
        strtab: StringTable<'a>,
        shndx: &'a [[u8; 4]],
        first_global: u32,
        section_index: u32,
        file_offset: u64,
        source: Source<'a>,
    ) -> Self {
        let first_global = usize::try_from(first_global)
            .unwrap_or(usize::MAX)
            .min(raw.len());
        Self {
            raw,
            strtab,
            shndx,
            first_global,
            section_index,
            file_offset,
            source,
        }
    }

    /// Number of symbols, including the null symbol 0.
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.raw.len()
    }

    /// Whether the table has no entries.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.raw.is_empty()
    }

    /// Index of the first non-local symbol (`sh_info`).
    #[inline]
    #[must_use]
    pub fn first_global(&self) -> usize {
        self.first_global
    }

    /// Section index of the symbol table (0 if there is none).
    #[inline]
    #[must_use]
    pub fn section_index(&self) -> u32 {
        self.section_index
    }

    /// The linked string table.
    #[inline]
    #[must_use]
    pub fn strtab(&self) -> StringTable<'a> {
        self.strtab
    }

    /// The raw records.
    #[inline]
    #[must_use]
    pub fn raw(&self) -> &'a [F::Sym] {
        self.raw
    }

    /// Decodes symbol `index` without looking up its name.
    #[inline]
    #[must_use]
    pub fn get_raw(&self, index: usize) -> Option<RawSymbol> {
        self.raw.get(index).map(F::decode_sym)
    }

    /// Decodes symbol `index`, looking up its name and section.
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` if `index` is out of range, the name is not
    /// in the string table, or an extended section index is missing.
    #[inline]
    pub fn get(&self, index: usize) -> Result<Symbol<'a>> {
        let raw = self.get_raw(index).ok_or_else(|| {
            self.source.malformed(
                self.file_offset,
                format!("symbol index {index} (out of range)"),
            )
        })?;
        self.resolve(index, &raw)
    }

    /// Looks up the name of a raw symbol read from entry `index`.
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` if the name offset is invalid.
    #[inline]
    pub fn name(&self, index: usize, raw: &RawSymbol) -> Result<&'a [u8]> {
        self.strtab
            .get(raw.st_name)
            .ok_or_else(|| self.error(index, "symbol name offset"))
    }

    /// Resolves the section index of a raw symbol read from entry `index`.
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` if the symbol uses `SHN_XINDEX` and the
    /// extended index table has no entry for it.
    #[inline]
    pub fn section(&self, index: usize, raw: &RawSymbol) -> Result<SectionIndex> {
        Ok(match raw.st_shndx {
            SHN_UNDEF => SectionIndex::Undefined,
            SHN_ABS => SectionIndex::Absolute,
            SHN_COMMON => SectionIndex::Common,
            SHN_XINDEX => {
                let entry = self
                    .shndx
                    .get(index)
                    .ok_or_else(|| self.error(index, "extended section index (missing)"))?;
                SectionIndex::Section(F::Endian::u32(*entry))
            }
            n if n >= SHN_LORESERVE => SectionIndex::Reserved(n),
            n => SectionIndex::Section(u32::from(n)),
        })
    }

    /// [`section`](Self::section) without the error: `None` if the symbol
    /// uses `SHN_XINDEX` and the extended index table has no entry for it.
    ///
    /// For hot paths (relocation targets): building the error `Result` there
    /// cost more than the lookup itself.
    #[inline]
    #[must_use]
    pub fn section_of(&self, index: usize, raw: &RawSymbol) -> Option<SectionIndex> {
        Some(match raw.st_shndx {
            SHN_UNDEF => SectionIndex::Undefined,
            SHN_ABS => SectionIndex::Absolute,
            SHN_COMMON => SectionIndex::Common,
            SHN_XINDEX => SectionIndex::Section(F::Endian::u32(*self.shndx.get(index)?)),
            n if n >= SHN_LORESERVE => SectionIndex::Reserved(n),
            n => SectionIndex::Section(u32::from(n)),
        })
    }

    /// Completes a raw symbol read from entry `index`.
    ///
    /// # Errors
    ///
    /// See [`name`](Self::name) and [`section`](Self::section).
    #[inline]
    pub fn resolve(&self, index: usize, raw: &RawSymbol) -> Result<Symbol<'a>> {
        Ok(Symbol {
            name: self.name(index, raw)?,
            value: raw.st_value,
            size: raw.st_size,
            info: raw.st_info,
            other: raw.st_other,
            section: self.section(index, raw)?,
        })
    }

    /// Iterates over all symbols, including the null symbol 0.
    #[must_use]
    pub fn iter(&self) -> SymbolIter<'a, F> {
        SymbolIter {
            table: *self,
            index: 0,
        }
    }

    /// Iterates over the non-local symbols (from `first_global` on).
    #[must_use]
    pub fn globals(&self) -> SymbolIter<'a, F> {
        SymbolIter {
            table: *self,
            index: self.first_global,
        }
    }

    /// Iterates over raw symbols, without name lookups.
    pub fn iter_raw(&self) -> impl ExactSizeIterator<Item = RawSymbol> + use<'a, F> {
        self.raw.iter().map(F::decode_sym)
    }

    #[cold]
    fn error(&self, index: usize, what: &str) -> crate::Error {
        self.source.malformed(
            entry_offset(self.file_offset, index, F::Sym::SIZE),
            format!("{what} (symbol {index})"),
        )
    }
}

/// Iterator over [`Symbol`]s, yielding an error for entries that cannot be
/// resolved (and continuing after them).
#[derive(Debug)]
pub struct SymbolIter<'a, F: ElfFormat> {
    table: SymbolTable<'a, F>,
    index: usize,
}

impl<F: ElfFormat> Clone for SymbolIter<'_, F> {
    fn clone(&self) -> Self {
        Self {
            table: self.table,
            index: self.index,
        }
    }
}

impl<'a, F: ElfFormat> SymbolIter<'a, F> {
    /// Index of the symbol the next call to `next` will yield.
    #[must_use]
    pub fn index(&self) -> usize {
        self.index
    }
}

impl<'a, F: ElfFormat> Iterator for SymbolIter<'a, F> {
    type Item = Result<Symbol<'a>>;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        let index = self.index;
        let raw = F::decode_sym(self.table.raw.get(index)?);
        self.index = index.saturating_add(1);
        Some(self.table.resolve(index, &raw))
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = self.table.raw.len().saturating_sub(self.index);
        (n, Some(n))
    }
}

impl<F: ElfFormat> ExactSizeIterator for SymbolIter<'_, F> {}
