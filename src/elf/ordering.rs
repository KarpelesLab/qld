//! Section ordering within output sections: `--symbol-ordering-file` and
//! `--call-graph-profile-sort` (lld semantics).
//!
//! Both produce a [`SectionOrder`]: a priority per input section, negative
//! for the sections to put first (lower first) and 0 for the rest. Layout
//! then moves the prioritized sections of each output section to its
//! start, in priority order, and keeps everything else in its usual order
//! (lld's `sortISDBySectionOrder`):
//!
//! - with the default rules, over the whole output section (lld has one
//!   input description per output section), except that `.init` and
//!   `.fini` are never reordered and sections sorted by init priority keep
//!   that order first;
//! - with a linker script, within each input description;
//! - on targets with range-extension thunks, when an executable output
//!   section is larger than the thunk spacing, the ordered sections go in
//!   the middle of the unordered ones rather than first, so that more code
//!   reaches them without a thunk.
//!
//! [`symbol_order`] reads `--symbol-ordering-file`: every section that
//! defines a listed symbol (global or local) gets the priority of the
//! earliest-listed such symbol. Symbols that cannot be ordered (undefined,
//! shared, absolute, linker-defined, or in a discarded section) and names
//! that match nothing are reported as warnings, unless
//! `--no-warn-symbol-ordering`, with lld's wording. Symbols in sections ICF
//! folded order the section they were folded into.
//!
//! `--call-graph-profile-sort` is to follow.

#![deny(clippy::arithmetic_side_effects)]

use std::path::Path;

use rayon::prelude::*;

use crate::args::LinkOptions;
use crate::diag::{Diagnostic, DiagnosticSink};
use crate::elf::read::SectionIndex;
use crate::elf::read::consts::{STT_FILE, STT_SECTION};
use crate::error::{Error, Result};
use crate::ids::SectionId;
use crate::symbols::{DefinitionKind, SymbolName};

use super::arch::Arch;
use super::refs::{Def, Refs};

/// A priority for input sections: negative values go first.
#[derive(Clone, Debug, Default)]
pub struct SectionOrder {
    /// By section ID; 0 for sections without a priority.
    priority: Vec<i32>,
    /// Whether any section has a priority.
    any: bool,
}

impl SectionOrder {
    /// An order for `count` sections, all unprioritized.
    #[must_use]
    pub fn new(count: usize) -> Self {
        Self {
            priority: vec![0; count],
            any: false,
        }
    }

    /// The priority of `id` (0: none).
    #[inline]
    #[must_use]
    pub fn priority(&self, id: SectionId) -> i32 {
        self.priority.get(id.index()).copied().unwrap_or(0)
    }

    /// Whether no section has a priority.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        !self.any
    }

    /// Lowers the priority of `id` to `priority` if that is smaller.
    pub fn lower(&mut self, id: SectionId, priority: i32) {
        if let Some(slot) = self.priority.get_mut(id.index())
            && priority < *slot
        {
            *slot = priority;
            self.any = true;
        }
    }

    /// Compares two members of a linker-script input description for
    /// ordering: `Some(id)` for input sections, `None` for everything else
    /// (which is never prioritized).
    #[must_use]
    pub fn compare(&self, a: Option<SectionId>, b: Option<SectionId>) -> core::cmp::Ordering {
        let key = |id: Option<SectionId>| id.map_or(0, |id| self.priority(id));
        key(a).cmp(&key(b))
    }

    /// Reorders `run`, one input description's members in their usual
    /// order: prioritized sections move to the front by priority (stable),
    /// or to the middle of the others when `spacing` is non-zero and the
    /// run's total size reaches it (see the module documentation).
    ///
    /// `id` gives the input section of a member, `size` its size (only
    /// called when `spacing` is non-zero).
    pub fn arrange<T: Copy>(
        &self,
        run: &mut [T],
        id: impl Fn(&T) -> Option<SectionId>,
        spacing: u64,
        size: impl Fn(&T) -> u64,
    ) {
        let priority = |item: &T| id(item).map_or(0, |id| self.priority(id));
        if run.iter().all(|item| priority(item) == 0) {
            return;
        }
        let mut ordered: Vec<(i32, T)> = Vec::new();
        let mut unordered: Vec<T> = Vec::with_capacity(run.len());
        for item in run.iter() {
            match priority(item) {
                0 => unordered.push(*item),
                p => ordered.push((p, *item)),
            }
        }
        ordered.sort_by_key(|&(p, _)| p);
        let mut at = 0usize;
        if spacing > 0 {
            let unordered_size: u64 = unordered.iter().map(&size).fold(0, u64::saturating_add);
            let total = ordered
                .iter()
                .map(|(_, item)| size(item))
                .fold(unordered_size, u64::saturating_add);
            if total >= spacing {
                let mut position = 0u64;
                while let Some(item) = unordered.get(at) {
                    position = position.saturating_add(size(item));
                    if position > unordered_size / 2 {
                        break;
                    }
                    at = at.saturating_add(1);
                }
            }
        }
        let (before, after) = unordered.split_at(at.min(unordered.len()));
        let arranged = before
            .iter()
            .copied()
            .chain(ordered.into_iter().map(|(_, item)| item))
            .chain(after.iter().copied());
        for (slot, item) in run.iter_mut().zip(arranged) {
            *slot = item;
        }
    }
}

