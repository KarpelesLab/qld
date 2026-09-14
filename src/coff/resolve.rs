//! COFF symbol resolution: the precedence rules, COMDAT selection and the
//! per-round hook that applies both.
//!
//! Precedence follows [`DefinitionKind`]: a strong definition beats a common
//! symbol, which beats an import-library definition, which beats a lazy
//! archive member. Ties go to the earlier input position, as everywhere in
//! qld.
//!
//! # COMDAT
//!
//! A COMDAT section names its group with the first external symbol defined in
//! it. [`ComdatTable`] keeps one copy per group, applying the
//! `IMAGE_COMDAT_SELECT_*` rule:
//!
//! | Selection | Rule |
//! | --- | --- |
//! | `NODUPLICATES` | a second copy is an error |
//! | `ANY` | the first copy wins |
//! | `SAME_SIZE` | copies must agree on size |
//! | `EXACT_MATCH` | copies must agree on size and checksum |
//! | `LARGEST` | the largest copy wins |
//! | `ASSOCIATIVE` | the section follows another section's fate |
//!
//! The copies of a group normally all arrive in the same resolution round
//! (they come from objects on the command line, or from members extracted
//! together). A claim made in an earlier round is final, so `LARGEST` picks
//! the largest copy among the round's files; see `docs/compatibility.md`.

#![deny(clippy::arithmetic_side_effects)]

use core::cmp::Ordering;

use hashbrown::HashMap;

use crate::diag::{Diagnostic, DiagnosticSink};
use crate::error::Result;
use crate::ids::FileId;
use crate::symbols::{
    Definition, DefinitionKind, InputPosition, Resolver, RoundFile, RoundHook, SymbolName,
};

use super::inputs::CoffInput;
use super::object::Comdat;
use super::read::consts::{
    IMAGE_COMDAT_SELECT_ASSOCIATIVE, IMAGE_COMDAT_SELECT_EXACT_MATCH, IMAGE_COMDAT_SELECT_LARGEST,
    IMAGE_COMDAT_SELECT_NODUPLICATES, IMAGE_COMDAT_SELECT_SAME_SIZE,
};

/// COFF symbol precedence.
#[derive(Clone, Copy, Debug, Default)]
pub struct CoffRules {
    /// `--allow-multiple-definition`: duplicates are not errors.
    pub allow_multiple_definition: bool,
}

impl Resolver for CoffRules {
    fn compare(&self, a: &Definition, b: &Definition) -> Ordering {
        match (a.kind as u8).cmp(&(b.kind as u8)) {
            // Between two common symbols the larger one wins, as in ELF.
            Ordering::Equal if a.kind == DefinitionKind::Common => a.aux.cmp(&b.aux),
            other => other,
        }
    }

    fn is_duplicate(&self, winner: &Definition, other: &Definition) -> bool {
        !self.allow_multiple_definition
            && winner.kind == DefinitionKind::Regular
            && other.kind == DefinitionKind::Regular
            && !winner.same_origin(other)
    }
}

/// The copy of a COMDAT group kept so far.
#[derive(Clone, Copy, Debug)]
struct Kept {
    selection: u8,
    length: u32,
    check_sum: u32,
    file: FileId,
    position: InputPosition,
    round: usize,
}

/// A COMDAT conflict worth reporting.
#[derive(Clone, Debug)]
pub struct ComdatConflict {
    /// The group's name.
    pub key: Vec<u8>,
    /// What is wrong.
    pub what: &'static str,
    /// The file that keeps the group.
    pub kept: FileId,
    /// The file whose copy was dropped.
    pub dropped: FileId,
}

/// Which file keeps each COMDAT group.
#[derive(Debug, Default)]
pub struct ComdatTable<'a> {
    kept: HashMap<SymbolName<'a>, Kept, foldhash::fast::FixedState>,
    /// Conflicts found while claiming, reported after resolution.
    pub conflicts: Vec<ComdatConflict>,
}

