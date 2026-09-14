//! ELF symbol precedence, COMDAT deduplication, and resolution diagnostics.
//!
//! # Precedence
//!
//! [`ElfRules`] ranks definitions as GNU ld, gold, lld and mold do:
//! strong > common (the larger wins) > weak > shared > lazy, with ties going
//! to the earlier input. Two strong definitions are a duplicate-symbol error,
//! unless both come from COMDAT group sections (C++ inline functions, static
//! locals and template instances are emitted once per object; only one group
//! survives).
//!
//! # COMDAT groups
//!
//! Resolution needs every live file's definitions before it can tell which
//! group copy survives, and archive members are loaded round by round, so
//! groups are deduplicated right after resolution: for each signature the
//! copy in the earliest live file (by input position) is kept and the
//! members of every other copy are discarded. Because a group's symbols are
//! ranked by the same input position, the definition resolution picked is
//! normally in the kept copy. The rare exceptions (a symbol that is strong in
//! a later copy and weak in the kept one) are re-resolved against the
//! surviving definitions by [`redirect_discarded`].

#![deny(clippy::arithmetic_side_effects)]

use core::cmp::Ordering;

use hashbrown::HashMap;
use rayon::prelude::*;

use crate::diag::{Diagnostic, DiagnosticSink};
use crate::elf::read::SectionIndex;
use crate::ids::{FileId, SymbolId};
use crate::symbols::{
    Definition, DefinitionKind, Resolution, Resolver, SymbolTable, SymbolUse, takes_precedence,
};

use super::inputs::ElfInput;
use super::sections::Sections;

/// Bit of [`Definition::aux`] set for definitions in COMDAT group sections.
pub const AUX_COMDAT: u64 = 1 << 63;

/// The ELF precedence rules; see the [module documentation](self).
#[derive(Clone, Copy, Debug, Default)]
pub struct ElfRules {
    /// `--allow-multiple-definition`: duplicate strong definitions are not
    /// errors (the first one wins).
    pub allow_multiple_definition: bool,
}

impl ElfRules {
    /// Rank of a definition kind; higher wins.
    #[must_use]
    pub const fn rank(kind: DefinitionKind) -> u8 {
        match kind {
            DefinitionKind::Undefined => 0,
            DefinitionKind::Lazy => 1,
            DefinitionKind::Shared => 2,
            DefinitionKind::Weak => 3,
            DefinitionKind::Common => 4,
            DefinitionKind::Regular => 5,
        }
    }
}

impl Resolver for ElfRules {
    fn compare(&self, a: &Definition, b: &Definition) -> Ordering {
        let by_rank = Self::rank(a.kind).cmp(&Self::rank(b.kind));
        if by_rank == Ordering::Equal && a.kind == DefinitionKind::Common {
            return (a.aux & !AUX_COMDAT).cmp(&(b.aux & !AUX_COMDAT));
        }
        by_rank
    }

    fn is_duplicate(&self, winner: &Definition, other: &Definition) -> bool {
        !self.allow_multiple_definition
            && winner.kind == DefinitionKind::Regular
            && other.kind == DefinitionKind::Regular
            && (winner.aux & AUX_COMDAT == 0 || other.aux & AUX_COMDAT == 0)
    }
}

