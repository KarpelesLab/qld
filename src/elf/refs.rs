//! Relocation targets: from a symbol index in a file to the definition it
//! refers to.
//!
//! A relocation names a symbol of its own file. A local symbol is defined
//! right there; a global one goes through the file's symbol IDs to the
//! definition resolution chose, which may be in another file, a common
//! symbol, a linker-defined symbol, or nothing.

#![deny(clippy::arithmetic_side_effects)]

use crate::elf::read::consts::{STB_WEAK, STT_GNU_IFUNC, STT_TLS};
use crate::elf::read::{RawSymbol, SectionIndex};
use crate::ids::{FileId, SectionId, SymbolId};
use crate::symbols::{DefinitionKind, Resolution, SymbolTable};

use super::inputs::ElfInput;
use super::sections::Sections;

/// The file ID used in [`crate::symbols::Definition`]s of linker-defined
/// symbols.
pub const LINKER_FILE: FileId = FileId::from_u32(u32::MAX - 1);

/// Where a relocation's symbol is defined.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Def {
    /// In section `section` of file `file`, at `value` (raw `st_value`).
    Section {
        /// Defining file.
        file: usize,
        /// Section index in that file.
        section: u32,
        /// The symbol's `st_value`.
        value: u64,
    },
    /// An absolute value.
    Absolute(u64),
    /// A common symbol (global), allocated by the linker.
    Common(SymbolId),
    /// A linker-defined symbol (global).
    Linker(SymbolId),
    /// Undefined; `weak` tells whether the reference is weak.
    Undefined {
        /// The reference (in the referring file) is weak.
        weak: bool,
    },
}

/// A resolved relocation target.
#[derive(Clone, Copy, Debug)]
pub struct Target {
    /// The global symbol, for global references.
    pub global: Option<SymbolId>,
    /// The definition.
    pub def: Def,
    /// The defining symbol's raw entry, when there is one in an object.
    pub raw: Option<RawSymbol>,
}

impl Target {
    /// Whether the symbol is an IFUNC.
    #[must_use]
    pub fn is_ifunc(&self) -> bool {
        self.raw.is_some_and(|raw| raw.kind() == STT_GNU_IFUNC)
            && matches!(self.def, Def::Section { .. })
    }

    /// Whether the symbol is thread-local.
    #[must_use]
    pub fn is_tls(&self) -> bool {
        self.raw.is_some_and(|raw| raw.kind() == STT_TLS)
    }

    /// Whether it is a section symbol.
    #[must_use]
    pub fn is_section_symbol(&self) -> bool {
        self.raw
            .is_some_and(|raw| raw.kind() == crate::elf::read::consts::STT_SECTION)
    }
}

/// Read-only access to everything needed to resolve relocation targets.
#[derive(Clone, Copy)]
pub struct Refs<'r, 'a> {
    /// All inputs.
    pub files: &'r [ElfInput<'a>],
    /// The global symbol table.
    pub symbols: &'r SymbolTable<'a>,
    /// The resolution result.
    pub resolution: &'r Resolution<'a>,
    /// Section numbering and liveness.
    pub sections: &'r Sections,
}

impl<'a> Refs<'_, 'a> {
    /// The global symbol ID of symbol `index` of `file`, if it is global.
    #[must_use]
    pub fn global_id(&self, file: usize, index: usize) -> Option<SymbolId> {
        let object = self.files.get(file)?.object.as_ref()?;
        let local = index.checked_sub(object.first_global)?;
        self.resolution
            .symbol_ids(FileId::new(file))
            .get(local)
            .copied()
    }

    /// Resolves symbol `index` of `file`.
    #[must_use]
    pub fn target(&self, file: usize, index: usize) -> Option<Target> {
        let object = self.files.get(file)?.object.as_ref()?;
        let symbols = object.elf.symbols();
        let raw = symbols.get_raw(index)?;
        if index < object.first_global {
            let def = local_def(file, index, &raw, symbols)?;
            return Some(Target {
                global: None,
                def,
                raw: Some(raw),
            });
        }
        let id = self.global_id(file, index)?;
        Some(self.global_target(id, raw.binding() == STB_WEAK))
    }

    /// Resolves global symbol `id`; `weak` is the binding of the reference.
    #[must_use]
    pub fn global_target(&self, id: SymbolId, weak: bool) -> Target {
        let def = self.symbols.definition(id);
        let undefined = Target {
            global: Some(id),
            def: Def::Undefined { weak },
            raw: None,
        };
        match def.kind {
            DefinitionKind::Undefined | DefinitionKind::Lazy | DefinitionKind::Shared => undefined,
            DefinitionKind::Common => Target {
                global: Some(id),
                def: Def::Common(id),
                raw: None,
            },
            DefinitionKind::Regular | DefinitionKind::Weak => {
                if def.file == LINKER_FILE {
                    return Target {
                        global: Some(id),
                        def: Def::Linker(id),
                        raw: None,
                    };
                }
                let file = def.file.index();
                let Some(object) = self.files.get(file).and_then(|f| f.object.as_ref()) else {
                    // The internal file: `--defsym`.
                    return Target {
                        global: Some(id),
                        def: Def::Linker(id),
                        raw: None,
                    };
                };
                let symbols = object.elf.symbols();
                let Some(index) = (def.index as usize).checked_add(object.first_global) else {
                    return undefined;
                };
                let Some(raw) = symbols.get_raw(index) else {
                    return undefined;
                };
                match local_def(file, index, &raw, symbols) {
                    Some(d) => Target {
                        global: Some(id),
                        def: d,
                        raw: Some(raw),
                    },
                    None => undefined,
                }
            }
        }
    }

    /// The section a target is defined in.
    #[must_use]
    pub fn target_section(&self, target: &Target) -> Option<SectionId> {
        match target.def {
            Def::Section { file, section, .. } => self.sections.id(file, section),
            _ => None,
        }
    }
}

fn local_def(
    file: usize,
    index: usize,
    raw: &RawSymbol,
    symbols: &crate::elf::read::SymbolTable<'_, crate::elf::read::Elf64Le>,
) -> Option<Def> {
    Some(match symbols.section(index, raw).ok()? {
        SectionIndex::Section(section) => Def::Section {
            file,
            section,
            value: raw.st_value,
        },
        SectionIndex::Absolute => Def::Absolute(raw.st_value),
        SectionIndex::Undefined => Def::Undefined {
            weak: raw.binding() == STB_WEAK,
        },
        SectionIndex::Common | SectionIndex::Reserved(_) => Def::Absolute(0),
    })
}
