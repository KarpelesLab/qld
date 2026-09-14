//! Identical code folding (`--icf=all`, `--icf=safe`).
//!
//! # Input
//!
//! The backend describes every input section as an [`IcfSection`] (contents,
//! a key for the properties that must match, and eligibility flags) and lists
//! each section's relocations, in a fixed order, as [`IcfReloc`]s in a
//! [`Csr`]. Relocation targets inside sections are given as a section plus an
//! offset, so the pass can compare them by equivalence class. Anything else
//! (undefined or absolute symbols, merged-string pieces, ...) is compared by
//! identity.
//!
//! # Algorithm
//!
//! Like lld, the pass computes the coarsest partition of the eligible
//! sections in which two sections share a class only if their constant parts
//! (key, contents, relocation offsets, types, addends and target offsets) are
//! equal and each pair of corresponding relocation targets is in the same
//! class. This is a greatest fixed point, so mutually recursive functions
//! fold correctly.
//!
//! 1. Hash the constant part of every eligible section in parallel, sort by
//!    `(hash, id)`, and split equal-hash runs into classes by exact
//!    comparison.
//! 2. Each round rehashes every section still in play from the current class
//!    ids of its relocation targets (reading a snapshot: class ids are only
//!    written after all comparisons of the round), then splits each class by
//!    hash and exact comparison of target classes.
//! 3. Stop when a round splits no class.
//!
//! A class's id is its lowest `SectionId`, which is also the representative
//! the other members fold into. Singleton classes, and classes whose members
//! have no section-relative relocations, can never split again and leave the
//! working set. Neither class ids nor results depend on hash values or thread
//! count.
//!
//! A run of `k` sections with equal hashes but different contents (a hash
//! collision, possibly adversarial) is split by a comparison sort, so the
//! worst case is `O(k log k)` comparisons rather than quadratic.
//!
//! Each round costs `O(W log W)` for `W` sections still in play. The number
//! of rounds is bounded by the length of the longest chain of references that
//! tells two otherwise identical sections apart; in practice it is small.

use std::cmp::Ordering as CmpOrdering;
use std::hash::Hasher;
use std::sync::atomic::{AtomicU32, Ordering};

use rayon::prelude::*;

use super::csr::{Csr, InputError};
use super::hash::hasher;
use crate::ids::{SectionId, SymbolId};

/// Slices longer than this are sorted or scanned in parallel.
const PARALLEL_THRESHOLD: usize = 4096;

/// Which sections ICF may fold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IcfMode {
    /// `--icf=all`: fold every foldable section, even if its address is
    /// compared somewhere.
    All,
    /// `--icf=safe`: fold only foldable sections whose address is not
    /// significant.
    Safe,
}

/// One input section, as ICF sees it.
#[derive(Clone, Copy, Debug)]
pub struct IcfSection<'a> {
    /// The section's bytes (zero-copy from the input mapping). For formats
    /// with implicit addends these include the addends.
    pub contents: &'a [u8],
    /// Everything else that must match for two sections to fold: type, flags,
    /// alignment, entry size, and the size of sections without file contents.
    /// Sections with different keys never fold, so the backend must make the
    /// key *exact* (for example an index into a table of interned tuples),
    /// not a hash.
    pub key: u64,
    /// Whether the section may be folded at all. Backends clear this for dead
    /// sections, writable data, sections with unusual relocations, and so on.
    /// A non-foldable section can still be a relocation target; it is its own
    /// class.
    pub foldable: bool,
    /// Whether the section's address is significant (`.llvm_addrsig`, or no
    /// such table and the section is address-taken). [`IcfMode::Safe`] does
    /// not fold these.
    pub address_significant: bool,
}

/// What a relocation points at.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum IcfTarget {
    /// A location inside an input section: the defining section of the
    /// target symbol, and the symbol's offset in it. Compared by the
    /// section's equivalence class plus `offset`.
    Section {
        /// The section containing the target.
        section: SectionId,
        /// Offset of the target within that section.
        offset: u64,
    },
    /// A symbol not defined in a section that participates (undefined,
    /// shared, absolute...). Compared by identity.
    Symbol(SymbolId),
    /// Any other target, compared by value (for example, the output
    /// location of a merged-string piece). The backend chooses the encoding.
    Value(u64),
}

