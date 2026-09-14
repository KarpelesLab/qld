//! The COFF symbol table: 18-byte (`IMAGE_SYMBOL`) or 20-byte `/bigobj`
//! (`IMAGE_SYMBOL_EX`) records, with their auxiliary records.

use super::consts::{
    IMAGE_COMDAT_SELECT_ASSOCIATIVE, IMAGE_SYM_ABSOLUTE, IMAGE_SYM_CLASS_EXTERNAL,
    IMAGE_SYM_CLASS_FILE, IMAGE_SYM_CLASS_FUNCTION, IMAGE_SYM_CLASS_LABEL, IMAGE_SYM_CLASS_SECTION,
    IMAGE_SYM_CLASS_STATIC, IMAGE_SYM_CLASS_WEAK_EXTERNAL, IMAGE_SYM_DEBUG,
    IMAGE_SYM_DTYPE_FUNCTION, IMAGE_SYM_TYPE_NULL, IMAGE_SYM_UNDEFINED, MAX_NUMBER_OF_SECTIONS_16,
};
use super::header::{BIGOBJ_SYMBOL_SIZE, SYMBOL_SIZE};
use super::source::{Source, array, entry_offset, subslice, u8_at, u16_at, u32_at, until_nul};
use super::strtab::StringTable;
use crate::error::Result;

/// What a symbol's section number denotes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SectionNumber {
    /// `IMAGE_SYM_UNDEFINED` (0): undefined, or common when the value is
    /// nonzero.
    Undefined,
    /// `IMAGE_SYM_ABSOLUTE` (-1): the value is an absolute number.
    Absolute,
    /// `IMAGE_SYM_DEBUG` (-2): debugging information (`.file` records).
    Debug,
    /// A 1-based section number.
    Section(u32),
    /// Another negative value, which no tool produces.
    Reserved(i32),
}

impl SectionNumber {
    /// Classifies a raw signed section number.
    #[must_use]
    pub fn from_raw(number: i32) -> Self {
        match number {
            IMAGE_SYM_UNDEFINED => Self::Undefined,
            IMAGE_SYM_ABSOLUTE => Self::Absolute,
            IMAGE_SYM_DEBUG => Self::Debug,
            n => match u32::try_from(n) {
                Ok(section) => Self::Section(section),
                Err(_) => Self::Reserved(n),
            },
        }
    }

    /// The section number, when the symbol is defined in a section.
    #[must_use]
    pub fn section(self) -> Option<u32> {
        match self {
            Self::Section(n) => Some(n),
            _ => None,
        }
    }
}

/// A decoded symbol record, with its name resolved and its auxiliary
/// records attached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Symbol<'a> {
    /// Index of the record in the symbol table (relocations refer to it).
    pub index: u32,
    /// The name: inline (NUL padding removed) or from the string table.
    pub name: &'a [u8],
    /// `Value`: section offset, absolute value, or common size.
    pub value: u32,
    /// `SectionNumber`, sign-extended (see [`SectionNumber`]).
    pub section_number: i32,
    /// `Type`: base type in the low 4 bits, complex type in the next 4.
    pub symbol_type: u16,
    /// `StorageClass` (`IMAGE_SYM_CLASS_*`).
    pub storage_class: u8,
    /// `NumberOfAuxSymbols`.
    pub number_of_aux_symbols: u8,
    /// The auxiliary records (`number_of_aux_symbols` records of 18 or 20
    /// bytes).
    pub aux: &'a [u8],
    /// Whether the records are 20-byte `/bigobj` records.
    pub bigobj: bool,
}

impl<'a> Symbol<'a> {
    /// The section number, classified.
    #[inline]
    #[must_use]
    pub fn section(&self) -> SectionNumber {
        SectionNumber::from_raw(self.section_number)
    }

    /// The base type (`IMAGE_SYM_TYPE_*`).
    #[inline]
    #[must_use]
    pub fn base_type(&self) -> u16 {
        self.symbol_type & 0xf
    }

    /// The complex type (`IMAGE_SYM_DTYPE_*`).
    #[inline]
    #[must_use]
    pub fn complex_type(&self) -> u16 {
        (self.symbol_type >> 4) & 0xf
    }