/// lld's thunk section spacing for `arch`: the size of executable output
/// sections past which ordered sections go in the middle (0: never).
#[must_use]
pub fn thunk_spacing(arch: Arch) -> u64 {
    if arch.needs_thunks() {
        // AArch64: the reach of a 26-bit branch, less some slack.
        (1u64 << 27).saturating_sub(0x3_0000)
    } else {
        0
    }
}

/// Whether sections of the output section `name` may be reordered.
#[must_use]
pub fn reorders(name: &[u8]) -> bool {
    name != b".init" && name != b".fini"
}

/// Reads a symbol ordering file: one symbol per line, surrounding white
/// space trimmed, empty lines and lines starting with `#` skipped. A
/// symbol listed twice keeps its first position (with a warning).
///
/// # Errors
///
/// Returns [`Error::Io`] if the file cannot be read.
pub fn read_symbol_ordering_file(
    path: &Path,
    warn: bool,
    diagnostics: &dyn DiagnosticSink,
) -> Result<Vec<Vec<u8>>> {
    let text = std::fs::read(path).map_err(|e| Error::io(path, e))?;
    let mut seen: hashbrown::HashSet<&[u8], foldhash::fast::FixedState> =
        hashbrown::HashSet::with_hasher(foldhash::fast::FixedState::default());
    let mut names = Vec::new();
    for line in text.split(|&b| b == b'\n') {
        let line = line.trim_ascii();
        if line.is_empty() || line.first() == Some(&b'#') {
            continue;
        }
        if !seen.insert(line) {
            if warn {
                diagnostics.emit(Diagnostic::warning(format!(
                    "{}: duplicate ordered symbol: {}",
                    path.display(),
                    String::from_utf8_lossy(line)
                )));
            }
            continue;
        }
        names.push(line.to_vec());
    }
    Ok(names)
}

/// The section order of a link: from `--symbol-ordering-file`, else from
/// the call graph (`--call-graph-profile-sort`,
/// `--call-graph-ordering-file`), else none.
///
/// # Errors
///
/// Returns [`Error::Option`] when both ordering files are given, and I/O
/// errors for unreadable files.
pub fn for_link(
    refs: &Refs<'_, '_>,
    options: &LinkOptions,
    diagnostics: &dyn DiagnosticSink,
) -> Result<Option<SectionOrder>> {
    if let Some(path) = &options.symbol_ordering_file {
        if options.call_graph_ordering_file.is_some() {
            return Err(Error::Option(
                "--symbol-ordering-file and --call-graph-order-file may not be used together"
                    .into(),
            ));
        }
        let names = read_symbol_ordering_file(path, !options.no_warn_symbol_ordering, diagnostics)?;
        return Ok(Some(symbol_order(refs, &names, options, diagnostics)));
    }
    Ok(None)
}