/// One relocation of a section, as ICF sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IcfReloc {
    /// Offset of the relocated field within the section.
    pub offset: u64,
    /// Relocation type, format-specific.
    pub kind: u32,
    /// Explicit addend (0 for formats with implicit addends).
    pub addend: i64,
    /// The relocation target.
    pub target: IcfTarget,
}

/// Validated ICF input: sections plus their relocations.
#[derive(Clone, Debug)]
pub struct IcfInput<'a> {
    sections: Vec<IcfSection<'a>>,
    relocs: Csr<IcfReloc>,
}

impl<'a> IcfInput<'a> {
    /// Combines per-section data (indexed by [`SectionId`]) with a relocation
    /// table whose row `i` lists section `i`'s relocations in order.
    ///
    /// # Errors
    ///
    /// Returns [`InputError::RowCount`] if the tables disagree on the number
    /// of sections, [`InputError::OutOfRange`] if a relocation targets a
    /// section that does not exist, or [`InputError::TooLarge`] if there are
    /// more sections than a [`SectionId`] can number.
    pub fn new(sections: Vec<IcfSection<'a>>, relocs: Csr<IcfReloc>) -> Result<Self, InputError> {
        let len = sections.len();
        if u32::try_from(len).is_err() {
            return Err(InputError::TooLarge("section count"));
        }
        if relocs.rows() != len {
            return Err(InputError::RowCount {
                expected: len,
                found: relocs.rows(),
            });
        }
        let bad = relocs
            .values()
            .par_iter()
            .find_map_first(|reloc| match reloc.target {
                IcfTarget::Section { section, .. } if section.index() >= len => Some(section),
                _ => None,
            });
        if let Some(section) = bad {
            return Err(InputError::OutOfRange {
                what: "relocation target section",
                index: u64::from(section.as_u32()),
                len,
            });
        }
        Ok(Self { sections, relocs })
    }

    /// Number of sections.
    #[must_use]
    pub fn len(&self) -> usize {
        self.sections.len()
    }

    /// Returns `true` if there are no sections.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sections.is_empty()
    }

    /// The sections.
    #[must_use]
    pub fn sections(&self) -> &[IcfSection<'a>] {
        &self.sections
    }

    /// The relocations of `section`.
    #[must_use]
    pub fn relocs(&self, section: SectionId) -> &[IcfReloc] {
        self.relocs.row(section.index())
    }
}

/// A working-set entry: one eligible section.
#[derive(Clone, Copy, Debug)]
struct Slot {
    hash: u64,
    id: u32,
    /// First slot of its class (or, transiently, of its hash run).
    start: bool,
    /// Its class is final; remove it from the working set.
    done: bool,
}

struct Folder<'i, 'a> {
    input: &'i IcfInput<'a>,
    /// Class id of every section: the lowest member's index. Non-eligible
    /// sections keep their own index. Written only between comparison phases.
    class: Vec<AtomicU32>,
}