    /// Storage class `EXTERNAL`.
    #[inline]
    #[must_use]
    pub fn is_external(&self) -> bool {
        self.storage_class == IMAGE_SYM_CLASS_EXTERNAL
    }

    /// An external symbol defined in a section or absolute.
    #[inline]
    #[must_use]
    pub fn is_defined_external(&self) -> bool {
        self.is_external() && self.section_number != IMAGE_SYM_UNDEFINED
    }

    /// An undefined external: `EXTERNAL`, section 0, value 0.
    #[inline]
    #[must_use]
    pub fn is_undefined(&self) -> bool {
        self.is_external() && self.section_number == IMAGE_SYM_UNDEFINED && self.value == 0
    }

    /// A common symbol: `EXTERNAL`, section 0, and the size as a nonzero
    /// value.
    #[inline]
    #[must_use]
    pub fn is_common(&self) -> bool {
        self.is_external() && self.section_number == IMAGE_SYM_UNDEFINED && self.value != 0
    }

    /// Storage class `WEAK_EXTERNAL`.
    #[inline]
    #[must_use]
    pub fn is_weak_external(&self) -> bool {
        self.storage_class == IMAGE_SYM_CLASS_WEAK_EXTERNAL
    }

    /// Section number `IMAGE_SYM_ABSOLUTE`.
    #[inline]
    #[must_use]
    pub fn is_absolute(&self) -> bool {
        self.section_number == IMAGE_SYM_ABSOLUTE
    }

    /// Section number `IMAGE_SYM_DEBUG`.
    #[inline]
    #[must_use]
    pub fn is_debug(&self) -> bool {
        self.section_number == IMAGE_SYM_DEBUG
    }

    /// Storage class `FILE`: the source file name is in the aux records.
    #[inline]
    #[must_use]
    pub fn is_file(&self) -> bool {
        self.storage_class == IMAGE_SYM_CLASS_FILE
    }

    /// Storage class `SECTION`.
    #[inline]
    #[must_use]
    pub fn is_section_class(&self) -> bool {
        self.storage_class == IMAGE_SYM_CLASS_SECTION
    }

    /// Storage class `LABEL`.
    #[inline]
    #[must_use]
    pub fn is_label(&self) -> bool {
        self.storage_class == IMAGE_SYM_CLASS_LABEL
    }

    /// Storage class `FUNCTION` (`.bf`, `.ef`, `.lf` line-number markers).
    #[inline]
    #[must_use]
    pub fn is_function_line_info(&self) -> bool {
        self.storage_class == IMAGE_SYM_CLASS_FUNCTION
    }

    /// A symbol followed by a section definition aux record: `STATIC` with
    /// aux records, or an `EXTERNAL` absolute symbol with aux records (C++/CLI
    /// appdomain globals). This is LLVM's `isSectionDefinition`.
    ///
    /// GCC also gives static function symbols an (all-zero) aux record; it
    /// reads as a section definition with length 0, as `llvm-readobj`
    /// shows it.
    #[inline]
    #[must_use]
    pub fn is_section_definition(&self) -> bool {
        self.number_of_aux_symbols != 0
            && (self.storage_class == IMAGE_SYM_CLASS_STATIC
                || (self.is_external() && self.is_absolute()))
    }

    /// An external function definition: `EXTERNAL`, complex type function,
    /// defined in a section. A function definition aux record may follow.
    #[inline]
    #[must_use]
    pub fn is_function_definition(&self) -> bool {
        self.is_external()
            && self.base_type() == IMAGE_SYM_TYPE_NULL
            && self.complex_type() == IMAGE_SYM_DTYPE_FUNCTION
            && self.section_number > 0
    }

    /// A section symbol, such as `.text`: `STATIC`, value 0, defined in a
    /// section, not a function, with a section definition aux record. COMDAT
    /// sections carry their selection in this record.
    #[inline]
    #[must_use]
    pub fn is_section_symbol(&self) -> bool {
        self.storage_class == IMAGE_SYM_CLASS_STATIC
            && self.value == 0
            && self.number_of_aux_symbols != 0
            && self.section_number > 0
            && self.complex_type() != IMAGE_SYM_DTYPE_FUNCTION
    }

