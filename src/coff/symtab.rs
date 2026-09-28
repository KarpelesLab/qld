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

use crate::args::options::{LinkOptions, StripMode};
use crate::symbols::DefinitionKind;

use super::layout::Layout;
use super::options::PeOptions;
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
    /// The raw 8-byte `Name` field of each output section header.
    ///
    /// A name longer than eight bytes is truncated, as the PE/COFF
    /// specification requires of an image, unless long section names are on:
    /// then it becomes `/<offset>` into the string table, which is GNU
    /// `ld`'s `--enable-long-section-names`. See [`long_section_names`].
    pub section_names: Vec<[u8; 8]>,
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

/// Whether the image keeps section names longer than eight bytes, through
/// the string table its symbol table carries.
///
/// The PE/COFF specification gives an image no string table, so its section
/// names are truncated to the eight bytes of the header field; GNU `ld` and
/// lld both do that. GNU `ld` makes one exception, which qld follows: an
/// unstripped link whose output has DWARF sections keeps every long name,
/// because GDB finds `.debug_*` in an image by name and cannot read a
/// truncated one. `--enable-long-section-names` and
/// `--disable-long-section-names` decide instead when either is given.
#[must_use]
pub fn long_section_names(options: &LinkOptions, pe: &PeOptions, layout: &Layout) -> bool {
    if let Some(choice) = pe.enable_long_section_names {
        return choice;
    }
    options.strip == StripMode::None
        && layout
            .sections
            .iter()
            .any(|section| section.name.starts_with(b".debug_"))
}

/// Builds the image's symbol table. `long_names` comes from
/// [`long_section_names`].
#[must_use]
pub fn build(addresses: &Addresses<'_, '_>, layout: &Layout, long_names: bool) -> SymbolTable {
    // The string table starts with its own size, then the long section
    // names, so a section header can point into it.
    let mut strings: Vec<u8> = vec![0, 0, 0, 0];
    let mut section_names = Vec::with_capacity(layout.sections.len());
    for section in &layout.sections {
        section_names.push(section_name_field(&section.name, long_names, &mut strings));
    }

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
        section_names,
    }
}

/// The `Name` field of a section header: the name itself when it fits in
/// eight bytes, then either `/<decimal offset>` into the string table or the
/// first eight bytes of the name, depending on `long_names`.
fn section_name_field(name: &[u8], long_names: bool, strings: &mut Vec<u8>) -> [u8; 8] {
    let mut field = [0u8; 8];
    if name.len() <= 8 || !long_names {
        let len = name.len().min(8);
        if let (Some(slot), Some(source)) = (field.get_mut(..len), name.get(..len)) {
            slot.copy_from_slice(source);
        }
        return field;
    }
    let offset = strings.len();
    strings.extend_from_slice(name);
    strings.push(0);
    let reference = format!("/{offset}");
    let bytes = reference.as_bytes();
    let len = bytes.len().min(8);
    if let (Some(slot), Some(source)) = (field.get_mut(..len), bytes.get(..len)) {
        slot.copy_from_slice(source);
    }
    field
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_section_names_go_to_the_string_table() {
        let mut strings = vec![0u8; 4];
        assert_eq!(
            section_name_field(b".text", true, &mut strings),
            *b".text\0\0\0"
        );
        assert_eq!(strings.len(), 4, "a short name needs no string");
        let field = section_name_field(b".debug_info", true, &mut strings);
        assert_eq!(&field[..2], b"/4");
        assert_eq!(&strings[4..16], b".debug_info\0");
        let field = section_name_field(b".debug_abbrev", true, &mut strings);
        assert_eq!(&field[..3], b"/16");
    }

    /// Without long section names the image truncates, as the PE/COFF
    /// specification requires, and the string table stays empty.
    #[test]
    fn short_section_names_truncate_in_the_image() {
        let mut strings = vec![0u8; 4];
        assert_eq!(
            section_name_field(b".eh_frame", false, &mut strings),
            *b".eh_fram"
        );
        assert_eq!(
            section_name_field(b".debug_info", false, &mut strings),
            *b".debug_i"
        );
        assert_eq!(strings.len(), 4, "no long name was stored");
    }

    #[test]
    fn an_empty_table_writes_nothing() {
        let table = SymbolTable::default();
        assert!(table.is_empty());
        assert!(table.bytes.is_empty());
    }
}
