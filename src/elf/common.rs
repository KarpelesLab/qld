//! Common symbol allocation.
//!
//! A common symbol that wins resolution gets space at the end of `.bss`,
//! with the size of the largest tentative definition (resolution already
//! picked it) and the alignment its `st_value` gives. Symbols are laid out
//! in input order of their winning definitions, so the layout does not
//! depend on symbol IDs or threads.

#![deny(clippy::arithmetic_side_effects)]

use rayon::prelude::*;

use crate::ids::SymbolId;
use crate::symbols::DefinitionKind;

use super::refs::Refs;
use super::resolve::AUX_COMDAT;

/// Allocated common symbols.
#[derive(Debug, Default)]
pub struct Commons {
    /// `(symbol, offset in the common block)`, sorted by symbol.
    pub entries: Vec<(SymbolId, u64)>,
    /// Total size.
    pub size: u64,
    /// Largest alignment.
    pub align: u64,
}

impl Commons {
    /// Offset of `id` in the common block.
    #[must_use]
    pub fn offset(&self, id: SymbolId) -> Option<u64> {
        let at = self.entries.binary_search_by_key(&id, |(i, _)| *i).ok()?;
        self.entries.get(at).map(|(_, offset)| *offset)
    }
}

/// Lays out the common symbols that won resolution.
#[must_use]
pub fn allocate<F: crate::elf::read::ElfFormat>(refs: &Refs<'_, '_, F>) -> Commons {
    let symbols = refs.symbols;
    let mut winners: Vec<(u64, u32, SymbolId, u64, u64)> = symbols
        .ids()
        .collect::<Vec<_>>()
        .into_par_iter()
        .filter_map(|id| {
            let def = symbols.definition(id);
            if def.kind != DefinitionKind::Common {
                return None;
            }
            let object = refs.files.get(def.file.index())?.object.as_ref()?;
            let index = (def.index as usize).checked_add(object.first_global)?;
            let raw = object.elf.symbols().get_raw(index)?;
            let align = raw.st_value.max(1);
            let align = if align.is_power_of_two() { align } else { 1 };
            Some((
                def.position.raw(),
                def.index,
                id,
                def.aux & !AUX_COMDAT,
                align,
            ))
        })
        .collect();
    winners.sort_unstable();
    let mut commons = Commons {
        entries: Vec::with_capacity(winners.len()),
        size: 0,
        align: 1,
    };
    for (_, _, id, size, align) in winners {
        let Some(offset) = commons
            .size
            .checked_add(align.wrapping_sub(1))
            .map(|v| v & !align.wrapping_sub(1))
        else {
            continue;
        };
        commons.size = offset.saturating_add(size);
        commons.align = commons.align.max(align);
        commons.entries.push((id, offset));
    }
    commons.entries.sort_unstable();
    commons
}