    /// Size of one aux record (18, or 20 for `/bigobj`).
    #[inline]
    #[must_use]
    pub fn aux_record_size(&self) -> usize {
        if self.bigobj {
            BIGOBJ_SYMBOL_SIZE
        } else {
            SYMBOL_SIZE
        }
    }

    /// Aux record `n` (0-based), if present.
    #[must_use]
    pub fn aux_record(&self, n: u8) -> Option<&'a [u8]> {
        let size = self.aux_record_size();
        let start = usize::from(n).checked_mul(size)?;
        let end = start.checked_add(size)?;
        self.aux.get(start..end)
    }

    /// The section definition aux record, for symbols where
    /// [`is_section_definition`](Self::is_section_definition) holds.
    #[must_use]
    pub fn section_definition(&self) -> Option<AuxSectionDefinition> {
        if !self.is_section_definition() {
            return None;
        }
        AuxSectionDefinition::decode(self.aux_record(0)?, self.bigobj)
    }

    /// The weak external aux record, for `WEAK_EXTERNAL` symbols.
    #[must_use]
    pub fn weak_external(&self) -> Option<AuxWeakExternal> {
        if !self.is_weak_external() {
            return None;
        }
        AuxWeakExternal::decode(self.aux_record(0)?)
    }

    /// The function definition aux record, for symbols where
    /// [`is_function_definition`](Self::is_function_definition) holds.
    #[must_use]
    pub fn function_definition(&self) -> Option<AuxFunctionDefinition> {
        if !self.is_function_definition() {
            return None;
        }
        AuxFunctionDefinition::decode(self.aux_record(0)?)
    }

    /// The source file name of a `FILE` symbol: the aux records joined, with
    /// trailing NUL padding removed.
    #[must_use]
    pub fn file_name(&self) -> Option<&'a [u8]> {
        if !self.is_file() {
            return None;
        }
        Some(until_nul(self.aux))
    }
}

/// A section definition aux record (`IMAGE_AUX_SYMBOL.Section`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct AuxSectionDefinition {
    /// `Length`: size of the section contents.
    pub length: u32,
    /// `NumberOfRelocations` (16 bits).
    pub number_of_relocations: u16,
    /// `NumberOfLinenumbers`.
    pub number_of_linenumbers: u16,
    /// `CheckSum`, used by `IMAGE_COMDAT_SELECT_EXACT_MATCH`.
    pub check_sum: u32,
    /// `Number`: for an associative COMDAT section, the section it is
    /// associated with (`HighNumber` included for `/bigobj`).
    pub number: u32,
    /// `Selection` (`IMAGE_COMDAT_SELECT_*`), for COMDAT sections.
    pub selection: u8,
}

impl AuxSectionDefinition {
    /// Decodes an aux record.
    #[must_use]
    pub fn decode(aux: &[u8], bigobj: bool) -> Option<Self> {
        let low = u32::from(u16_at(aux, 12)?);
        let number = if bigobj {
            low | (u32::from(u16_at(aux, 16)?) << 16)
        } else {
            low
        };
        Some(Self {
            length: u32_at(aux, 0)?,
            number_of_relocations: u16_at(aux, 4)?,
            number_of_linenumbers: u16_at(aux, 6)?,
            check_sum: u32_at(aux, 8)?,
            number,
            selection: u8_at(aux, 14)?,
        })
    }

    /// For `IMAGE_COMDAT_SELECT_ASSOCIATIVE`, the associated section number.
    #[must_use]
    pub fn associative_section(&self) -> Option<u32> {
        (self.selection == IMAGE_COMDAT_SELECT_ASSOCIATIVE).then_some(self.number)
    }
}

/// A weak external aux record (`IMAGE_AUX_SYMBOL.Sym` for weak externals).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct AuxWeakExternal {
    /// `TagIndex`: symbol table index of the default definition.
    pub tag_index: u32,
    /// `Characteristics`: the search type
    /// (`IMAGE_WEAK_EXTERN_SEARCH_*`, `IMAGE_WEAK_EXTERN_ANTI_DEPENDENCY`).
    pub characteristics: u32,
}