impl Folder<'_, '_> {
    fn section(&self, id: u32) -> &IcfSection<'_> {
        &self.input.sections[id as usize]
    }

    fn relocs(&self, id: u32) -> &[IcfReloc] {
        self.input.relocs.row(id as usize)
    }

    fn class_of(&self, section: SectionId) -> u32 {
        self.class[section.index()].load(Ordering::Relaxed)
    }

    fn constant_hash(&self, id: u32) -> u64 {
        let section = self.section(id);
        let relocs = self.relocs(id);
        let mut h = hasher();
        h.write_u64(section.key);
        h.write_usize(section.contents.len());
        h.write(section.contents);
        h.write_usize(relocs.len());
        for reloc in relocs {
            let (tag, value) = target_shape(reloc.target);
            h.write_u64(reloc.offset);
            h.write_u32(reloc.kind);
            h.write_i64(reloc.addend);
            h.write_u8(tag);
            h.write_u64(value);
        }
        h.finish()
    }

    fn compare_constant(&self, a: u32, b: u32) -> CmpOrdering {
        let (sa, sb) = (self.section(a), self.section(b));
        let (ra, rb) = (self.relocs(a), self.relocs(b));
        sa.key
            .cmp(&sb.key)
            .then_with(|| sa.contents.len().cmp(&sb.contents.len()))
            .then_with(|| ra.len().cmp(&rb.len()))
            .then_with(|| sa.contents.cmp(sb.contents))
            .then_with(|| {
                let key = |r: &IcfReloc| {
                    let (tag, value) = target_shape(r.target);
                    (r.offset, r.kind, r.addend, tag, value)
                };
                ra.iter().map(key).cmp(rb.iter().map(key))
            })
    }

    fn variable_hash(&self, id: u32) -> u64 {
        let mut h = hasher();
        for reloc in self.relocs(id) {
            if let IcfTarget::Section { section, .. } = reloc.target {
                h.write_u32(self.class_of(section));
            }
        }
        h.finish()
    }

    /// Compares target classes. Only called on sections of the same class,
    /// which therefore have identical relocation shapes.
    fn compare_variable(&self, a: u32, b: u32) -> CmpOrdering {
        let key = |reloc: &IcfReloc| match reloc.target {
            IcfTarget::Section { section, .. } => self.class_of(section),
            _ => 0,
        };
        self.relocs(a)
            .iter()
            .map(key)
            .cmp(self.relocs(b).iter().map(key))
    }

    fn has_section_targets(&self, id: u32) -> bool {
        self.relocs(id)
            .iter()
            .any(|reloc| matches!(reloc.target, IcfTarget::Section { .. }))
    }

    /// Splits one run of equal-hash slots, sorted by id, into classes by
    /// exact comparison, setting `start` on each class's first slot. Classes
    /// come out with ascending ids, so the first slot is the lowest.
    fn split_run(&self, run: &mut [Slot], compare: &(impl Fn(u32, u32) -> CmpOrdering + Sync)) {
        let Some(first) = run.first().map(|slot| slot.id) else {
            return;
        };
        for slot in run.iter_mut() {
            slot.start = false;
        }
        run[0].start = true;
        let equal_to_first = |slot: &Slot| compare(first, slot.id) == CmpOrdering::Equal;
        let all_equal = if run.len() > PARALLEL_THRESHOLD {
            run[1..].par_iter().all(equal_to_first)
        } else {
            run[1..].iter().all(equal_to_first)
        };
        if all_equal {
            return;
        }
        let order = |x: &Slot, y: &Slot| compare(x.id, y.id).then(x.id.cmp(&y.id));
        if run.len() > PARALLEL_THRESHOLD {
            run.par_sort_unstable_by(order);
        } else {
            run.sort_unstable_by(order);
        }
        for index in 1..run.len() {
            run[index].start = compare(run[index - 1].id, run[index].id) != CmpOrdering::Equal;
        }
        run[0].start = true;
    }

    /// Sorts a class by `(hash, id)` and splits it by hash runs and exact
    /// comparison.
    fn split_class(&self, class: &mut [Slot], compare: &(impl Fn(u32, u32) -> CmpOrdering + Sync)) {
        if class.len() > PARALLEL_THRESHOLD {
            class.par_sort_unstable_by_key(|slot| (slot.hash, slot.id));
        } else {
            class.sort_unstable_by_key(|slot| (slot.hash, slot.id));
        }
        let mut rest = class;
        while let Some(hash) = rest.first().map(|slot| slot.hash) {
            let len = rest.iter().take_while(|slot| slot.hash == hash).count();
            let (run, tail) = rest.split_at_mut(len);
            self.split_run(run, compare);
            rest = tail;
        }
    }

    /// Writes each class's id to its members, marks final classes done, and
    /// removes them from the working set.
    fn commit(&self, work: &mut Vec<Slot>) {
        work.par_chunk_by_mut(|_, next| !next.start)
            .for_each(|class| {
                let representative = class[0].id;
                for slot in class.iter() {
                    self.class[slot.id as usize].store(representative, Ordering::Relaxed);
                }
                if class.len() == 1 || !self.has_section_targets(representative) {
                    for slot in class.iter_mut() {
                        slot.done = true;
                    }
                }
            });
        work.retain(|slot| !slot.done);
    }
}

fn target_shape(target: IcfTarget) -> (u8, u64) {
    match target {
        IcfTarget::Section { offset, .. } => (0, offset),
        IcfTarget::Symbol(symbol) => (1, u64::from(symbol.as_u32())),
        IcfTarget::Value(value) => (2, value),
    }
}

fn count_classes(work: &[Slot]) -> usize {
    work.par_iter().filter(|slot| slot.start).count()
}

