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
//! [`callgraph`] implements `--call-graph-profile-sort` (`hfsort` and
//! `cdsort`). When both a call graph and `--symbol-ordering-file` apply,
//! the listed symbols' sections come first, then the call graph's, as in
//! lld 23.

#![deny(clippy::arithmetic_side_effects)]

use std::path::Path;

use rayon::prelude::*;

use crate::args::LinkOptions;
use crate::args::options::CallGraphSort;
use crate::diag::{Diagnostic, DiagnosticSink};
use crate::elf::read::SectionIndex;
use crate::elf::read::consts::{STT_FILE, STT_SECTION};
use crate::error::{Error, Result};
use crate::ids::{SectionId, SymbolId};
use crate::symbols::{DefinitionKind, SymbolName};

use super::arch::Arch;
use super::inputs::ElfInput;
use super::place::Placement;
use super::refs::{Def, Refs};

pub mod callgraph;
pub mod cdsort;

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

/// The section order of a link, as lld's `buildSectionOrder`: sections of
/// the call graph (`--call-graph-profile-sort`, `--call-graph-ordering-file`)
/// in the order its algorithm gives, and before them the sections of the
/// symbols of `--symbol-ordering-file`. `None` when neither applies.
///
/// # Errors
///
/// Returns [`Error::Option`] when both ordering files are given or the
/// call graph file does not parse, I/O errors for unreadable files, and
/// [`Error::Malformed`] for broken call graph profile sections.
pub fn for_link<F: crate::elf::read::ElfFormat>(
    refs: &Refs<'_, '_, F>,
    placement: &Placement<'_>,
    options: &LinkOptions,
    diagnostics: &dyn DiagnosticSink,
) -> Result<Option<SectionOrder>> {
    if options.symbol_ordering_file.is_some() && options.call_graph_ordering_file.is_some() {
        return Err(Error::Option(
            "--symbol-ordering-file and --call-graph-order-file may not be used together".into(),
        ));
    }
    let warn = !options.no_warn_symbol_ordering;
    let ignore_undefined = ignores_undefined(options);
    // The call graph is used when a sort is asked for (lld also sorts with
    // `cdsort` by default; qld keeps GNU ld's layout unless asked).
    let algorithm = match (
        options.call_graph_profile_sort,
        &options.call_graph_ordering_file,
    ) {
        (Some(algorithm), _) => algorithm,
        (None, Some(_)) => CallGraphSort::default(),
        (None, None) => CallGraphSort::None,
    };
    let mut order = SectionOrder::new(refs.sections.len());
    let mut graph_sections = 0usize;
    if algorithm != CallGraphSort::None {
        let profile = match &options.call_graph_ordering_file {
            Some(path) => callgraph::from_file(refs, path, warn, ignore_undefined, diagnostics)?,
            None => callgraph::from_objects(refs, diagnostics)?,
        };
        if !profile.is_empty() {
            let (sections, first) = callgraph::order(refs, placement, &profile, algorithm);
            let mut priority = first;
            for &id in &sections {
                order.lower(id, i32::try_from(priority).unwrap_or(i32::MIN));
                priority = priority.saturating_add(1);
            }
            graph_sections = sections.len();
            if algorithm == CallGraphSort::Hfsort
                && let Some(path) = &options.print_symbol_order
            {
                callgraph::print_symbol_order(refs, &sections, path)?;
            }
        }
    }
    if let Some(path) = &options.symbol_ordering_file {
        let names = read_symbol_ordering_file(path, warn, diagnostics)?;
        symbol_order(
            refs,
            &names,
            graph_sections,
            &mut order,
            options,
            diagnostics,
        );
    }
    Ok((!order.is_empty()).then_some(order))
}

