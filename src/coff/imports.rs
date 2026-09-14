//! Short import libraries and DLLs linked directly.
//!
//! MSVC-style import libraries hold [`ShortImport`] objects rather than real
//! COFF objects, and GNU `ld` also accepts a `.dll` on the command line and
//! imports whatever it exports. qld turns both into the *same* thing a GNU
//! `dlltool` import library holds: ordinary COFF objects whose `.idata$N`
//! sections build the import directory. Everything downstream — resolution,
//! grouped-section ordering, relocations — then treats them like any other
//! object, with no special case.
//!
//! The generated objects are registered in the link's [`FileTable`], so
//! their bytes live as long as the link and the names inside them can be
//! borrowed for the symbol table. Each import group produces `<id>h.o`
//! (the import directory entry), one `<id>s<NNNNN>.o` per import, and
//! `<id>t.o` (the terminators and the DLL name), named so that they sort in
//! that order. They are lazy: nothing is linked in unless the image refers
//! to one of the symbols.

#![deny(clippy::arithmetic_side_effects)]

use std::sync::Arc;

use crate::error::{Error, Result};
use crate::input::FileTable;

use super::implib::{self, Import, ImportName};
use super::read::{ExportTarget, PeImage, ShortImport};

/// The imports of one DLL, collected while the inputs are walked.
#[derive(Clone, Debug)]
pub struct DllGroup {
    /// The DLL name, as the import directory records it.
    pub dll: Vec<u8>,
    /// The input number the group takes, so its symbols keep the command
    /// line's precedence order.
    pub position: u32,
    /// The imports, in the order they were seen, without duplicates.
    pub imports: Vec<Import>,
}

impl DllGroup {
    /// The `dlltool` identifier for the group: the DLL name with every
    /// character that is not alphanumeric replaced by `_`.
    #[must_use]
    pub fn id(&self) -> Vec<u8> {
        self.dll
            .iter()
            .map(|&byte| {
                if byte.is_ascii_alphanumeric() {
                    byte
                } else {
                    b'_'
                }
            })
            .collect()
    }
}

/// The import groups of a link, keyed by DLL name without regard to case, as
/// on Windows.
#[derive(Debug, Default)]
pub struct Groups {
    groups: Vec<DllGroup>,
}

impl Groups {
    /// An empty set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether anything was collected.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    /// The groups, in the order their DLLs were first seen.
    #[must_use]
    pub fn groups(&self) -> &[DllGroup] {
        &self.groups
    }

    /// Records one import, creating the DLL's group if needed.
    ///
    /// `position` is used only when the group is created, so a DLL keeps the
    /// place of its first import.
    pub fn add(&mut self, dll: &[u8], position: u32, import: Import) {
        let key = dll.to_ascii_lowercase();
        let at = match self
            .groups
            .iter()
            .position(|group| group.dll.to_ascii_lowercase() == key)
        {
            Some(at) => at,
            None => {
                self.groups.push(DllGroup {
                    dll: dll.to_vec(),
                    position,
                    imports: Vec::new(),
                });
                self.groups.len().saturating_sub(1)
            }
        };
        let Some(group) = self.groups.get_mut(at) else {
            return;
        };
        if !group
            .imports
            .iter()
            .any(|existing| existing.symbol == import.symbol)
        {
            group.imports.push(import);
        }
    }

    /// Records the import a short import object describes.
    pub fn add_short_import(&mut self, import: &ShortImport<'_>, position: u32) {
        let name = match import.import_name() {
            super::read::ImportName::Ordinal(ordinal) => ImportName::Ordinal(ordinal),
            super::read::ImportName::Name { hint, name } => ImportName::Name {
                hint,
                name: name.to_vec(),
            },
        };
        self.add(
            import.dll_name,
            position,
            Import {
                symbol: import.symbol_name.to_vec(),
                name,
                data: !import.defines_symbol_name(),
            },
        );
    }

    /// Records every symbol a directly linked DLL exports.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Malformed`](crate::Error::Malformed) if the export
    /// directory cannot be read.
    pub fn add_dll(&mut self, image: &PeImage<'_>, position: u32, fallback: &[u8]) -> Result<()> {
        let Some(directory) = image.exports()? else {
            return Ok(());
        };
        let dll = if directory.dll_name.is_empty() {
            fallback
        } else {
            directory.dll_name
        };
        for export in directory.all()? {
            let Some(name) = export.name else {
                // An ordinal-only export.
                let ordinal = u16::try_from(export.ordinal).unwrap_or(0);
                self.add(
                    dll,
                    position,
                    Import {
                        symbol: format!("ordinal{ordinal}").into_bytes(),
                        name: ImportName::Ordinal(ordinal),
                        data: true,
                    },
                );
                continue;
            };
            self.add(
                dll,
                position,
                Import {
                    symbol: name.to_vec(),
                    name: ImportName::Name {
                        hint: 0,
                        name: name.to_vec(),
                    },
                    // An export whose address is in a data section gets no
                    // thunk; a forwarder is always reached through one.
                    data: match export.target {
                        ExportTarget::Rva(rva) => image.is_data_rva(rva),
                        ExportTarget::Forwarder(_) => false,
                    },
                },
            );
        }
        Ok(())
    }
}