impl AuxWeakExternal {
    /// Decodes an aux record.
    #[must_use]
    pub fn decode(aux: &[u8]) -> Option<Self> {
        Some(Self {
            tag_index: u32_at(aux, 0)?,
            characteristics: u32_at(aux, 4)?,
        })
    }
}

/// A function definition aux record.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct AuxFunctionDefinition {
    /// `TagIndex`: index of the `.bf` record.
    pub tag_index: u32,
    /// `TotalSize`: size of the function's code.
    pub total_size: u32,
    /// `PointerToLinenumber`: file offset of its line numbers.
    pub pointer_to_linenumber: u32,
    /// `PointerToNextFunction`: index of the next function's symbol.
    pub pointer_to_next_function: u32,
}

impl AuxFunctionDefinition {
    /// Decodes an aux record.
    #[must_use]
    pub fn decode(aux: &[u8]) -> Option<Self> {
        Some(Self {
            tag_index: u32_at(aux, 0)?,
            total_size: u32_at(aux, 4)?,
            pointer_to_linenumber: u32_at(aux, 8)?,
            pointer_to_next_function: u32_at(aux, 12)?,
        })
    }
}

/// The symbol table and the string table that follows it.
///
/// Records are decoded on access; the table is a slice and a few integers,
/// and is `Copy`.
#[derive(Clone, Copy, Debug)]
pub struct SymbolTable<'a> {
    data: &'a [u8],
    count: u32,
    bigobj: bool,
    strings: StringTable<'a>,
    file_offset: u64,
    source: Source<'a>,
}

impl<'a> SymbolTable<'a> {
    /// An empty table, for files without symbols.
    #[must_use]
    pub fn empty(source: Source<'a>) -> Self {
        Self {
            data: &[],
            count: 0,
            bigobj: false,
            strings: StringTable::default(),
            file_offset: 0,
            source,
        }
    }