impl<'a> ComdatTable<'a> {
    /// An empty table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Offers `comdat` from `file`, and returns whether `file` keeps it.
    ///
    /// Files must be offered in `(position, file)` order within a round, and
    /// rounds in order, which is what [`RoundHook::after_load`] guarantees.
    pub fn offer(
        &mut self,
        comdat: &Comdat<'a>,
        file: FileId,
        position: InputPosition,
        round: usize,
    ) -> bool {
        let candidate = Kept {
            selection: comdat.selection,
            length: comdat.length,
            check_sum: comdat.check_sum,
            file,
            position,
            round,
        };
        let Some(current) = self.kept.get(&comdat.key).copied() else {
            self.kept.insert(comdat.key, candidate);
            return true;
        };
        let conflict = |what: &'static str| ComdatConflict {
            key: comdat.key.bytes().to_vec(),
            what,
            kept: current.file,
            dropped: file,
        };
        let selection = current.selection.max(comdat.selection);
        let replace = match selection {
            IMAGE_COMDAT_SELECT_NODUPLICATES => {
                self.conflicts
                    .push(conflict("duplicate NODUPLICATES COMDAT section"));
                false
            }
            IMAGE_COMDAT_SELECT_SAME_SIZE => {
                if current.length != comdat.length {
                    self.conflicts
                        .push(conflict("COMDAT sections of different sizes (SAME_SIZE)"));
                }
                false
            }
            IMAGE_COMDAT_SELECT_EXACT_MATCH => {
                if current.length != comdat.length || current.check_sum != comdat.check_sum {
                    self.conflicts
                        .push(conflict("COMDAT sections that differ (EXACT_MATCH)"));
                }
                false
            }
            // Only copies of the same round compete: an earlier round's
            // definitions are already in the symbol table.
            IMAGE_COMDAT_SELECT_LARGEST => {
                current.round == round
                    && (comdat.length > current.length
                        || (comdat.length == current.length
                            && (position, file) < (current.position, current.file)))
            }
            _ => false,
        };
        if replace {
            self.kept.insert(comdat.key, candidate);
            return true;
        }
        current.file == file
    }

    /// Whether `file` keeps the group named `key`.
    #[must_use]
    pub fn keeps(&self, key: &SymbolName<'a>, file: FileId) -> bool {
        self.kept.get(key).is_some_and(|kept| kept.file == file)
    }
}

/// The per-round hook: claims COMDAT groups before the round's symbols are
/// inserted, so that definitions from discarded copies never reach the table.
#[derive(Debug, Default)]
pub struct ComdatHook<'a> {
    /// The claim table, kept for the whole resolution.
    pub table: ComdatTable<'a>,
}

impl<'a> RoundHook<CoffInput<'a>> for ComdatHook<'a> {
    fn after_load(
        &mut self,
        round: usize,
        files: &mut [RoundFile<'_, CoffInput<'a>>],
    ) -> Result<()> {
        // `files` is sorted by (position, id), so claiming sequentially is
        // deterministic.
        for entry in files.iter_mut() {
            let id = entry.id;
            let position = entry.file.position;
            let Some(parsed) = entry.file.parsed.as_mut() else {
                continue;
            };
            let comdats: Vec<Comdat<'a>> = parsed
                .sections
                .iter()
                .filter_map(|section| section.comdat)
                .filter(|comdat| comdat.selection != IMAGE_COMDAT_SELECT_ASSOCIATIVE)
                .collect();
            for comdat in &comdats {
                self.table.offer(comdat, id, position, round);
            }
        }
        for entry in files.iter_mut() {
            let id = entry.id;
            let Some(parsed) = entry.file.parsed.as_mut() else {
                continue;
            };
            let table = &self.table;
            parsed.discard_lost_comdats(&|key| table.keeps(key, id));
        }
        Ok(())
    }
}