/// One generated object, ready to become a lazy link input.
#[derive(Debug)]
pub struct Generated {
    /// The file name, which also decides the grouped-section order.
    pub name: String,
    /// The object's bytes, registered in the file table.
    pub id: crate::ids::FileId,
    /// The symbols it defines, for the lazy name list.
    pub defines: Vec<Vec<u8>>,
    /// The input number the object takes.
    pub position: u32,
    /// Its ordinal within the group.
    pub ordinal: u32,
}

/// Generates the COFF objects for every group and registers them in `table`.
///
/// # Errors
///
/// Returns [`Error::Limit`] if an object does not fit the COFF format, and
/// any error from registering the bytes.
pub fn generate(table: &FileTable, groups: &Groups, machine: u16) -> Result<Vec<Generated>> {
    let mut out = Vec::new();
    for group in groups.groups() {
        let id = group.id();
        let text = String::from_utf8_lossy(&id).into_owned();
        let head_symbol = [b"_head_".as_slice(), &id].concat();
        let iname_symbol = [b"__".as_slice(), &id, b"_iname"].concat();
        let mut ordinal = 0u32;
        let mut push = |name: String,
                        bytes: Vec<u8>,
                        defines: Vec<Vec<u8>>,
                        out: &mut Vec<Generated>|
         -> Result<()> {
            let file = table.add_bytes(name.clone(), Arc::from(bytes))?;
            out.push(Generated {
                name,
                id: file,
                defines,
                position: group.position,
                ordinal,
            });
            ordinal = ordinal.saturating_add(1);
            Ok(())
        };
        push(
            format!("{text}h.o"),
            implib::head(machine, &head_symbol, &iname_symbol)?,
            vec![head_symbol.clone()],
            &mut out,
        )?;
        for (index, import) in group.imports.iter().enumerate() {
            push(
                format!("{text}s{index:05}.o"),
                implib::import_member(machine, import, &head_symbol)?,
                import.defines(),
                &mut out,
            )?;
        }
        push(
            format!("{text}t.o"),
            implib::tail(machine, &iname_symbol, &group.dll)?,
            vec![iname_symbol],
            &mut out,
        )?;
    }
    Ok(out)
}

/// Rejects an import group for a machine qld cannot write import thunks for.
///
/// # Errors
///
/// Returns [`Error::Unimplemented`] for anything but x86-64.
pub fn check_machine(machine: u16) -> Result<()> {
    if machine == super::read::consts::IMAGE_FILE_MACHINE_AMD64 {
        return Ok(());
    }
    Err(Error::Unimplemented(format!(
        "short import libraries for machine {machine:#x} (roadmap M7: x86-64 first)"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn import(symbol: &[u8]) -> Import {
        Import {
            symbol: symbol.to_vec(),
            name: ImportName::Name {
                hint: 0,
                name: symbol.to_vec(),
            },
            data: false,
        }
    }

    #[test]
    fn groups_are_keyed_without_regard_to_case_and_keep_their_place() {
        let mut groups = Groups::new();
        groups.add(b"KERNEL32.dll", 3, import(b"Sleep"));
        groups.add(b"kernel32.DLL", 9, import(b"GetLastError"));
        groups.add(b"USER32.dll", 4, import(b"MessageBoxA"));
        groups.add(b"KERNEL32.dll", 9, import(b"Sleep"));
        let names: Vec<&[u8]> = groups.groups().iter().map(|g| g.dll.as_slice()).collect();
        assert_eq!(names, [&b"KERNEL32.dll"[..], b"USER32.dll"]);
        assert_eq!(groups.groups()[0].position, 3, "the first sighting wins");
        assert_eq!(groups.groups()[0].imports.len(), 2, "duplicates are folded");
        assert_eq!(groups.groups()[0].id(), b"KERNEL32_dll");
    }

    #[test]
    fn generated_objects_sort_head_then_imports_then_tail() {
        let mut groups = Groups::new();
        groups.add(b"a.dll", 1, import(b"one"));
        groups.add(b"a.dll", 1, import(b"two"));
        let table = FileTable::new();
        let generated = generate(
            &table,
            &groups,
            super::super::read::consts::IMAGE_FILE_MACHINE_AMD64,
        )
        .unwrap();
        let names: Vec<&str> = generated.iter().map(|item| item.name.as_str()).collect();
        assert_eq!(
            names,
            ["a_dllh.o", "a_dlls00000.o", "a_dlls00001.o", "a_dllt.o"]
        );
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted);
        assert_eq!(
            generated[1].defines,
            vec![b"__imp_one".to_vec(), b"one".to_vec()]
        );
        // Every generated object must parse as an ordinary COFF object.
        for item in &generated {
            let data = table.get(item.id).expect("registered").data();
            super::super::read::CoffObject::parse(
                data,
                super::super::read::Source::new(std::path::Path::new(&item.name)),
            )
            .unwrap();
        }
    }
}
