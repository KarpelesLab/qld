//! A relocatable object as the linker uses it: the parsed file, its atoms,
//! and its global symbols in the form symbol resolution wants.

#![deny(clippy::arithmetic_side_effects)]

use crate::error::Result;
use crate::macho::read::consts::{N_ABS, N_INDR, N_SECT, N_UNDF};
use crate::macho::read::{AtomRelocation, Atomization, ObjectFile, Symbol, SymbolKind};
use crate::symbols::{DefinitionKind, SymbolName, SymbolUse};

/// Marks a symbol table entry that is not a global.
pub const NOT_GLOBAL: u32 = u32::MAX;

/// A parsed object file with its atoms.
#[derive(Debug)]
pub struct LinkObject<'a> {
    /// The object.
    pub file: ObjectFile<'a>,
    /// Its atoms.
    pub atoms: Atomization,
    /// Global symbol names, in symbol table order.
    pub names: Vec<SymbolName<'a>>,
    /// How each global takes part in resolution.
    pub uses: Vec<SymbolUse>,
    /// Symbol table index of each global.
    pub global_symbols: Vec<u32>,
    /// Global index of each symbol table entry, or [`NOT_GLOBAL`].
    pub global_of_symbol: Vec<u32>,
    /// Relocations of each section, mapped to atoms; filled after resolution.
    pub relocations: Vec<Vec<AtomRelocation>>,
}

/// The `aux` bit of a definition that may be hidden automatically
/// (`N_WEAK_DEF | N_WEAK_REF`, `.weak_def_can_be_hidden`).
pub const AUX_PRIVATE_EXTERN: u64 = 1;

impl<'a> LinkObject<'a> {
    /// Parses and atomizes `file`.
    ///
    /// # Errors
    ///
    /// Malformed symbols or sections.
    pub fn new(file: ObjectFile<'a>) -> Result<Self> {
        let atoms = Atomization::new(&file)?;
        let symtab = file.symbols();
        let mut names = Vec::new();
        let mut uses = Vec::new();
        let mut global_symbols = Vec::new();
        let mut global_of_symbol = vec![NOT_GLOBAL; symtab.len()];
        for symbol in symtab.iter() {
            let symbol = symbol?;
            if !symbol.is_external() {
                continue;
            }
            let use_ = symbol_use(&symbol);
            let index = u32::try_from(names.len()).unwrap_or(NOT_GLOBAL);
            if let Some(slot) = usize::try_from(symbol.index)
                .ok()
                .and_then(|i| global_of_symbol.get_mut(i))
            {
                *slot = index;
            }
            names.push(SymbolName::new(symbol.name));
            uses.push(use_);
            global_symbols.push(symbol.index);
        }
        Ok(Self {
            file,
            atoms,
            names,
            uses,
            global_symbols,
            global_of_symbol,
            relocations: Vec::new(),
        })
    }

    /// Loads the relocations of every section.
    ///
    /// # Errors
    ///
    /// Malformed relocations.
    pub fn load_relocations(&mut self) -> Result<()> {
        let mut all = Vec::with_capacity(self.file.sections().len());
        for index in 0..self.file.sections().len() {
            all.push(self.atoms.relocations(&self.file, index)?);
        }
        self.relocations = all;
        Ok(())
    }

    /// The names an archive member would define, for its lazy entry.
    ///
    /// # Errors
    ///
    /// Malformed symbols.
    pub fn defined_names(file: &ObjectFile<'a>) -> Result<Vec<SymbolName<'a>>> {
        let mut out = Vec::new();
        for symbol in file.symbols().iter() {
            let symbol = symbol?;
            if symbol.is_external() && (symbol.is_defined() || symbol.is_common()) {
                out.push(SymbolName::new(symbol.name));
            }
        }
        Ok(out)
    }
}

/// How an external symbol takes part in resolution.
#[must_use]
pub fn symbol_use(symbol: &Symbol<'_>) -> SymbolUse {
    let aux = u64::from(symbol.is_private_external());
    match symbol.n_type & 0x0e {
        N_SECT | N_ABS => SymbolUse::Definition {
            kind: if symbol.is_weak_def() {
                DefinitionKind::Weak
            } else {
                DefinitionKind::Regular
            },
            aux,
        },
        N_UNDF if symbol.n_value != 0 => SymbolUse::Definition {
            kind: DefinitionKind::Common,
            aux: symbol.n_value,
        },
        N_UNDF => SymbolUse::Reference {
            weak: symbol.is_weak_ref(),
        },
        N_INDR => SymbolUse::Definition {
            kind: DefinitionKind::Regular,
            aux,
        },
        _ => match symbol.kind() {
            SymbolKind::PreboundUndefined => SymbolUse::Reference { weak: false },
            _ => SymbolUse::Ignore,
        },
    }
}