/// Runs identical code folding over `input`.
///
/// Must run inside the caller's rayon pool. The result is the same for any
/// thread count.
#[must_use]
pub fn fold_identical(input: &IcfInput<'_>, mode: IcfMode) -> IcfResult {
    let len = input.len();
    // `IcfInput::new` guarantees the count fits in u32.
    let folder = Folder {
        input,
        class: (0..len)
            .into_par_iter()
            .map(|index| AtomicU32::new(index as u32))
            .collect(),
    };

    // Round 0: classes of equal constant parts.
    let mut work: Vec<Slot> = input
        .sections
        .par_iter()
        .enumerate()
        .filter(|(_, section)| {
            section.foldable && (mode == IcfMode::All || !section.address_significant)
        })
        .map(|(index, _)| {
            let id = index as u32;
            Slot {
                hash: folder.constant_hash(id),
                id,
                start: false,
                done: false,
            }
        })
        .collect();
    work.par_sort_unstable_by_key(|slot| (slot.hash, slot.id));
    let mut previous = None;
    for slot in &mut work {
        slot.start = previous != Some(slot.hash);
        previous = Some(slot.hash);
    }
    let constant = |a, b| folder.compare_constant(a, b);
    work.par_chunk_by_mut(|_, next| !next.start)
        .for_each(|run| folder.split_run(run, &constant));
    folder.commit(&mut work);

    // Refinement rounds.
    let mut rounds = 1;
    while !work.is_empty() {
        rounds += 1;
        let before = count_classes(&work);
        work.par_iter_mut()
            .for_each(|slot| slot.hash = folder.variable_hash(slot.id));
        let variable = |a, b| folder.compare_variable(a, b);
        work.par_chunk_by_mut(|_, next| !next.start)
            .for_each(|class| folder.split_class(class, &variable));
        let after = count_classes(&work);
        folder.commit(&mut work);
        if after == before {
            break;
        }
    }

    IcfResult {
        fold_into: folder
            .class
            .into_iter()
            .map(|class| SectionId::from_u32(class.into_inner()))
            .collect(),
        rounds,
    }
}

/// The outcome of identical code folding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IcfResult {
    fold_into: Vec<SectionId>,
    rounds: usize,
}

impl IcfResult {
    /// For each section, the section it folds into: itself if kept, or the
    /// lowest-numbered section of its class.
    #[must_use]
    pub fn fold_into(&self) -> &[SectionId] {
        &self.fold_into
    }

    /// The section `section` folds into (itself if it is kept or out of
    /// range).
    #[must_use]
    pub fn representative(&self, section: SectionId) -> SectionId {
        self.fold_into
            .get(section.index())
            .copied()
            .unwrap_or(section)
    }

    /// Whether `section` is folded into another section.
    #[must_use]
    pub fn is_folded(&self, section: SectionId) -> bool {
        self.representative(section) != section
    }

    /// Number of sections folded away.
    #[must_use]
    pub fn num_folded(&self) -> usize {
        self.fold_into
            .par_iter()
            .enumerate()
            .filter(|&(index, rep)| rep.index() != index)
            .count()
    }

    /// Number of hashing rounds run, including the initial one.
    #[must_use]
    pub fn rounds(&self) -> usize {
        self.rounds
    }

    /// The folds, grouped by kept section, for `--print-icf-sections`.
    #[must_use]
    pub fn report(&self) -> IcfReport {
        let mut pairs: Vec<(SectionId, SectionId)> = self
            .fold_into
            .iter()
            .enumerate()
            .filter(|&(index, rep)| rep.index() != index)
            .map(|(index, &rep)| (rep, SectionId::new(index)))
            .collect();
        // Stable: members stay in input order within each group.
        pairs.sort_by_key(|&(rep, _)| rep);
        let mut report = IcfReport {
            kept: Vec::new(),
            bounds: vec![0],
            folded: Vec::with_capacity(pairs.len()),
        };
        for (rep, member) in pairs {
            if report.kept.last() != Some(&rep) {
                if !report.kept.is_empty() {
                    report.bounds.push(report.folded.len());
                }
                report.kept.push(rep);
            }
            report.folded.push(member);
        }
        if !report.kept.is_empty() {
            report.bounds.push(report.folded.len());
        }
        report
    }
}

/// Folded sections grouped by the section they fold into, sorted by kept
/// section and then by folded section.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IcfReport {
    kept: Vec<SectionId>,
    /// `bounds[i]..bounds[i + 1]` is group `i`'s range in `folded`.
    bounds: Vec<usize>,
    folded: Vec<SectionId>,
}

/// One group of an [`IcfReport`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IcfGroup<'r> {
    /// The section kept (`selected section` in lld's output).
    pub kept: SectionId,
    /// The sections folded into it (`removing identical section`).
    pub folded: &'r [SectionId],
}