    /// Locates the symbol table at `pointer` with `count` records, and the
    /// string table after it, in `file`.
    ///
    /// A missing string table (the file ends right after the symbols) is
    /// treated as empty.
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` if the symbol table or the declared string
    /// table extends past the end of the file.
    pub fn parse(
        file: &'a [u8],
        pointer: u32,
        count: u32,
        bigobj: bool,
        source: Source<'a>,
    ) -> Result<Self> {
        if pointer == 0 {
            if count != 0 {
                return Err(source.malformed(8, "symbol table (count without a table)"));
            }
            return Ok(Self {
                bigobj,
                ..Self::empty(source)
            });
        }
        let record = if bigobj {
            BIGOBJ_SYMBOL_SIZE
        } else {
            SYMBOL_SIZE
        };
        let offset = u64::from(pointer);
        let size = u64::from(count).saturating_mul(super::source::to_u64(record));
        let data = subslice(file, offset, size)
            .ok_or_else(|| source.malformed(offset, "symbol table (out of bounds)"))?;
        let strings_offset = offset.saturating_add(size);
        let strings = match subslice(file, strings_offset, 4) {
            None => StringTable::new(&[], strings_offset),
            Some(size_field) => {
                let declared = array::<4>(size_field, 0).map_or(0, u32::from_le_bytes);
                // Some tools write 0 for an empty table.
                let declared = declared.max(4);
                let table =
                    subslice(file, strings_offset, u64::from(declared)).ok_or_else(|| {
                        source.malformed(strings_offset, "string table size (out of bounds)")
                    })?;
                StringTable::new(table, strings_offset)
            }
        };
        Ok(Self {
            data,
            count,
            bigobj,
            strings,
            file_offset: offset,
            source,
        })
    }

    /// Number of records, including aux records.
    #[inline]
    #[must_use]
    pub fn len(&self) -> u32 {
        self.count
    }

    /// Whether the table has no records.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Whether the records are 20-byte `/bigobj` records.
    #[inline]
    #[must_use]
    pub fn is_bigobj(&self) -> bool {
        self.bigobj
    }

    /// Size of one record.
    #[inline]
    #[must_use]
    pub fn record_size(&self) -> usize {
        if self.bigobj {
            BIGOBJ_SYMBOL_SIZE
        } else {
            SYMBOL_SIZE
        }
    }

    /// The string table.
    #[inline]
    #[must_use]
    pub fn strings(&self) -> StringTable<'a> {
        self.strings
    }

    /// File offset of the symbol table.
    #[inline]
    #[must_use]
    pub fn file_offset(&self) -> u64 {
        self.file_offset
    }

    /// The raw record bytes.
    #[inline]
    #[must_use]
    pub fn data(&self) -> &'a [u8] {
        self.data
    }

    /// Decodes record `index` as a symbol, attaching its aux records.
    ///
    /// Relocations and aux records refer to symbols by record index; the
    /// caller is responsible for not pointing into another symbol's aux
    /// records (which decode as garbage, not as an error).
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` if `index` is out of range, the name
    /// offset is invalid, or the aux records run past the table.
    pub fn get(&self, index: u32) -> Result<Symbol<'a>> {
        let size = self.record_size();
        let start = usize::try_from(index)
            .ok()
            .and_then(|i| i.checked_mul(size))
            .filter(|_| index < self.count)
            .ok_or_else(|| {
                self.source.malformed(
                    self.file_offset,
                    format!("symbol index {index} (out of range)"),
                )
            })?;
        let record = start
            .checked_add(size)
            .and_then(|end| self.data.get(start..end))
            .ok_or_else(|| self.error(index, "symbol record"))?;
        let name_field: &'a [u8; 8] = record
            .first_chunk::<8>()
            .ok_or_else(|| self.error(index, "symbol record"))?;
        let name = if name_field.starts_with(&[0, 0, 0, 0]) {
            let offset = u32_at(name_field, 4).unwrap_or(0);
            self.strings
                .get(offset)
                .ok_or_else(|| self.error(index, "symbol name offset"))?
        } else {
            until_nul(name_field)
        };
        let value = u32_at(record, 8).unwrap_or(0);
        let (section_number, rest) = if self.bigobj {
            (u32_at(record, 12).map_or(0, |n| n.cast_signed()), 16)
        } else {
            let raw = u16_at(record, 12).unwrap_or(0);
            let number = if raw <= MAX_NUMBER_OF_SECTIONS_16 {
                i32::from(raw)
            } else {
                i32::from(raw.cast_signed())
            };
            (number, 14)
        };
        let symbol_type = u16_at(record, rest).unwrap_or(0);
        let storage_class = u8_at(record, rest.wrapping_add(2)).unwrap_or(0);
        let number_of_aux_symbols = u8_at(record, rest.wrapping_add(3)).unwrap_or(0);
        let aux_start = start.checked_add(size);
        let aux_len = usize::from(number_of_aux_symbols).checked_mul(size);
        let aux = aux_start
            .zip(aux_len)
            .and_then(|(s, l)| self.data.get(s..s.checked_add(l)?))
            .ok_or_else(|| self.error(index, "symbol aux records (past the end of the table)"))?;
        Ok(Symbol {
            index,
            name,
            value,
            section_number,
            symbol_type,
            storage_class,
            number_of_aux_symbols,
            aux,
            bigobj: self.bigobj,
        })
    }

    /// Iterates over the symbols, skipping aux records.
    #[must_use]
    pub fn iter(&self) -> SymbolIter<'a> {
        SymbolIter {
            table: *self,
            index: 0,
        }
    }

    #[cold]
    fn error(&self, index: u32, what: &str) -> crate::Error {
        self.source.malformed(
            entry_offset(self.file_offset, u64::from(index), self.record_size()),
            format!("{what} (symbol {index})"),
        )
    }
}

impl<'a> IntoIterator for SymbolTable<'a> {
    type Item = Result<Symbol<'a>>;
    type IntoIter = SymbolIter<'a>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// Iterator over the symbols of a [`SymbolTable`], skipping aux records.
///
/// An entry that cannot be decoded yields an error; if its aux records run
/// past the table, iteration stops after the error.
#[derive(Clone, Debug)]
pub struct SymbolIter<'a> {
    table: SymbolTable<'a>,
    index: u32,
}

