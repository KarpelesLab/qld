//! Symbol cross-reference diagnostics: `-y`/`--trace-symbol`,
//! `--warn-common` and `--cref`.
//!
//! All three read what resolution saw: each live file's global symbol names
//! and how it uses them, in input order.
//!
//! - `-y NAME` prints, for every live file that mentions `NAME`, whether it
//!   defines it or refers to it, in GNU ld's words (`main.o: reference to
//!   puts`), as notes.
//! - `--warn-common` replays the common symbol merges in input order and
//!   warns as GNU ld does: two commons of one size, a larger or smaller
//!   common, and a definition meeting a common.
//! - `--cref` builds GNU ld's cross-reference table, sorted by name: the
//!   defining file first, then the other files that mention the symbol.
//!   Unlike GNU ld, symbols that only shared libraries mention are left out
//!   (GNU ld lists every symbol of every library).

#![deny(clippy::arithmetic_side_effects)]

use std::fmt::Write as _;

use hashbrown::HashMap;
use rayon::prelude::*;

use crate::args::LinkOptions;
use crate::diag::{Diagnostic, DiagnosticSink};
use crate::ids::{FileId, SymbolId};
use crate::symbols::{DefinitionKind, Resolution, ResolveFile, SymbolTable, SymbolUse};

use super::inputs::{ElfInput, InputRole};

/// The live files that take part in symbol reports, with their index.
fn live_inputs<'f, 'a, F: crate::elf::read::ElfFormat>(
    files: &'f [ElfInput<'a, F>],
    resolution: &Resolution<'_>,
) -> impl Iterator<Item = (usize, &'f ElfInput<'a, F>)> {
    files.iter().enumerate().filter(|(index, file)| {
        file.role != InputRole::Internal && resolution.is_live(FileId::new(*index))
    })
}