/// Why a listed symbol cannot order a section.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Unorderable {
    Undefined,
    Shared,
    Absolute,
    Synthetic,
    Discarded,
}

impl Unorderable {
    fn what(self) -> &'static str {
        match self {
            Self::Undefined => "undefined",
            Self::Shared => "shared",
            Self::Absolute => "absolute",
            Self::Synthetic => "synthetic",
            Self::Discarded => "discarded",
        }
    }
}

/// Builds the section order of `--symbol-ordering-file` from its `names`.
///
/// Call it once sections are final (after `--gc-sections` and ICF).
#[must_use]
pub fn symbol_order(
    refs: &Refs<'_, '_>,
    names: &[Vec<u8>],
    options: &LinkOptions,
    diagnostics: &dyn DiagnosticSink,
) -> SectionOrder {
    let mut order = SectionOrder::new(refs.sections.len());
    let count = i32::try_from(names.len()).unwrap_or(i32::MAX);
    let priority_of = |index: usize| -> i32 {
        i32::try_from(index)
            .unwrap_or(i32::MAX)
            .saturating_sub(count)
    };
    let warn = !options.no_warn_symbol_ordering;
    let ignore_undefined = matches!(
        options.unresolved_symbols,
        Some(
            crate::args::UnresolvedSymbols::IgnoreAll
                | crate::args::UnresolvedSymbols::IgnoreInObjectFiles
        )
    );
    let mut present = vec![false; names.len()];
    let mut problems: Vec<(String, Unorderable, usize)> = Vec::new();

    // Global symbols, by name.
    for (index, name) in names.iter().enumerate() {
        let Some(id) = refs.symbols.lookup(&SymbolName::new(name)) else {
            continue;
        };
        let definition = refs.symbols.definition(id);
        match definition.kind {
            // lld looks at every symbol of its table, so a name that only
            // an unextracted archive member or an undefined reference
            // knows still counts as present.
            DefinitionKind::Lazy => {
                if let Some(slot) = present.get_mut(index) {
                    *slot = true;
                }
                continue;
            }
            DefinitionKind::Undefined => {
                // Only names something refers to are in the table.
                if let Some(slot) = present.get_mut(index) {
                    *slot = true;
                }
                if !ignore_undefined && let Some(file) = first_reference(refs, name) {
                    problems.push((file, Unorderable::Undefined, index));
                }
                continue;
            }
            _ => {}
        }
        if let Some(slot) = present.get_mut(index) {
            *slot = true;
        }
        let file = || {
            refs.files.get(definition.file.index()).map_or_else(
                || "<internal>".to_string(),
                super::inputs::ElfInput::display,
            )
        };
        let target = refs.global_target(id, false);
        match target.def {
            Def::Section {
                file: f, section, ..
            } => match refs
                .sections
                .id(f, section)
                .and_then(|s| refs.sections.resolve(s))
            {
                Some(live) => order.lower(live, priority_of(index)),
                None => problems.push((file(), Unorderable::Discarded, index)),
            },
            Def::Absolute(_) => problems.push((file(), Unorderable::Absolute, index)),
            Def::Linker(_) => {
                problems.push(("<internal>".to_string(), Unorderable::Synthetic, index));
            }
            Def::Shared(_) => problems.push((file(), Unorderable::Shared, index)),
            Def::Undefined { .. } => {
                if !ignore_undefined {
                    problems.push((file(), Unorderable::Undefined, index));
                }
            }
            // Common symbols live in a linker-allocated block, which is not
            // an input section: nothing to order (and nothing to report).
            Def::Common(_) => {}
        }
    }

    // Local symbols, found per file in parallel.
    let mut lookup: hashbrown::HashMap<&[u8], usize, foldhash::fast::FixedState> =
        hashbrown::HashMap::with_capacity_and_hasher(
            names.len(),
            foldhash::fast::FixedState::default(),
        );
    for (index, name) in names.iter().enumerate() {
        lookup.insert(name.as_slice(), index);
    }
    let locals: Vec<Vec<(usize, Option<SectionIndex>, u8)>> = refs
        .files
        .par_iter()
        .enumerate()
        .map(|(file, input)| {
            let mut found = Vec::new();
            let Some(object) = &input.object else {
                return found;
            };
            if !refs.resolution.is_live(crate::ids::FileId::new(file)) {
                return found;
            }
            let symbols = object.elf.symbols();
            for index in 1..object.first_global.min(symbols.len()) {
                let Some(raw) = symbols.get_raw(index) else {
                    continue;
                };
                if raw.kind() == STT_SECTION {
                    continue;
                }
                let Ok(name) = symbols.name(index, &raw) else {
                    continue;
                };
                if name.is_empty() {
                    continue;
                }
                if let Some(&at) = lookup.get(name) {
                    let section = if raw.kind() == STT_FILE {
                        Some(SectionIndex::Absolute)
                    } else {
                        symbols.section_of(index, &raw)
                    };
                    found.push((at, section, raw.kind()));
                }
            }
            found
        })
        .collect();
    for (file, found) in locals.iter().enumerate() {
        for &(index, section, _) in found {
            if let Some(slot) = present.get_mut(index) {
                *slot = true;
            }
            let display = || {
                refs.files
                    .get(file)
                    .map_or_else(String::new, super::inputs::ElfInput::display)
            };
            match section {
                Some(SectionIndex::Section(section)) => match refs
                    .sections
                    .id(file, section)
                    .and_then(|s| refs.sections.resolve(s))
                {
                    Some(live) => order.lower(live, priority_of(index)),
                    None => problems.push((display(), Unorderable::Discarded, index)),
                },
                Some(SectionIndex::Absolute | SectionIndex::Common) => {
                    problems.push((display(), Unorderable::Absolute, index));
                }
                _ => {}
            }
        }
    }

    if warn {
        for (file, why, index) in &problems {
            let name = names.get(*index).map_or(&b""[..], Vec::as_slice);
            diagnostics.emit(Diagnostic::warning(format!(
                "{file}: unable to order {} symbol: {}",
                why.what(),
                String::from_utf8_lossy(name)
            )));
        }
        for (name, present) in names.iter().zip(&present) {
            if !present {
                diagnostics.emit(Diagnostic::warning(format!(
                    "symbol ordering file: no such symbol: {}",
                    String::from_utf8_lossy(name)
                )));
            }
        }
    }
    order
}