impl SymbolIter<'_> {
    /// Record index of the symbol the next call to `next` will yield.
    #[must_use]
    pub fn index(&self) -> u32 {
        self.index
    }
}

impl<'a> Iterator for SymbolIter<'a> {
    type Item = Result<Symbol<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.index >= self.table.count {
            return None;
        }
        let index = self.index;
        match self.table.get(index) {
            Ok(symbol) => {
                self.index = index
                    .saturating_add(1)
                    .saturating_add(u32::from(symbol.number_of_aux_symbols));
                Some(Ok(symbol))
            }
            Err(error) => {
                // Skip the record and its aux records if the count is
                // readable; otherwise stop.
                let size = self.table.record_size();
                let aux = usize::try_from(index)
                    .ok()
                    .and_then(|i| i.checked_add(1)?.checked_mul(size)?.checked_sub(1))
                    .and_then(|last| u8_at(self.table.data, last));
                self.index = match aux {
                    Some(aux) => index
                        .saturating_add(1)
                        .saturating_add(u32::from(aux))
                        .min(self.table.count),
                    None => self.table.count,
                };
                Some(Err(error))
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let left = usize::try_from(self.table.count.saturating_sub(self.index)).unwrap_or(0);
        (usize::from(left != 0), Some(left))
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn record(name: &[u8; 8], value: u32, section: u16, ty: u16, class: u8, aux: u8) -> Vec<u8> {
        let mut r = Vec::from(&name[..]);
        r.extend_from_slice(&value.to_le_bytes());
        r.extend_from_slice(&section.to_le_bytes());
        r.extend_from_slice(&ty.to_le_bytes());
        r.push(class);
        r.push(aux);
        r
    }

    #[test]
    fn decode_regular() {
        let mut file = vec![0u8; 4];
        file.extend(record(b".text\0\0\0", 0, 1, 0, 3, 1));
        let mut aux = vec![0u8; 18];
        aux[0] = 16;
        aux[12] = 7;
        aux[14] = 5;
        file.extend(aux);
        file.extend(record(&[0, 0, 0, 0, 4, 0, 0, 0], 8, 0, 0, 2, 0));
        file.extend(record(b"abs\0\0\0\0\0", 3, 0xffff, 0, 2, 0));
        file.extend(record(b"dbg\0\0\0\0\0", 0, 0xfffe, 0, 103, 0));
        file.extend(b"\x0f\0\0\0long_name\0\0\0");
        let source = Source::new(Path::new("t.o"));
        let table = SymbolTable::parse(&file, 4, 5, false, source).unwrap();
        let symbols: Vec<_> = table.iter().map(Result::unwrap).collect();
        assert_eq!(symbols.len(), 4);
        assert_eq!(symbols[0].name, b".text");
        let def = symbols[0].section_definition().unwrap();
        assert_eq!(def.length, 16);
        assert_eq!(def.associative_section(), Some(7));
        assert_eq!(symbols[1].index, 2);
        assert_eq!(symbols[1].name, b"long_name");
        assert!(symbols[1].is_common());
        assert_eq!(symbols[2].section(), SectionNumber::Absolute);
        assert_eq!(symbols[3].section(), SectionNumber::Debug);
        assert!(table.get(5).is_err());
    }

    #[test]
    fn truncated_aux_is_an_error() {
        let mut file = record(b"f\0\0\0\0\0\0\0", 0, 0, 0, 103, 3);
        file.extend([0u8; 18]);
        let source = Source::new(Path::new("t.o"));
        let table = SymbolTable::parse(&file, 0, 0, false, source).unwrap();
        assert!(table.is_empty());
        let shifted = [&[0u8; 1][..], &file].concat();
        let table = SymbolTable::parse(&shifted, 1, 2, false, source).unwrap();
        let items: Vec<_> = table.iter().collect();
        assert_eq!(items.len(), 1);
        assert!(items[0].is_err());
    }
}