/// Emits the `-y`/`--trace-symbol` notes.
pub fn trace_symbols<F: crate::elf::read::ElfFormat>(
    files: &[ElfInput<'_, F>],
    resolution: &Resolution<'_>,
    options: &LinkOptions,
    diagnostics: &dyn DiagnosticSink,
) {
    if options.trace_symbols.is_empty() {
        return;
    }
    for (_, file) in live_inputs(files, resolution) {
        let names = file.symbol_names();
        for (local, name) in names.iter().enumerate() {
            if name.version().is_some()
                || !options
                    .trace_symbols
                    .iter()
                    .any(|t| t.as_bytes() == name.bytes())
            {
                continue;
            }
            let what = match file.symbol_use(local) {
                SymbolUse::Reference { .. } => "reference to",
                SymbolUse::Definition { .. } => "definition of",
                _ => continue,
            };
            diagnostics.emit(
                Diagnostic::new(
                    crate::diag::Severity::Note,
                    format!("{}: {what} {}", file.display(), name.display()),
                )
                .order(file.position.raw()),
            );
        }
    }
}

/// How a file defines a symbol, for `--warn-common`.
#[derive(Clone, Copy, Debug)]
enum Held {
    Common { size: u64, file: usize },
    Defined { file: usize },
}

/// Emits the `--warn-common` warnings.
pub fn warn_common<F: crate::elf::read::ElfFormat>(
    files: &[ElfInput<'_, F>],
    resolution: &Resolution<'_>,
    options: &LinkOptions,
    diagnostics: &dyn DiagnosticSink,
) {
    if !options.warn_common {
        return;
    }
    // Symbols with a common definition somewhere.
    let mut commons: Vec<SymbolId> = files
        .par_iter()
        .enumerate()
        .filter(|(index, file)| file.object.is_some() && resolution.is_live(FileId::new(*index)))
        .flat_map_iter(|(index, file)| {
            let ids = resolution.symbol_ids(FileId::new(index));
            (0..ids.len())
                .filter(|&local| {
                    matches!(
                        file.symbol_use(local),
                        SymbolUse::Definition {
                            kind: DefinitionKind::Common,
                            ..
                        }
                    )
                })
                .filter_map(|local| ids.get(local).copied())
                .collect::<Vec<_>>()
        })
        .collect();
    if commons.is_empty() {
        return;
    }
    commons.par_sort_unstable();
    commons.dedup();
    let mut held: HashMap<SymbolId, Held, foldhash::fast::FixedState> =
        HashMap::with_hasher(foldhash::fast::FixedState::with_seed(0x636f_6d6d));
    for (index, file) in live_inputs(files, resolution) {
        let Some(object) = &file.object else {
            continue;
        };
        let ids = resolution.symbol_ids(FileId::new(index));
        for (local, &id) in ids.iter().enumerate() {
            if commons.binary_search(&id).is_err() {
                continue;
            }
            let SymbolUse::Definition { kind, aux } = file.symbol_use(local) else {
                continue;
            };
            let new = match kind {
                DefinitionKind::Common => Held::Common {
                    size: aux & !super::resolve::AUX_COMDAT,
                    file: index,
                },
                DefinitionKind::Regular | DefinitionKind::Weak => Held::Defined { file: index },
                _ => continue,
            };
            let name = object.names.get(local).map_or_else(String::new, |n| {
                String::from_utf8_lossy(n.bytes()).into_owned()
            });
            let display = |file: usize| files.get(file).map_or_else(String::new, ElfInput::display);
            let (message, keep_new) = match (held.get(&id).copied(), new) {
                (None, _) => (None, true),
                (Some(Held::Defined { .. }), Held::Defined { .. }) => (None, false),
                (Some(Held::Common { file: old, .. }), Held::Defined { file: this }) => (
                    Some(format!(
                        "{}: definition of `{name}' overriding common from {}",
                        display(this),
                        display(old)
                    )),
                    true,
                ),
                (Some(Held::Defined { file: old }), Held::Common { file: this, .. }) => (
                    Some(format!(
                        "{}: common of `{name}' overridden by definition from {}",
                        display(this),
                        display(old)
                    )),
                    false,
                ),
                (
                    Some(Held::Common {
                        size: old_size,
                        file: old,
                    }),
                    Held::Common { size, file: this },
                ) => {
                    if old_size > size {
                        (
                            Some(format!(
                                "{}: common of `{name}' overridden by larger common from {}",
                                display(this),
                                display(old)
                            )),
                            false,
                        )
                    } else if size > old_size {
                        (
                            Some(format!(
                                "{}: common of `{name}' overriding smaller common from {}",
                                display(this),
                                display(old)
                            )),
                            true,
                        )
                    } else {
                        (
                            Some(format!(
                                "{} and {}: multiple common of `{name}'",
                                display(this),
                                display(old)
                            )),
                            false,
                        )
                    }
                }
            };
            if let Some(message) = message {
                diagnostics.emit(Diagnostic::warning(message).order(file.position.raw()));
            }
            if keep_new {
                held.insert(id, new);
            }
        }
    }
}

/// Width of the symbol column of the cross-reference table.
const CREF_COLUMN: usize = 50;

/// Renders the `--cref` table, or `None` without `--cref`.
#[must_use]
pub fn cross_reference<F: crate::elf::read::ElfFormat>(
    files: &[ElfInput<'_, F>],
    symbols: &SymbolTable<'_>,
    resolution: &Resolution<'_>,
    options: &LinkOptions,
) -> Option<String> {
    if !options.cref {
        return None;
    }
    // (symbol, file) for every mention.
    let mut mentions: Vec<(SymbolId, usize, bool)> = live_inputs(files, resolution)
        .collect::<Vec<_>>()
        .into_par_iter()
        .flat_map_iter(|(index, file)| {
            let ids = resolution.symbol_ids(FileId::new(index));
            let regular = file.object.is_some();
            (0..ids.len())
                .filter(|&local| {
                    matches!(
                        file.symbol_use(local),
                        SymbolUse::Reference { .. } | SymbolUse::Definition { .. }
                    )
                })
                .filter_map(|local| ids.get(local).map(|&id| (id, index, regular)))
                .collect::<Vec<_>>()
        })
        .collect();
    mentions.par_sort_unstable();
    mentions.dedup_by_key(|(id, file, _)| (*id, *file));

    let mut rows: Vec<(&[u8], SymbolId, Vec<usize>)> = mentions
        .chunk_by(|a, b| a.0 == b.0)
        .filter(|group| group.iter().any(|&(_, _, regular)| regular))
        .filter_map(|group| {
            let id = group.first()?.0;
            let name = symbols.name(id);
            if name.version().is_some() {
                return None;
            }
            let definer = symbols.definition(id);
            let defined = !matches!(
                definer.kind,
                DefinitionKind::Undefined | DefinitionKind::Lazy
            );
            let mut list: Vec<usize> = Vec::with_capacity(group.len());
            if defined && group.iter().any(|&(_, f, _)| f == definer.file.index()) {
                list.push(definer.file.index());
            }
            list.extend(
                group
                    .iter()
                    .map(|&(_, f, _)| f)
                    .filter(|&f| !(defined && f == definer.file.index())),
            );
            Some((name.bytes(), id, list))
        })
        .collect();
    rows.par_sort_unstable_by(|a, b| a.0.cmp(b.0).then(a.1.cmp(&b.1)));

    let mut text = String::from("\nCross Reference Table\n\nSymbol");
    text.push_str(&" ".repeat(CREF_COLUMN.saturating_sub(6)));
    text.push_str("File\n");
    for (name, _, list) in rows {
        let name = String::from_utf8_lossy(name);
        for (position, file) in list.iter().enumerate() {
            let display = files.get(*file).map_or_else(String::new, ElfInput::display);
            if position == 0 {
                text.push_str(&name);
                if name.len() >= CREF_COLUMN {
                    text.push('\n');
                    text.push_str(&" ".repeat(CREF_COLUMN));
                } else {
                    text.push_str(&" ".repeat(CREF_COLUMN.saturating_sub(name.len())));
                }
            } else {
                text.push_str(&" ".repeat(CREF_COLUMN));
            }
            let _ = writeln!(text, "{display}");
        }
    }
    Some(text)
}
