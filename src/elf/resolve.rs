//! ELF symbol precedence, COMDAT deduplication, and resolution diagnostics.
//!
//! # Precedence
//!
//! [`ElfRules`] ranks definitions as GNU ld, gold, lld and mold do:
//! strong > common (the larger wins) > weak > shared and lazy, with ties
//! going to the earlier input. Two strong definitions are a duplicate-symbol
//! error, unless both come from COMDAT group sections (C++ inline functions,
//! static locals and template instances are emitted once per object; only
//! one group survives).
//!
//! Between a shared library and an unextracted archive member, the earlier
//! one on the command line wins, as in GNU ld: a member of an archive that
//! comes first is extracted by a non-weak reference, and the shared library
//! is used otherwise. This is what keeps gcc's `-lgcc --as-needed -lgcc_s`
//! from binding `__popcountdi2` to `libgcc_s.so.1` (and adding a
//! `DT_NEEDED` on it) where GNU ld links the helper from `libgcc.a`. A
//! symbol that ends resolution with an unextracted lazy definition (only
//! weak references, or none) is bound to its shared definition afterwards
//! by [`dso::bind_unextracted`](super::dso::bind_unextracted).
//!
//! # COMDAT groups
//!
//! Groups are claimed while resolution loads files ([`ComdatHook`], a
//! [`RoundHook`]): for each signature, the copy in the file loaded in the
//! earliest round wins, and among the files of one round the one with the
//! lowest input position. A file that loses a claim reports the definitions
//! in that group's sections as [`SymbolUse::Ignore`](crate::symbols::SymbolUse::Ignore), so they never compete
//! and its references bind to the kept copy. [`deduplicate_comdat`] then
//! marks the members of the discarded copies dead.
//!
//! GNU ld keeps the first copy it loads. It loads archive members as it
//! meets them on the command line (rescanning `--start-group` groups), so a
//! member extracted by a reference from a later file is loaded after that
//! file; qld's rounds extract members after every file of the previous
//! round, so a group in such a member can lose to a copy in a later object
//! where GNU ld keeps the member's. The copies are interchangeable by the
//! one-definition rule.

#![deny(clippy::arithmetic_side_effects)]

use core::cmp::Ordering;

use rayon::prelude::*;

use crate::diag::{Diagnostic, DiagnosticSink};
use crate::elf::read::SectionIndex;
use crate::error::Result;
use crate::symbols::{
    Definition, DefinitionKind, GroupClaims, Resolution, Resolver, RoundFile, RoundHook, SymbolName,
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
        // A lazy member and a shared library: the earlier input wins.
        if matches!(
            (a.kind, b.kind),
            (DefinitionKind::Lazy, DefinitionKind::Shared)
                | (DefinitionKind::Shared, DefinitionKind::Lazy)
        ) {
            return b.position.cmp(&a.position);
        }
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

/// Claims COMDAT groups as resolution rounds load files; see the [module
/// documentation](self).
#[derive(Debug, Default)]
pub struct ComdatHook<'a> {
    claims: GroupClaims<'a>,
}

impl<'a> RoundHook<ElfInput<'a>> for ComdatHook<'a> {
    fn after_load(
        &mut self,
        _round: usize,
        files: &mut [RoundFile<'_, ElfInput<'a>>],
    ) -> Result<()> {
        let round = self.claims.begin_round();
        files.par_iter().for_each(|round_file| {
            let Some(object) = &round_file.file.object else {
                return;
            };
            for group in &object.groups {
                round.offer(
                    SymbolName::new(group.signature),
                    round_file.file.position,
                    round_file.id,
                );
            }
        });
        files.par_iter_mut().for_each(|round_file| {
            let id = round_file.id;
            let Some(object) = &mut round_file.file.object else {
                return;
            };
            let discarded: Vec<bool> = object
                .groups
                .iter()
                .map(|group| round.owner(&SymbolName::new(group.signature)) != Some(id))
                .collect();
            if discarded.contains(&true) {
                object.discard_groups(discarded);
            }
        });
        Ok(())
    }
}

/// Marks dead the members of the COMDAT group copies [`ComdatHook`]
/// discarded. Returns the number of discarded groups.
pub fn deduplicate_comdat(files: &[ElfInput<'_>], sections: &mut Sections) -> usize {
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
        for (group, &dropped) in object.groups.iter().zip(&object.discarded_groups) {
            if !dropped {
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

/// Reports duplicate definitions, lld-style. Returns the number of errors.
pub fn report_duplicates(
    files: &[ElfInput<'_>],
    resolution: &Resolution<'_>,
    sections: &Sections,
    demangle: bool,
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
        let mut diagnostic = Diagnostic::error(format!(
            "duplicate symbol: {}",
            crate::hints::display_symbol(duplicate.name.bytes(), demangle)
        ))
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
    use crate::ids::FileId;
    use crate::symbols::InputPosition;
    use crate::symbols::takes_precedence;

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

        // Shared versus lazy: the earlier input, either way round.
        let lazy_early = def(DefinitionKind::Lazy, 1, 0);
        let shared = def(DefinitionKind::Shared, 2, 0);
        let lazy_late = def(DefinitionKind::Lazy, 3, 0);
        assert!(takes_precedence(&rules, &lazy_early, &shared));
        assert!(!takes_precedence(&rules, &shared, &lazy_early));
        assert!(takes_precedence(&rules, &shared, &lazy_late));
        assert!(!takes_precedence(&rules, &lazy_late, &shared));
        assert!(takes_precedence(&rules, &weak, &lazy_early));

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