/// Keeps the first copy of every COMDAT group (by input order) and marks the
/// members of the other copies dead in `sections`. Returns the number of
/// discarded groups.
pub fn deduplicate_comdat(files: &[ElfInput<'_>], sections: &mut Sections) -> usize {
    let mut seen: HashMap<&[u8], usize, foldhash::fast::FixedState> =
        HashMap::with_hasher(foldhash::fast::FixedState::with_seed(0x636f_6d64_6174));
    let mut discarded = 0usize;
    for (file_index, file) in files.iter().enumerate() {
        let Some(object) = &file.object else {
            continue;
        };
        if sections
            .base
            .get(file_index)
            .is_none_or(|&b| b == super::sections::NONE)
        {
            continue;
        }
        for group in &object.groups {
            if seen.insert(group.signature, file_index).is_none() {
                continue;
            }
            discarded = discarded.saturating_add(1);
            for &member in &group.members {
                if let Some(id) = sections.id(file_index, member)
                    && let Some(live) = sections.live.get_mut(id.index())
                {
                    *live = false;
                }
            }
        }
    }
    discarded
}

/// Whether the definition `def` lies in a dead section.
fn in_dead_section(files: &[ElfInput<'_>], sections: &Sections, def: &Definition) -> bool {
    let file_index = def.file.index();
    let Some(object) = files.get(file_index).and_then(|f| f.object.as_ref()) else {
        return false;
    };
    let Some(symbol_index) = (def.index as usize).checked_add(object.first_global) else {
        return false;
    };
    let Some(raw) = object.elf.symbols().get_raw(symbol_index) else {
        return false;
    };
    match object.elf.symbols().section(symbol_index, &raw) {
        Ok(SectionIndex::Section(section)) => !sections.is_live_in(file_index, section),
        _ => false,
    }
}

/// Re-resolves symbols whose winning definition lies in a discarded COMDAT
/// group section, choosing among the definitions in live sections. Symbols
/// left with no definition become undefined. Returns how many symbols were
/// redirected.
pub fn redirect_discarded(
    table: &SymbolTable<'_>,
    rules: &ElfRules,
    files: &[ElfInput<'_>],
    resolution: &Resolution<'_>,
    sections: &Sections,
) -> usize {
    let affected: Vec<SymbolId> = table
        .ids()
        .collect::<Vec<_>>()
        .into_par_iter()
        .filter(|&id| {
            let def = table.definition(id);
            matches!(
                def.kind,
                DefinitionKind::Regular | DefinitionKind::Weak | DefinitionKind::Common
            ) && in_dead_section(files, sections, &def)
        })
        .collect();
    if affected.is_empty() {
        return 0;
    }
    let mut flags = vec![false; table.len()];
    for id in &affected {
        if let Some(flag) = flags.get_mut(id.index()) {
            *flag = true;
        }
    }
    let mut candidates: Vec<(SymbolId, Definition)> = files
        .par_iter()
        .enumerate()
        .filter(|(index, _)| resolution.is_live(FileId::new(*index)))
        .flat_map_iter(|(index, file)| {
            let ids = resolution.symbol_ids(FileId::new(index));
            let flags = &flags;
            ids.iter().enumerate().filter_map(move |(symbol, &id)| {
                if !flags.get(id.index()).copied().unwrap_or(false) {
                    return None;
                }
                let object = file.object.as_ref()?;
                let SymbolUse::Definition { kind, aux } = *object.uses.get(symbol)? else {
                    return None;
                };
                let def = Definition {
                    kind,
                    file: FileId::new(index),
                    index: u32::try_from(symbol).ok()?,
                    position: file.position,
                    aux,
                };
                (!in_dead_section(files, sections, &def)).then_some((id, def))
            })
        })
        .collect();
    candidates.sort_unstable_by_key(|(id, def)| (*id, def.tie_key()));
    let mut best: Vec<(SymbolId, Definition)> = Vec::with_capacity(affected.len());
    for (id, def) in candidates {
        match best.last_mut() {
            Some((last, current)) if *last == id => {
                if takes_precedence(rules, &def, current) {
                    *current = def;
                }
            }
            _ => best.push((id, def)),
        }
    }
    for &id in &affected {
        let replacement = best
            .binary_search_by_key(&id, |(i, _)| *i)
            .ok()
            .and_then(|at| best.get(at))
            .map_or_else(Definition::undefined, |(_, def)| *def);
        table.replace_definition(id, &replacement);
    }
    affected.len()
}

/// Reports duplicate definitions, lld-style. Returns the number of errors.
pub fn report_duplicates(
    files: &[ElfInput<'_>],
    resolution: &Resolution<'_>,
    sections: &Sections,
    diagnostics: &dyn DiagnosticSink,
) -> usize {
    let mut errors = 0usize;
    for duplicate in resolution.duplicates() {
        // A definition whose section was discarded with its COMDAT group is
        // not a real conflict.
        let others: Vec<&Definition> = duplicate
            .others
            .iter()
            .filter(|def| !in_dead_section(files, sections, def))
            .collect();
        if others.is_empty() || in_dead_section(files, sections, &duplicate.winner) {
            continue;
        }
        let mut diagnostic =
            Diagnostic::error(format!("duplicate symbol: {}", duplicate.name.display()))
                .order(duplicate.winner.position.raw());
        for def in std::iter::once(&duplicate.winner).chain(others) {
            diagnostic = diagnostic.detail(format!("defined at {}", definition_site(files, def)));
        }
        diagnostics.emit(diagnostic);
        errors = errors.saturating_add(1);
    }
    errors
}

/// `file.o:(.section+0xoffset)` for a definition.
#[must_use]
pub fn definition_site(files: &[ElfInput<'_>], def: &Definition) -> String {
    let Some(file) = files.get(def.file.index()) else {
        return "<linker>".to_string();
    };
    let Some(object) = &file.object else {
        return file.display();
    };
    let index = (def.index as usize).saturating_add(object.first_global);
    let Ok(symbol) = object.elf.symbols().get(index) else {
        return file.display();
    };
    match symbol.section {
        SectionIndex::Section(section) => {
            let name = object.section(section).map_or_else(String::new, |s| {
                String::from_utf8_lossy(s.name).into_owned()
            });
            format!("{}:({name}+{:#x})", file.display(), symbol.value)
        }
        _ => file.display(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::symbols::InputPosition;

    fn def(kind: DefinitionKind, position: u32, aux: u64) -> Definition {
        Definition {
            kind,
            file: FileId::new(position as usize),
            index: 0,
            position: InputPosition::new(position, 0),
            aux,
        }
    }

    #[test]
    fn precedence_and_duplicates() {
        let rules = ElfRules::default();
        let strong = def(DefinitionKind::Regular, 2, 0);
        let weak = def(DefinitionKind::Weak, 1, 0);
        let common_small = def(DefinitionKind::Common, 0, 4);
        let common_large = def(DefinitionKind::Common, 3, 64);
        assert!(takes_precedence(&rules, &strong, &weak));
        assert!(takes_precedence(&rules, &common_small, &weak));
        assert!(takes_precedence(&rules, &common_large, &common_small));
        assert!(takes_precedence(&rules, &strong, &common_large));

        let comdat_a = def(DefinitionKind::Regular, 1, AUX_COMDAT);
        let comdat_b = def(DefinitionKind::Regular, 2, AUX_COMDAT);
        assert!(!rules.is_duplicate(&comdat_a, &comdat_b));
        assert!(rules.is_duplicate(&strong, &comdat_a));
        assert!(rules.is_duplicate(&strong, &def(DefinitionKind::Regular, 5, 0)));
        let permissive = ElfRules {
            allow_multiple_definition: true,
        };
        assert!(!permissive.is_duplicate(&strong, &def(DefinitionKind::Regular, 5, 0)));
    }
}