/// Whether `--unresolved-symbols` silences undefined symbols (lld's
/// `UnresolvedPolicy::Ignore`).
fn ignores_undefined(options: &LinkOptions) -> bool {
    matches!(
        options.unresolved_symbols,
        Some(
            crate::args::UnresolvedSymbols::IgnoreAll
                | crate::args::UnresolvedSymbols::IgnoreInObjectFiles
        )
    )
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

/// Where a symbol leads for ordering.
enum Located {
    /// The (live, or folded into) section it orders.
    Section(SectionId),
    /// A symbol that cannot be ordered: the file to name in the warning.
    Unorderable(String, Unorderable),
    /// Nothing to order and nothing to report (lazy and common symbols).
    Nothing,
}

/// Where global symbol `id` leads.
fn locate_global<F: crate::elf::read::ElfFormat>(
    refs: &Refs<'_, '_, F>,
    id: SymbolId,
    ignore_undefined: bool,
) -> Located {
    let definition = refs.symbols.definition(id);
    let file = || {
        refs.files
            .get(definition.file.index())
            .map_or_else(|| "<internal>".to_string(), ElfInput::display)
    };
    match definition.kind {
        DefinitionKind::Lazy => return Located::Nothing,
        DefinitionKind::Undefined => {
            if ignore_undefined {
                return Located::Nothing;
            }
            let name = refs.symbols.name(id);
            return match first_reference(refs, name.bytes()) {
                Some(file) => Located::Unorderable(file, Unorderable::Undefined),
                None => Located::Nothing,
            };
        }
        _ => {}
    }
    match refs.global_target(id, false).def {
        Def::Section {
            file: f, section, ..
        } => match refs
            .sections
            .id(f, section)
            .and_then(|s| refs.sections.resolve(s))
        {
            Some(live) => Located::Section(live),
            None => Located::Unorderable(file(), Unorderable::Discarded),
        },
        Def::Absolute(_) => Located::Unorderable(file(), Unorderable::Absolute),
        Def::Linker(_) => Located::Unorderable("<internal>".to_string(), Unorderable::Synthetic),
        Def::Shared(_) => Located::Unorderable(file(), Unorderable::Shared),
        Def::Undefined { .. } if !ignore_undefined => {
            Located::Unorderable(file(), Unorderable::Undefined)
        }
        // Common symbols live in a linker-allocated block, which is not an
        // input section: nothing to order (and nothing to report).
        Def::Undefined { .. } | Def::Common(_) => Located::Nothing,
    }
}

/// Where local symbol `index` of `file` leads.
fn locate_local<F: crate::elf::read::ElfFormat>(
    refs: &Refs<'_, '_, F>,
    file: usize,
    index: usize,
) -> Located {
    let Some(object) = refs.files.get(file).and_then(|f| f.object.as_ref()) else {
        return Located::Nothing;
    };
    let symbols = object.elf.symbols();
    let Some(raw) = symbols.get_raw(index) else {
        return Located::Nothing;
    };
    let display = || {
        refs.files
            .get(file)
            .map_or_else(String::new, ElfInput::display)
    };
    let section = if raw.kind() == STT_FILE {
        Some(SectionIndex::Absolute)
    } else {
        symbols.section_of(index, &raw)
    };
    match section {
        Some(SectionIndex::Section(section)) => match refs
            .sections
            .id(file, section)
            .and_then(|s| refs.sections.resolve(s))
        {
            Some(live) => Located::Section(live),
            None => Located::Unorderable(display(), Unorderable::Discarded),
        },
        Some(SectionIndex::Absolute | SectionIndex::Common) => {
            Located::Unorderable(display(), Unorderable::Absolute)
        }
        _ => Located::Nothing,
    }
}

/// Why symbol `index` of `file` cannot be ordered, as `(file to name,
/// what)`, or `None` if it can.
fn unorderable<F: crate::elf::read::ElfFormat>(
    refs: &Refs<'_, '_, F>,
    file: usize,
    index: usize,
    ignore_undefined: bool,
) -> Option<(String, &'static str)> {
    let first_global = refs.files.get(file)?.object.as_ref()?.first_global;
    let located = if index < first_global {
        locate_local(refs, file, index)
    } else {
        locate_global(refs, refs.global_id(file, index)?, ignore_undefined)
    };
    match located {
        Located::Unorderable(file, why) => Some((file, why.what())),
        _ => None,
    }
}

/// Adds the sections of `--symbol-ordering-file`'s `names` to `order`,
/// ahead of the `before` sections already in it, and reports what cannot
/// be ordered.
///
/// Call it once sections are final (after `--gc-sections` and ICF).
pub fn symbol_order<F: crate::elf::read::ElfFormat>(
    refs: &Refs<'_, '_, F>,
    names: &[Vec<u8>],
    before: usize,
    order: &mut SectionOrder,
    options: &LinkOptions,
    diagnostics: &dyn DiagnosticSink,
) {
    let base = i32::try_from(names.len().saturating_add(before)).unwrap_or(i32::MAX);
    let priority_of = |index: usize| -> i32 {
        i32::try_from(index)
            .unwrap_or(i32::MAX)
            .saturating_sub(base)
    };
    let warn = !options.no_warn_symbol_ordering;
    let ignore_undefined = ignores_undefined(options);
    let mut present = vec![false; names.len()];
    let mut problems: Vec<(String, Unorderable, usize)> = Vec::new();
    let mut apply = |located: Located, index: usize, problems: &mut Vec<_>| match located {
        Located::Section(id) => order.lower(id, priority_of(index)),
        Located::Unorderable(file, why) => problems.push((file, why, index)),
        Located::Nothing => {}
    };

    // Global symbols, by name. lld looks at every symbol of its table, so
    // a name that only an unextracted archive member or an undefined
    // reference knows still counts as present.
    for (index, name) in names.iter().enumerate() {
        let Some(id) = refs.symbols.lookup(&SymbolName::new(name)) else {
            continue;
        };
        if let Some(slot) = present.get_mut(index) {
            *slot = true;
        }
        apply(
            locate_global(refs, id, ignore_undefined),
            index,
            &mut problems,
        );
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
    let locals: Vec<Vec<(usize, usize)>> = refs
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
                if !name.is_empty()
                    && let Some(&at) = lookup.get(name)
                {
                    found.push((at, index));
                }
            }
            found
        })
        .collect();
    for (file, found) in locals.iter().enumerate() {
        for &(at, index) in found {
            if let Some(slot) = present.get_mut(at) {
                *slot = true;
            }
            apply(locate_local(refs, file, index), at, &mut problems);
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
}

/// The first live file (in input order) whose global symbols include an
/// undefined `name`, for diagnostics.
fn first_reference<F: crate::elf::read::ElfFormat>(
    refs: &Refs<'_, '_, F>,
    name: &[u8],
) -> Option<String> {
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