/// Reports the COMDAT conflicts found while claiming. Returns the number of
/// errors emitted.
#[must_use]
pub fn report_conflicts(
    conflicts: &[ComdatConflict],
    files: &[CoffInput<'_>],
    diagnostics: &dyn DiagnosticSink,
) -> usize {
    let mut errors = 0usize;
    for conflict in conflicts {
        let name = |id: FileId| {
            files
                .get(id.index())
                .map_or_else(|| "<unknown>".to_string(), CoffInput::display)
        };
        let fatal = conflict.what.contains("NODUPLICATES");
        let message = format!(
            "{}: `{}` in {} and {}",
            conflict.what,
            String::from_utf8_lossy(&conflict.key),
            name(conflict.kept),
            name(conflict.dropped)
        );
        if fatal {
            diagnostics.emit(Diagnostic::error(message));
            errors = errors.saturating_add(1);
        } else {
            diagnostics.emit(Diagnostic::warning(message));
        }
    }
    errors
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coff::read::consts::IMAGE_COMDAT_SELECT_ANY;

    fn comdat(key: &'static [u8], selection: u8, length: u32) -> Comdat<'static> {
        Comdat {
            selection,
            key: SymbolName::new(key),
            associative: 0,
            check_sum: length,
            length,
        }
    }

    fn position(input: u32) -> InputPosition {
        InputPosition::new(input, 0)
    }

    #[test]
    fn any_keeps_the_first_copy() {
        let mut table = ComdatTable::new();
        let group = comdat(b"g", IMAGE_COMDAT_SELECT_ANY, 8);
        assert!(table.offer(&group, FileId::new(1), position(1), 0));
        assert!(!table.offer(&group, FileId::new(2), position(2), 0));
        assert!(table.keeps(&group.key, FileId::new(1)));
        assert!(table.conflicts.is_empty());
    }

    #[test]
    fn largest_wins_within_a_round() {
        let mut table = ComdatTable::new();
        let small = comdat(b"g", IMAGE_COMDAT_SELECT_LARGEST, 8);
        let large = comdat(b"g", IMAGE_COMDAT_SELECT_LARGEST, 16);
        table.offer(&small, FileId::new(1), position(1), 0);
        assert!(table.offer(&large, FileId::new(2), position(2), 0));
        assert!(table.keeps(&large.key, FileId::new(2)));
        // A later round does not take the group back.
        let larger = comdat(b"g", IMAGE_COMDAT_SELECT_LARGEST, 32);
        assert!(!table.offer(&larger, FileId::new(3), position(3), 1));
    }

    #[test]
    fn mismatches_are_reported() {
        let mut table = ComdatTable::new();
        table.offer(
            &comdat(b"a", IMAGE_COMDAT_SELECT_NODUPLICATES, 4),
            FileId::new(1),
            position(1),
            0,
        );
        table.offer(
            &comdat(b"a", IMAGE_COMDAT_SELECT_NODUPLICATES, 4),
            FileId::new(2),
            position(2),
            0,
        );
        table.offer(
            &comdat(b"b", IMAGE_COMDAT_SELECT_SAME_SIZE, 4),
            FileId::new(1),
            position(1),
            0,
        );
        table.offer(
            &comdat(b"b", IMAGE_COMDAT_SELECT_SAME_SIZE, 8),
            FileId::new(2),
            position(2),
            0,
        );
        table.offer(
            &comdat(b"c", IMAGE_COMDAT_SELECT_EXACT_MATCH, 4),
            FileId::new(1),
            position(1),
            0,
        );
        table.offer(
            &comdat(b"c", IMAGE_COMDAT_SELECT_EXACT_MATCH, 4),
            FileId::new(2),
            position(2),
            0,
        );
        let what: Vec<&str> = table.conflicts.iter().map(|c| c.what).collect();
        assert_eq!(what.len(), 2, "{what:?}");
        assert!(what[0].contains("NODUPLICATES"));
        assert!(what[1].contains("SAME_SIZE"));
    }

    #[test]
    fn precedence_prefers_strong_then_common_then_lazy() {
        let rules = CoffRules::default();
        let make = |kind, aux| Definition {
            kind,
            file: FileId::new(0),
            index: 0,
            position: position(1),
            aux,
        };
        let strong = make(DefinitionKind::Regular, 0);
        let common = make(DefinitionKind::Common, 4);
        let bigger = make(DefinitionKind::Common, 8);
        let lazy = make(DefinitionKind::Lazy, 0);
        assert_eq!(rules.compare(&strong, &common), Ordering::Greater);
        assert_eq!(rules.compare(&common, &lazy), Ordering::Greater);
        assert_eq!(rules.compare(&bigger, &common), Ordering::Greater);
        assert!(rules.is_duplicate(
            &strong,
            &Definition {
                file: FileId::new(1),
                ..strong
            }
        ));
    }
}