impl IcfReport {
    /// Number of groups.
    #[must_use]
    pub fn len(&self) -> usize {
        self.kept.len()
    }

    /// Returns `true` if nothing was folded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.kept.is_empty()
    }

    /// Iterates over the groups in order.
    pub fn groups(&self) -> impl Iterator<Item = IcfGroup<'_>> + '_ {
        self.kept.iter().enumerate().map(|(index, &kept)| IcfGroup {
            kept,
            folded: self
                .folded
                .get(self.bounds[index]..self.bounds[index + 1])
                .unwrap_or(&[]),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::csr::CsrBuilder;
    use super::*;

    fn id(index: usize) -> SectionId {
        SectionId::new(index)
    }

    fn call(section: usize) -> IcfReloc {
        IcfReloc {
            offset: 1,
            kind: 4,
            addend: -4,
            target: IcfTarget::Section {
                section: id(section),
                offset: 0,
            },
        }
    }

    fn section(contents: &[u8]) -> IcfSection<'_> {
        IcfSection {
            contents,
            key: 1,
            foldable: true,
            address_significant: false,
        }
    }

    fn run(
        sections: Vec<IcfSection<'_>>,
        relocs: &[(usize, IcfReloc)],
        mode: IcfMode,
    ) -> IcfResult {
        let mut builder = CsrBuilder::new(sections.len());
        for &(row, reloc) in relocs {
            builder.push(row, reloc);
        }
        let input = IcfInput::new(sections, builder.build().unwrap()).unwrap();
        fold_identical(&input, mode)
    }

    #[test]
    fn folds_identical_leaves() {
        let code = b"\x55\xc3";
        let result = run(
            vec![
                section(code),
                section(b"\x90"),
                section(code),
                section(code),
            ],
            &[],
            IcfMode::All,
        );
        assert_eq!(result.fold_into(), &[id(0), id(1), id(0), id(0)]);
        let report = result.report();
        let groups: Vec<_> = report.groups().collect();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].kept, id(0));
        assert_eq!(groups[0].folded, &[id(2), id(3)]);
    }

    #[test]
    fn folds_mutual_recursion() {
        // 0 <-> 1 and 2 <-> 3 are identical pairs of mutually recursive
        // functions; 4 calls a different leaf 5.
        let code = b"\xe8\0\0\0\0\xc3";
        let sections = vec![
            section(code),
            section(code),
            section(code),
            section(code),
            section(code),
            section(b"\xc3"),
        ];
        let relocs = [
            (0, call(1)),
            (1, call(0)),
            (2, call(3)),
            (3, call(2)),
            (4, call(5)),
        ];
        let result = run(sections, &relocs, IcfMode::All);
        assert_eq!(
            result.fold_into(),
            &[id(0), id(0), id(0), id(0), id(4), id(5)]
        );
    }

    #[test]
    fn distinguishes_by_target_class() {
        let code = b"\xe8\0\0\0\0\xc3";
        let sections = vec![
            section(code),
            section(code),
            section(b"\x01"),
            section(b"\x02"),
        ];
        let relocs = [(0, call(2)), (1, call(3))];
        let result = run(sections, &relocs, IcfMode::All);
        assert!(!result.is_folded(id(1)));
        assert_eq!(result.num_folded(), 0);
    }

    #[test]
    fn safe_mode_respects_address_significance() {
        let code = b"\xc3";
        let mut significant = section(code);
        significant.address_significant = true;
        let sections = vec![section(code), significant, section(code)];
        assert_eq!(
            run(sections.clone(), &[], IcfMode::Safe).fold_into(),
            &[id(0), id(1), id(0)]
        );
        assert_eq!(
            run(sections, &[], IcfMode::All).fold_into(),
            &[id(0), id(0), id(0)]
        );
    }

    #[test]
    fn keys_and_non_foldable_sections_block_folding() {
        let code = b"\xc3";
        let mut other_key = section(code);
        other_key.key = 2;
        let mut pinned = section(code);
        pinned.foldable = false;
        let result = run(vec![pinned, section(code), other_key], &[], IcfMode::All);
        assert_eq!(result.num_folded(), 0);
    }

    #[test]
    fn rejects_bad_input() {
        let mut builder = CsrBuilder::new(1);
        builder.push(0, call(3));
        assert!(IcfInput::new(vec![section(b"")], builder.build().unwrap()).is_err());
        assert!(IcfInput::new(vec![], Csr::empty(1)).is_err());
    }
}