/// The first live file (in input order) whose global symbols include an
/// undefined `name`, for diagnostics.
fn first_reference(refs: &Refs<'_, '_>, name: &[u8]) -> Option<String> {
    refs.files.iter().enumerate().find_map(|(file, input)| {
        let object = input.object.as_ref()?;
        if !refs.resolution.is_live(crate::ids::FileId::new(file)) {
            return None;
        }
        object
            .names
            .iter()
            .any(|n| n.bytes() == name)
            .then(|| input.display())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arrange_moves_ordered_first_and_keeps_the_rest() {
        let mut order = SectionOrder::new(8);
        order.lower(SectionId::from_u32(5), -3);
        order.lower(SectionId::from_u32(2), -2);
        order.lower(SectionId::from_u32(2), -1);
        let mut run: Vec<u32> = (0..8).collect();
        order.arrange(&mut run, |&i| Some(SectionId::from_u32(i)), 0, |_| 0);
        assert_eq!(run, [5, 2, 0, 1, 3, 4, 6, 7]);
    }

    #[test]
    fn arrange_uses_the_middle_past_the_thunk_spacing() {
        let mut order = SectionOrder::new(6);
        order.lower(SectionId::from_u32(4), -1);
        let mut run: Vec<u32> = (0..6).collect();
        // Five unordered sections of 10 bytes: the ordered one goes after
        // the third (30 > 50 / 2).
        order.arrange(&mut run, |&i| Some(SectionId::from_u32(i)), 10, |_| 10);
        assert_eq!(run, [0, 1, 4, 2, 3, 5]);
        let mut run: Vec<u32> = (0..6).collect();
        order.arrange(&mut run, |&i| Some(SectionId::from_u32(i)), 1000, |_| 10);
        assert_eq!(run, [4, 0, 1, 2, 3, 5]);
    }
}
