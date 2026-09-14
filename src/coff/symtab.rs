//! The COFF symbol table of the image.
//!
//! PE images may carry the same 18-byte symbol records an object does, after
//! the last section's contents. MinGW's `nm`, `objdump` and `gdb` read them,
//! so qld writes one unless `-s` / `--strip-all` was given.
//!
//! What is written, in this order, which is a function of the layout alone:
//!
//! 1. one `STATIC` symbol per output section, with its section definition
//!    auxiliary record, as `objdump -t` expects;
//! 2. every global symbol the image defines, with its offset inside the
//!    section as `Value` and its output section number, which is how a PE
//!    image records an address;
//! 3. the globals that stayed undefined, which a COFF weak external's
//!    fallback leaves behind (GNU `ld` keeps them too).
//!
//! Local symbols are not written yet; see `docs/compatibility.md`.

#![deny(clippy::arithmetic_side_effects)]

use crate::symbols::DefinitionKind;

use super::layout::Layout;
use super::read::consts::{IMAGE_SYM_CLASS_EXTERNAL, IMAGE_SYM_CLASS_STATIC};
use super::reloc::{Addresses, Value};

/// Size of one symbol record.
pub const SYMBOL_SIZE: usize = 18;

/// The symbol table and its string table, ready to append to the image.
#[derive(Debug, Default)]
pub struct SymbolTable {
    /// The symbol records, followed by the string table.
    pub bytes: Vec<u8>,
    /// `NumberOfSymbols`, auxiliary records included.
    pub count: u32,
}

impl SymbolTable {
    /// Whether there is nothing to write.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
}

/// One symbol to write.
struct Entry<'a> {
    name: &'a [u8],
    value: u32,
    section: i16,
    storage_class: u8,
    /// The section definition auxiliary record, for section symbols.
    aux: Option<[u8; SYMBOL_SIZE]>,
}

/// Builds the image's symbol table.
#[must_use]
pub fn build(addresses: &Addresses<'_, '_>, layout: &Layout) -> SymbolTable {
    let mut entries: Vec<Entry<'_>> = Vec::new();
    for (index, section) in layout.sections.iter().enumerate() {
        let mut aux = [0u8; SYMBOL_SIZE];
        if let Some(slot) = aux.first_chunk_mut::<4>() {
            *slot = section.virtual_size.to_le_bytes();
        }
        entries.push(Entry {
            name: &section.name,
            value: 0,
            section: i16::try_from(index.saturating_add(1)).unwrap_or(0),
            storage_class: IMAGE_SYM_CLASS_STATIC,
            aux: Some(aux),
        });
    }

    // Globals, ordered by address then name so the table is deterministic.
    let mut defined: Vec<Entry<'_>> = Vec::new();
    let mut undefined: Vec<Entry<'_>> = Vec::new();
    for id in addresses.symbols.ids() {
        let name = addresses.symbols.name(id).bytes();
        match addresses.value(id) {
            // In an image, `Value` is the offset inside the section: a
            // reader adds the section's `VirtualAddress` and the image base.
            Some(Value::Address { rva, section }) => {
                let base = layout
                    .sections
                    .get(section as usize)
                    .map_or(0, |output| output.rva);
                defined.push(Entry {
                    name,
                    value: rva.wrapping_sub(base),
                    section: i16::try_from(section.saturating_add(1)).unwrap_or(0),
                    storage_class: IMAGE_SYM_CLASS_EXTERNAL,
                    aux: None,
                });
            }
            Some(Value::Absolute(value)) => defined.push(Entry {
                name,
                value: u32::try_from(value).unwrap_or(0),
                // IMAGE_SYM_ABSOLUTE.
                section: -1,
                storage_class: IMAGE_SYM_CLASS_EXTERNAL,
                aux: None,
            }),
            None => {
                // Only symbols the link actually referred to are worth
                // recording as undefined.
                if addresses.symbols.definition_kind(id) != DefinitionKind::Lazy {
                    undefined.push(Entry {
                        name,
                        value: 0,
                        section: 0,
                        storage_class: IMAGE_SYM_CLASS_EXTERNAL,
                        aux: None,
                    });
                }
            }
        }
    }
    defined.sort_by(|a, b| (a.section, a.value, a.name).cmp(&(b.section, b.value, b.name)));
    undefined.sort_by(|a, b| a.name.cmp(b.name));
    entries.append(&mut defined);
    entries.append(&mut undefined);

    let mut records: Vec<u8> = Vec::with_capacity(entries.len().saturating_mul(SYMBOL_SIZE));
    let mut strings: Vec<u8> = vec![0, 0, 0, 0];
    let mut count = 0u32;
    for entry in &entries {
        if entry.name.len() <= 8 {
            let mut name = [0u8; 8];
            if let Some(slot) = name.get_mut(..entry.name.len()) {
                slot.copy_from_slice(entry.name);
            }
            records.extend_from_slice(&name);
        } else {
            records.extend_from_slice(&0u32.to_le_bytes());
            records.extend_from_slice(&u32::try_from(strings.len()).unwrap_or(0).to_le_bytes());
            strings.extend_from_slice(entry.name);
            strings.push(0);
        }
        records.extend_from_slice(&entry.value.to_le_bytes());
        records.extend_from_slice(&entry.section.to_le_bytes());
        records.extend_from_slice(&0u16.to_le_bytes()); // Type
        records.push(entry.storage_class);
        records.push(u8::from(entry.aux.is_some()));
        count = count.saturating_add(1);
        if let Some(aux) = entry.aux {
            records.extend_from_slice(&aux);
            count = count.saturating_add(1);
        }
    }
    let size = u32::try_from(strings.len()).unwrap_or(4);
    if let Some(slot) = strings.first_chunk_mut::<4>() {
        *slot = size.to_le_bytes();
    }
    records.extend_from_slice(&strings);
    SymbolTable {
        bytes: records,
        count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_table_writes_nothing() {
        let table = SymbolTable::default();
        assert!(table.is_empty());
        assert!(table.bytes.is_empty());
    }
}
