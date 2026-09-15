//! Mergeable sections (`SHF_MERGE`, `SHF_STRINGS`; Mach-O `cstring_literals`
//! and literal pools).
//!
//! Merging runs in two phases, at two points of the pipeline
//! (`docs/architecture.md`):
//!
//! # Phase 1: split (stage 4, object parsing)
//!
//! [`split_section`] splits one input section into pieces and hashes each
//! piece, independently of every other section, so the backend calls it from
//! its parallel per-file parsing. Pieces are NUL-terminated strings (the
//! terminator is part of the piece; characters of 1, 2 or 4 bytes) or
//! fixed-size entries. The resulting [`SplitSection`] maps any input offset to
//! a [`PieceRef`] (piece, addend within the piece) before deduplication, which
//! is how the relocation scan (stage 6) records references into merge
//! sections. Malformed sections give a [`MalformedMerge`].
//!
//! # Phase 2: deduplicate and lay out (stage 8, after GC, before ICF)
//!
//! The backend sorts the live split sections into [`MergeGroup`]s, one per
//! output merged section (same piece kind, and the same name, flags and
//! alignment as its format requires), and passes them to
//! [`merge_split_sections`] as [`MergeInput`]s in input order, optionally
//! with a per-piece liveness bitmap.
//!
//! 1. **Deduplicate** in parallel through a sharded hash table, keyed by the
//!    precomputed piece hash mixed with the group number, and confirmed by
//!    comparing bytes. There are no locks: runs of input sections bucket
//!    their live pieces by shard in parallel, then each shard's table (a
//!    plain hashbrown table) is filled by one task that walks its buckets in
//!    piece order. Each distinct content keeps the lowest piece number, so
//!    the *leader* of every piece is its first live occurrence in input
//!    order.
//! 2. **Tail merge** (optional, strings only, `-O2`): per group, sort the
//!    leaders by reversed content, in parallel. A string that is a suffix of
//!    the string before it in descending order shares that string's storage,
//!    if the offset of the suffix is a multiple of the alignment. Cost:
//!    `O(U log U)` comparisons for `U` distinct strings, each comparison
//!    `O(common suffix length)`.
//! 3. **Assign offsets** per group, in first-occurrence order: every piece
//!    that owns storage starts at the next multiple of the group's
//!    alignment. Without tail merging this runs in parallel over runs of
//!    sections, each laid out from 0 and then shifted by its aligned start;
//!    with it, sequentially, and tail-merged strings point into their owner.
//!    Every other live piece takes its leader's offset (in parallel).
//!
//! The resulting [`MergedSections`] maps (section, piece) and
//! (section, input offset) to an output offset, and writes each group's
//! contents in parallel.
//!
//! [`merge_sections`] runs both phases at once, for callers that have all
//! sections at hand.
//!
//! # Errors
//!
//! Phase 1 reports a malformed section as a [`MalformedMerge`], which needs
//! the file name to become a [`crate::Error`] ([`MalformedMerge::into_error`]).
//! Phase 2 only fails on inconsistent arguments, an [`InputError`], which
//! converts into [`crate::Error::Internal`] with `?`. [`MergeError`], from the
//! one-shot wrapper, combines both and so has no `From` conversion either.

mod split;

use std::fmt;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use hashbrown::HashTable;
use hashbrown::hash_table::Entry;
use rayon::prelude::*;

use super::bitset::BitSet;
use super::csr::{CsrBuilder, InputError};
use split::MIN_PIECES_PER_TASK;
pub use split::{MalformedMerge, MergeProblem, PieceRef, SplitSection, split_section};

/// Number of dedup table shards. Does not affect results.
const SHARDS: usize = 256;

/// Pieces per run of input sections bucketed together by `deduplicate`.
/// Does not affect results.
const PIECES_PER_RUN: usize = 1 << 16;

/// Output offset of a piece that is not live.
const DEAD_OFFSET: u64 = u64::MAX;

/// Leader of a piece that is not live.
const DEAD_LEADER: u32 = u32::MAX;

/// How a mergeable section splits into pieces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeKind {
    /// NUL-terminated strings of `char_size`-byte characters (1, 2 or 4;
    /// ELF `SHF_STRINGS` with `sh_entsize`). A character is a terminator when
    /// all its bytes are zero.
    Strings {
        /// Bytes per character.
        char_size: u8,
    },
    /// Fixed-size entries of `entry_size` bytes (ELF `sh_entsize`).
    Fixed {
        /// Bytes per entry; must not be zero.
        entry_size: u64,
    },
}

/// One output merged section: the sections in it share this description.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MergeGroup {
    /// How the group's sections split into pieces.
    pub kind: MergeKind,
    /// Alignment of every piece in the output; a power of two, at least the
    /// alignment of every section in the group.
    pub alignment: u64,
    /// Whether strings may share storage with strings they are a suffix of
    /// (`-O2`). Ignored for fixed-size entries.
    pub tail_merge: bool,
}

/// An error from [`merge_sections`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MergeError {
    /// An input section's contents are malformed.
    Malformed {
        /// Index of the section in the input slice.
        section: usize,
        /// What is wrong with it.
        malformed: MalformedMerge,
    },
    /// The groups or sections passed in are inconsistent.
    Input(InputError),
}

impl fmt::Display for MergeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed { section, malformed } => {
                write!(f, "merge section {section}: {malformed}")
            }
            Self::Input(input) => input.fmt(f),
        }
    }
}

impl std::error::Error for MergeError {}

impl From<InputError> for MergeError {
    fn from(error: InputError) -> Self {
        Self::Input(error)
    }
}

/// A piece that owns storage in a group's output, for writing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutputPiece {
    /// Offset in the merged output section.
    pub output_offset: u64,
    /// Index of the input section the bytes come from.
    pub section: u32,
    /// Offset of the bytes in that input section.
    pub input_offset: u64,
    /// Number of bytes.
    pub len: u64,
}

/// The layout of one output merged section.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergedGroup {
    size: u64,
    alignment: u64,
    pieces: Vec<OutputPiece>,
}

impl MergedGroup {
    /// Size of the merged section in bytes.
    #[must_use]
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Alignment of the merged section.
    #[must_use]
    pub fn alignment(&self) -> u64 {
        self.alignment
    }

    /// The pieces that own storage, in increasing output offset. Gaps between
    /// them are alignment padding.
    #[must_use]
    pub fn pieces(&self) -> &[OutputPiece] {
        &self.pieces
    }
}

/// One live split section given to [`merge_split_sections`].
#[derive(Clone, Copy, Debug)]
pub struct MergeInput<'s, 'a> {
    /// Index of the section's group in the slice of [`MergeGroup`]s.
    pub group: u32,
    /// The section, split at parse time.
    pub split: &'s SplitSection<'a>,
}

/// The result of [`merge_split_sections`] or [`merge_sections`].
///
/// Sections are named by their index in the input slice; pieces by their
/// index within their section, as in [`SplitSection`].
#[derive(Clone, Debug)]
pub struct MergedSections<'s, 'a> {
    splits: Splits<'s, 'a>,
    section_groups: Vec<u32>,
    groups: Vec<MergedGroup>,
    /// Global piece numbers: pieces of section `s` are
    /// `piece_base[s]..piece_base[s + 1]`.
    piece_base: Vec<usize>,
    /// Output offset of every piece, by global number; [`DEAD_OFFSET`] for
    /// pieces that are not live.
    output_offset: Vec<u64>,
}

impl<'a> MergedSections<'_, 'a> {
    /// The merged layout of group `group`.
    #[must_use]
    pub fn group(&self, group: usize) -> Option<&MergedGroup> {
        self.groups.get(group)
    }

    /// All groups, in the order they were given.
    #[must_use]
    pub fn groups(&self) -> &[MergedGroup] {
        &self.groups
    }

    /// Number of input sections.
    #[must_use]
    pub fn num_sections(&self) -> usize {
        self.splits.len()
    }

    /// Total number of pieces across all sections, live or not.
    #[must_use]
    pub fn num_pieces(&self) -> usize {
        self.output_offset.len()
    }

    /// The split section `section`.
    #[must_use]
    pub fn split(&self, section: usize) -> Option<&SplitSection<'a>> {
        self.splits.get(section)
    }

    /// The group number of section `section`.
    #[must_use]
    pub fn section_group(&self, section: usize) -> Option<u32> {
        self.section_groups.get(section).copied()
    }

    /// Global number of the first piece of `section`: its pieces are
    /// numbered from here on, as in the liveness bitmap given to
    /// [`merge_split_sections`]. `Some(num_pieces())` for one past the last
    /// section.
    #[must_use]
    pub fn first_piece(&self, section: usize) -> Option<usize> {
        self.piece_base.get(section).copied()
    }

    /// Finds the piece containing `offset` in section `section`, and the
    /// offset within that piece. `None` if the section does not exist or the
    /// offset is at or past its end. Pieces that are not live are found too.
    #[must_use]
    pub fn piece_at(&self, section: usize, offset: u64) -> Option<PieceRef> {
        self.split(section)?.piece_at(offset)
    }

    /// Output offset of the start of `piece` of `section`, in its group's
    /// merged section. `None` if there is no such piece or it is not live.
    #[must_use]
    pub fn piece_output_offset(&self, section: usize, piece: u32) -> Option<u64> {
        let base = *self.piece_base.get(section)?;
        let end = *self.piece_base.get(section.checked_add(1)?)?;
        let index = base.checked_add(piece as usize).filter(|&i| i < end)?;
        self.output_offset
            .get(index)
            .copied()
            .filter(|&offset| offset != DEAD_OFFSET)
    }

    /// Maps `offset` in input section `section` to an offset in the merged
    /// output section of its group, keeping the position within the piece.
    /// `None` if the offset is out of range or its piece is not live.
    #[must_use]
    pub fn output_offset(&self, section: usize, offset: u64) -> Option<u64> {
        let found = self.piece_at(section, offset)?;
        self.piece_output_offset(section, found.piece)?
            .checked_add(found.addend)
    }

    /// Writes the merged contents of `group` into `out`, in parallel. Bytes
    /// of `out` not covered by a piece (padding and any tail beyond the size)
    /// are zeroed.
    ///
    /// # Errors
    ///
    /// Returns [`InputError::OutOfRange`] if `group` does not exist or `out`
    /// is shorter than the group's size.
    pub fn write_group(&self, group: usize, out: &mut [u8]) -> Result<(), InputError> {
        let merged = self.groups.get(group).ok_or(InputError::OutOfRange {
            what: "merge group",
            index: group as u64,
            len: self.groups.len(),
        })?;
        if (out.len() as u64) < merged.size {
            return Err(InputError::OutOfRange {
                what: "merged section size",
                index: merged.size,
                len: out.len(),
            });
        }
        write_pieces(&self.splits, &merged.pieces, 0, out);
        Ok(())
    }
}

/// Writes `pieces` (sorted by output offset, non-overlapping, all starting at
/// or after `base`) into `out`, which starts at output offset `base`.
fn write_pieces(splits: &Splits<'_, '_>, pieces: &[OutputPiece], base: u64, out: &mut [u8]) {
    if pieces.len() > MIN_PIECES_PER_TASK {
        let mid = pieces.len() / 2;
        let split = pieces[mid].output_offset;
        let at = usize::try_from(split - base)
            .unwrap_or(out.len())
            .min(out.len());
        let (left_out, right_out) = out.split_at_mut(at);
        let (left, right) = pieces.split_at(mid);
        rayon::join(
            || write_pieces(splits, left, base, left_out),
            || write_pieces(splits, right, split, right_out),
        );
        return;
    }
    let mut cursor = 0usize;
    for piece in pieces {
        let bytes = splits
            .get(piece.section as usize)
            .and_then(|section| {
                let start = usize::try_from(piece.input_offset).ok()?;
                let len = usize::try_from(piece.len).ok()?;
                section.data().get(start..start.checked_add(len)?)
            })
            .unwrap_or(&[]);
        let Some(start) = usize::try_from(piece.output_offset - base).ok() else {
            break;
        };
        let Some(dest) = out.get_mut(start..start.saturating_add(bytes.len())) else {
            break;
        };
        dest.copy_from_slice(bytes);
        if let Some(gap) = out.get_mut(cursor.min(start)..start) {
            gap.fill(0);
        }
        cursor = start + bytes.len();
    }
    if let Some(rest) = out.get_mut(cursor..) {
        rest.fill(0);
    }
}

/// A piece that is the first occurrence of its contents, during layout.
struct Leader<'a> {
    piece: u32,
    section: u32,
    input_offset: u64,
    bytes: &'a [u8],
}

/// A dedup table entry: one distinct piece content.
#[derive(Clone, Copy, Debug)]
struct Unique<'a> {
    hash: u64,
    group: u32,
    /// The lowest piece number with this content: its leader.
    leader: u32,
    bytes: &'a [u8],
}

/// The live pieces of a run of consecutive input sections, bucketed by
/// shard: bucket `s` is `entries[starts[s]..starts[s + 1]]`, in piece order.
struct Bucketed {
    starts: Vec<u32>,
    /// (input section, piece within the section).
    entries: Vec<(u32, u32)>,
}

impl Bucketed {
    fn bucket(&self, shard: usize) -> &[(u32, u32)] {
        let start = self.starts[shard] as usize;
        let end = self.starts[shard + 1] as usize;
        &self.entries[start..end]
    }
}

fn check_groups(groups: &[MergeGroup]) -> Result<(), InputError> {
    for (index, group) in groups.iter().enumerate() {
        let valid_kind = match group.kind {
            MergeKind::Strings { char_size } => matches!(char_size, 1 | 2 | 4),
            MergeKind::Fixed { entry_size } => entry_size != 0,
        };
        if !valid_kind || !group.alignment.is_power_of_two() {
            return Err(InputError::OutOfRange {
                what: "merge group kind or alignment",
                index: index as u64,
                len: groups.len(),
            });
        }
    }
    Ok(())
}

fn shard_of(hash: u64) -> usize {
    // hashbrown uses the low bits for buckets and the top 7 for tags.
    (hash >> 32) as usize & (SHARDS - 1)
}

/// Mixes the group number into a piece hash, so that equal pieces of
/// different groups land in different buckets. Group 0 keeps the hash.
#[inline]
fn group_hash(hash: u64, group: u32) -> u64 {
    hash ^ u64::from(group).wrapping_mul(0x9e37_79b9_7f4a_7c15)
}

fn align_to(value: u64, alignment: u64) -> Option<u64> {
    let mask = alignment - 1;
    value.checked_add(mask).map(|v| v & !mask)
}

/// Deduplicates and lays out split mergeable sections (phase 2).
///
/// `inputs` lists the live split sections in input order, each with its
/// group: that order decides which copy of a piece is kept and where it
/// goes. Each section's kind must equal its group's, and its alignment must
/// not exceed the group's.
///
/// `live_pieces`, if given, has one bit per piece of every input, numbered
/// consecutively: piece `p` of `inputs[i]` is bit
/// `inputs[..i].num_pieces() + p`. Pieces whose bit is clear get no output
/// offset and do not count as occurrences. `None` means every piece is live.
///
/// Must run inside the caller's rayon pool; the result is the same for any
/// thread count.
///
/// # Errors
///
/// [`InputError`] if a group is invalid (a character size other than 1, 2 or
/// 4, a zero entry size, an alignment that is not a power of two), an input
/// names a group that does not exist or does not match it, the liveness
/// bitmap has the wrong length, there are 2^32 pieces or more, or a merged
/// section's size overflows.
pub fn merge_split_sections<'s, 'a>(
    groups: &[MergeGroup],
    inputs: &[MergeInput<'s, 'a>],
    live_pieces: Option<&BitSet>,
) -> Result<MergedSections<'s, 'a>, InputError> {
    let layout = lay_out(groups, inputs, live_pieces)?;
    Ok(layout.into_merged(
        Splits::Borrowed(inputs.iter().map(|input| input.split).collect()),
        inputs.iter().map(|input| input.group).collect(),
    ))
}

/// One mergeable input section for [`merge_sections`].
#[derive(Clone, Copy, Debug)]
pub struct MergeSection<'a> {
    /// Index of the section's group in the slice of [`MergeGroup`]s.
    pub group: u32,
    /// The section's bytes, zero-copy from the input mapping.
    pub data: &'a [u8],
}

/// Splits, deduplicates and lays out mergeable sections: both phases at once.
///
/// Each section is split with its group's kind and alignment (phase 1, in
/// parallel), then all are merged with every piece live (phase 2).
/// `sections` must be in input order. Must run inside the caller's rayon
/// pool; the result is the same for any thread count.
///
/// # Errors
///
/// [`MergeError::Malformed`] for the lowest-numbered malformed section, or
/// [`MergeError::Input`] if a group is invalid, a section names a group that
/// does not exist, or phase 2 fails.
pub fn merge_sections<'a>(
    groups: &[MergeGroup],
    sections: &[MergeSection<'a>],
) -> Result<MergedSections<'a, 'a>, MergeError> {
    check_groups(groups)?;
    let group_of = |section: &MergeSection<'_>| {
        groups
            .get(section.group as usize)
            .ok_or(InputError::OutOfRange {
                what: "merge group",
                index: u64::from(section.group),
                len: groups.len(),
            })
    };
    for section in sections {
        group_of(section)?;
    }
    let split = |section: &MergeSection<'a>| match group_of(section) {
        Ok(group) => split_section(section.data, group.kind, group.alignment),
        // Checked above.
        Err(_) => Err(MalformedMerge {
            offset: 0,
            problem: MergeProblem::InvalidEntrySize { size: 0 },
        }),
    };
    // Collect options rather than a `Result`, which rayon cannot collect as
    // an indexed iterator.
    let mut splits: Vec<Option<SplitSection<'a>>> = Vec::with_capacity(sections.len());
    sections
        .par_iter()
        .map(|section| split(section).ok())
        .collect_into_vec(&mut splits);
    let splits: Vec<SplitSection<'a>> = match splits.into_iter().collect() {
        Some(splits) => splits,
        None => {
            // Report the lowest-numbered malformed section.
            let (section, malformed) = sections
                .iter()
                .enumerate()
                .find_map(|(index, section)| split(section).err().map(|error| (index, error)))
                .unwrap_or((
                    0,
                    MalformedMerge {
                        offset: 0,
                        problem: MergeProblem::InvalidEntrySize { size: 0 },
                    },
                ));
            return Err(MergeError::Malformed { section, malformed });
        }
    };
    let inputs: Vec<MergeInput<'_, 'a>> = splits
        .iter()
        .zip(sections)
        .map(|(split, section)| MergeInput {
            group: section.group,
            split,
        })
        .collect();
    let layout = lay_out(groups, &inputs, None)?;
    drop(inputs);
    Ok(layout.into_merged(
        Splits::Owned(splits),
        sections.iter().map(|section| section.group).collect(),
    ))
}

/// The split sections a [`MergedSections`] refers to: borrowed from the
/// backend after [`merge_split_sections`], owned after [`merge_sections`].
#[derive(Clone, Debug)]
enum Splits<'s, 'a> {
    Borrowed(Vec<&'s SplitSection<'a>>),
    Owned(Vec<SplitSection<'a>>),
}

impl<'a> Splits<'_, 'a> {
    fn len(&self) -> usize {
        match self {
            Self::Borrowed(splits) => splits.len(),
            Self::Owned(splits) => splits.len(),
        }
    }

    #[inline]
    fn get(&self, section: usize) -> Option<&SplitSection<'a>> {
        match self {
            Self::Borrowed(splits) => splits.get(section).copied(),
            Self::Owned(splits) => splits.get(section),
        }
    }
}

/// Everything phase 2 computes, without the borrowed inputs.
struct Layout {
    groups: Vec<MergedGroup>,
    piece_base: Vec<usize>,
    output_offset: Vec<u64>,
}

impl Layout {
    fn into_merged<'s, 'a>(
        self,
        splits: Splits<'s, 'a>,
        section_groups: Vec<u32>,
    ) -> MergedSections<'s, 'a> {
        MergedSections {
            splits,
            section_groups,
            groups: self.groups,
            piece_base: self.piece_base,
            output_offset: self.output_offset,
        }
    }
}

/// Phase 2 proper: validates, deduplicates and lays out.
fn lay_out(
    groups: &[MergeGroup],
    inputs: &[MergeInput<'_, '_>],
    live: Option<&BitSet>,
) -> Result<Layout, InputError> {
    check_groups(groups)?;
    if u32::try_from(inputs.len()).is_err() {
        return Err(InputError::TooLarge("merge section count"));
    }
    let mut piece_base = Vec::with_capacity(inputs.len() + 1);
    piece_base.push(0usize);
    let mut total = 0usize;
    for (index, input) in inputs.iter().enumerate() {
        let group = groups
            .get(input.group as usize)
            .ok_or(InputError::OutOfRange {
                what: "merge group",
                index: u64::from(input.group),
                len: groups.len(),
            })?;
        if input.split.kind() != group.kind || input.split.alignment() > group.alignment {
            return Err(InputError::Mismatch {
                what: "merge section kind or alignment and its group's",
                index: index as u64,
            });
        }
        total = total
            .checked_add(input.split.num_pieces())
            .ok_or(InputError::TooLarge("merge piece count"))?;
        piece_base.push(total);
    }
    // Piece numbers are u32, with `DEAD_LEADER` reserved.
    if u32::try_from(total).map_or(true, |total| total == DEAD_LEADER) {
        return Err(InputError::TooLarge("merge piece count"));
    }
    if let Some(live) = live
        && live.len() != total
    {
        return Err(InputError::Mismatch {
            what: "merge piece liveness bitmap length and piece count",
            index: live.len() as u64,
        });
    }

    // 1. Deduplicate: keep the lowest live piece number for each content.
    let leader = deduplicate(inputs, &piece_base, total, live);

    // 2 and 3. Tail merge and lay out each group.
    let mut members = CsrBuilder::with_capacity(groups.len(), inputs.len());
    for (index, input) in inputs.iter().enumerate() {
        members.push(input.group as usize, index);
    }
    let members = members.build()?;
    let out_offset: Vec<AtomicU64> = (0..total)
        .into_par_iter()
        .map(|_| AtomicU64::new(DEAD_OFFSET))
        .collect();
    let merged: Vec<Result<MergedGroup, InputError>> = groups
        .par_iter()
        .enumerate()
        .map(|(group_index, group)| {
            layout_group(
                group,
                members.row(group_index),
                inputs,
                &piece_base,
                &leader,
                &out_offset,
            )
        })
        .collect();
    let groups = merged.into_iter().collect::<Result<Vec<_>, _>>()?;

    // Every other live piece takes its leader's offset. Leaders are only
    // read.
    (0..total)
        .into_par_iter()
        .with_min_len(MIN_PIECES_PER_TASK)
        .for_each(|index| {
            let lead = leader[index];
            if lead != DEAD_LEADER && lead as usize != index {
                let value = out_offset[lead as usize].load(Ordering::Relaxed);
                out_offset[index].store(value, Ordering::Relaxed);
            }
        });

    Ok(Layout {
        groups,
        piece_base,
        // In place: the same layout, so no copy.
        output_offset: out_offset.into_iter().map(AtomicU64::into_inner).collect(),
    })
}

/// Finds the leader of every live piece: the lowest live piece number with
/// the same contents in the same group ([`DEAD_LEADER`] for dead pieces).
///
/// Lock-free: the input sections are cut into runs of about
/// [`PIECES_PER_RUN`] pieces, and each run's live pieces are bucketed by
/// shard, in parallel. Then every shard, in parallel, walks its buckets in
/// run order, so it sees its pieces in increasing piece number and the first
/// one inserted into its table is the leader.
fn deduplicate(
    inputs: &[MergeInput<'_, '_>],
    piece_base: &[usize],
    total: usize,
    live: Option<&BitSet>,
) -> Vec<u32> {
    let mut runs = Vec::new();
    let mut run_start = 0usize;
    for section in 0..inputs.len() {
        if piece_base[section + 1] - piece_base[run_start] >= PIECES_PER_RUN {
            runs.push(run_start..section + 1);
            run_start = section + 1;
        }
    }
    if run_start < inputs.len() {
        runs.push(run_start..inputs.len());
    }
    // Every live piece of a section, with its group-mixed hash.
    let pieces = |section: usize| {
        let input = &inputs[section];
        let base = piece_base[section];
        input
            .split
            .hashes()
            .iter()
            .enumerate()
            .filter(move |&(local, _)| live.is_none_or(|live| live.get(base + local)))
            .map(move |(local, &hash)| (local, group_hash(hash, input.group)))
    };
    let buckets: Vec<Bucketed> = runs
        .par_iter()
        .map(|run| {
            let mut starts = vec![0u32; SHARDS + 1];
            for section in run.clone() {
                for (_, hash) in pieces(section) {
                    starts[shard_of(hash) + 1] += 1;
                }
            }
            for shard in 0..SHARDS {
                starts[shard + 1] += starts[shard];
            }
            let mut cursor = starts.clone();
            let mut entries = vec![(0u32, 0u32); starts[SHARDS] as usize];
            for section in run.clone() {
                for (local, hash) in pieces(section) {
                    let at = &mut cursor[shard_of(hash)];
                    // Section and piece counts fit in u32 (checked by the
                    // caller).
                    entries[*at as usize] = (section as u32, local as u32);
                    *at += 1;
                }
            }
            Bucketed { starts, entries }
        })
        .collect();

    let leader: Vec<AtomicU32> = (0..total)
        .into_par_iter()
        .with_min_len(MIN_PIECES_PER_TASK)
        .map(|_| AtomicU32::new(DEAD_LEADER))
        .collect();
    (0..SHARDS).into_par_iter().for_each(|shard| {
        let count: usize = buckets.iter().map(|b| b.bucket(shard).len()).sum();
        let mut table: HashTable<Unique<'_>> = HashTable::with_capacity(count / 2);
        for bucket in &buckets {
            for &(section, local) in bucket.bucket(shard) {
                let input = &inputs[section as usize];
                let local = local as usize;
                let hash = group_hash(input.split.hashes()[local], input.group);
                let bytes = input.split.bytes_of(local);
                let index = (piece_base[section as usize] + local) as u32;
                let group = input.group;
                let entry = table.entry(
                    hash,
                    |unique| unique.hash == hash && unique.group == group && unique.bytes == bytes,
                    |unique| unique.hash,
                );
                let lead = match entry {
                    Entry::Occupied(occupied) => occupied.get().leader,
                    Entry::Vacant(vacant) => {
                        vacant.insert(Unique {
                            hash,
                            group,
                            leader: index,
                            bytes,
                        });
                        index
                    }
                };
                leader[index as usize].store(lead, Ordering::Relaxed);
            }
        }
    });
    leader.into_iter().map(AtomicU32::into_inner).collect()
}

/// The storage-owning pieces of a run of member sections, with offsets from
/// the run's start, and their global piece numbers.
struct RunLayout {
    pieces: Vec<OutputPiece>,
    numbers: Vec<u32>,
    /// End of the last piece, from the run's start.
    span: u64,
}

/// Lays out a group without tail merging, in parallel.
///
/// Every leader owns storage, at the next multiple of the alignment. The
/// member sections are cut into runs, and each run is laid out from offset 0
/// in parallel. A run that starts at an aligned offset `base` then only
/// shifts by `base` (`align(base + x) = base + align(x)`), so a sequential
/// pass over the runs' spans gives each run its base, and the result is the
/// same as a single sequential pass.
fn layout_group_in_runs(
    group: &MergeGroup,
    members: &[usize],
    inputs: &[MergeInput<'_, '_>],
    piece_base: &[usize],
    leader: &[u32],
    out_offset: &[AtomicU64],
) -> Result<MergedGroup, InputError> {
    let alignment = group.alignment;
    let too_large = InputError::TooLarge("merged section size");
    let mut runs = Vec::new();
    let mut run_start = 0usize;
    let mut run_pieces = 0usize;
    for (position, &section) in members.iter().enumerate() {
        run_pieces += piece_base[section + 1] - piece_base[section];
        if run_pieces >= PIECES_PER_RUN {
            runs.push(run_start..position + 1);
            run_start = position + 1;
            run_pieces = 0;
        }
    }
    if run_start < members.len() {
        runs.push(run_start..members.len());
    }
    let local: Vec<Option<RunLayout>> = runs
        .par_iter()
        .map(|run| {
            let mut layout = RunLayout {
                pieces: Vec::new(),
                numbers: Vec::new(),
                span: 0,
            };
            for &section in &members[run.clone()] {
                let split = inputs[section].split;
                let base = piece_base[section];
                let row = &leader[base..piece_base[section + 1]];
                for (local, &lead) in row.iter().enumerate() {
                    let index = base + local;
                    if lead as usize != index {
                        continue;
                    }
                    let (start, _) = split.bounds(local);
                    let len = split.bytes_of(local).len() as u64;
                    let offset = align_to(layout.span, alignment)?;
                    layout.span = offset.checked_add(len)?;
                    layout.pieces.push(OutputPiece {
                        output_offset: offset,
                        section: section as u32,
                        input_offset: start as u64,
                        len,
                    });
                    layout.numbers.push(index as u32);
                }
            }
            Some(layout)
        })
        .collect();
    let mut local = local
        .into_iter()
        .collect::<Option<Vec<RunLayout>>>()
        .ok_or(too_large.clone())?;
    let mut bases = Vec::with_capacity(local.len());
    let mut next = 0u64;
    let mut size = 0u64;
    for run in &local {
        bases.push(next);
        if !run.pieces.is_empty() {
            size = next.checked_add(run.span).ok_or(too_large.clone())?;
            next = align_to(size, alignment).ok_or(too_large.clone())?;
        }
    }
    // Offsets within a run are at most its span, so these sums are at most
    // `size`, checked above.
    local.par_iter_mut().zip(&bases).for_each(|(run, &base)| {
        for (piece, &number) in run.pieces.iter_mut().zip(&run.numbers) {
            piece.output_offset += base;
            out_offset[number as usize].store(piece.output_offset, Ordering::Relaxed);
        }
    });
    let pieces = local
        .into_par_iter()
        .flat_map_iter(|run| run.pieces)
        .collect();
    Ok(MergedGroup {
        size,
        alignment,
        pieces,
    })
}

/// Lays out one group: collects its leaders in input order, tail merges
/// them if asked, assigns offsets, and records them in `out_offset`.
fn layout_group(
    group: &MergeGroup,
    members: &[usize],
    inputs: &[MergeInput<'_, '_>],
    piece_base: &[usize],
    leader: &[u32],
    out_offset: &[AtomicU64],
) -> Result<MergedGroup, InputError> {
    if !(group.tail_merge && matches!(group.kind, MergeKind::Strings { .. })) {
        return layout_group_in_runs(group, members, inputs, piece_base, leader, out_offset);
    }
    // Leaders of this group, in input order.
    let mut leaders: Vec<Leader<'_>> = Vec::new();
    for &section in members {
        let split = inputs[section].split;
        let base = piece_base[section];
        let row = &leader[base..piece_base[section + 1]];
        for (local, &lead) in row.iter().enumerate() {
            let index = base + local;
            if lead as usize == index {
                let (start, end) = split.bounds(local);
                leaders.push(Leader {
                    piece: index as u32,
                    section: section as u32,
                    input_offset: start as u64,
                    bytes: split.data().get(start..end).unwrap_or(&[]),
                });
            }
        }
    }

    // `link[i] = (owner position, delta)`; owners link to themselves.
    let alignment = group.alignment;
    let mut link: Vec<(u32, u64)> = (0..leaders.len()).map(|i| (i as u32, 0)).collect();
    if group.tail_merge && matches!(group.kind, MergeKind::Strings { .. }) {
        let mut order: Vec<u32> = (0..leaders.len() as u32).collect();
        order.par_sort_unstable_by(|&a, &b| {
            let (x, y) = (leaders[a as usize].bytes, leaders[b as usize].bytes);
            y.iter().rev().cmp(x.iter().rev()).then(a.cmp(&b))
        });
        let mut owner: Option<u32> = None;
        for &position in &order {
            let bytes = leaders[position as usize].bytes;
            if let Some(owner_position) = owner {
                let owner_bytes = leaders[owner_position as usize].bytes;
                if owner_bytes.ends_with(bytes) {
                    let delta = (owner_bytes.len() - bytes.len()) as u64;
                    if delta & (alignment - 1) == 0 {
                        link[position as usize] = (owner_position, delta);
                        continue;
                    }
                }
            }
            owner = Some(position);
        }
    }

    let mut offsets = vec![0u64; leaders.len()];
    let mut size = 0u64;
    let mut pieces = Vec::new();
    let too_large = InputError::TooLarge("merged section size");
    for (position, &(owner, _)) in link.iter().enumerate() {
        if owner as usize != position {
            continue;
        }
        let owner = &leaders[position];
        let len = owner.bytes.len() as u64;
        let start = align_to(size, alignment).ok_or(too_large.clone())?;
        size = start.checked_add(len).ok_or(too_large.clone())?;
        offsets[position] = start;
        pieces.push(OutputPiece {
            output_offset: start,
            section: owner.section,
            input_offset: owner.input_offset,
            len,
        });
    }
    for (position, &(owner, delta)) in link.iter().enumerate() {
        if owner as usize != position {
            offsets[position] = offsets[owner as usize] + delta;
        }
    }
    for (position, lead) in leaders.iter().enumerate() {
        out_offset[lead.piece as usize].store(offsets[position], Ordering::Relaxed);
    }
    Ok(MergedGroup {
        size,
        alignment,
        pieces,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(tail_merge: bool) -> MergeGroup {
        MergeGroup {
            kind: MergeKind::Strings { char_size: 1 },
            alignment: 1,
            tail_merge,
        }
    }

    fn contents(merged: &MergedSections<'_, '_>, group: usize) -> Vec<u8> {
        let size = merged.group(group).unwrap().size() as usize;
        let mut out = vec![0xff; size];
        merged.write_group(group, &mut out).unwrap();
        out
    }

    #[test]
    fn dedups_strings_in_first_occurrence_order() {
        let a = MergeSection {
            group: 0,
            data: b"foo\0bar\0",
        };
        let b = MergeSection {
            group: 0,
            data: b"baz\0foo\0",
        };
        let merged = merge_sections(&[strings(false)], &[a, b]).unwrap();
        assert_eq!(contents(&merged, 0), b"foo\0bar\0baz\0");
        assert_eq!(merged.output_offset(1, 4), Some(0));
        assert_eq!(merged.output_offset(1, 1), Some(9));
        assert_eq!(merged.output_offset(0, 6), Some(6));
        assert_eq!(merged.output_offset(0, 8), None);
        assert_eq!(merged.output_offset(2, 0), None);
        let found = merged.piece_at(1, 6).unwrap();
        assert_eq!(
            found,
            PieceRef {
                piece: 1,
                addend: 2
            }
        );
        assert_eq!(merged.piece_output_offset(1, found.piece), Some(0));
        assert_eq!(merged.piece_output_offset(1, 2), None);
        assert_eq!(merged.first_piece(1), Some(2));
        assert_eq!(merged.section_group(1), Some(0));
    }

    #[test]
    fn two_phases_match_the_wrapper() {
        let datas: [&[u8]; 3] = [b"foo\0bar\0", b"baz\0foo\0", b"bar\0qux\0"];
        let group = strings(false);
        let splits: Vec<SplitSection<'_>> = datas
            .iter()
            .map(|data| split_section(data, group.kind, 1).unwrap())
            .collect();
        let inputs: Vec<MergeInput<'_, '_>> = splits
            .iter()
            .map(|split| MergeInput { group: 0, split })
            .collect();
        let merged = merge_split_sections(&[group], &inputs, None).unwrap();
        let sections: Vec<MergeSection<'_>> = datas
            .iter()
            .map(|data| MergeSection { group: 0, data })
            .collect();
        let wrapped = merge_sections(&[group], &sections).unwrap();
        assert_eq!(merged.groups(), wrapped.groups());
        assert_eq!(contents(&merged, 0), b"foo\0bar\0baz\0qux\0");
    }

    #[test]
    fn dead_pieces_get_no_storage() {
        let group = strings(false);
        let a = split_section(b"foo\0bar\0", group.kind, 1).unwrap();
        let b = split_section(b"bar\0foo\0", group.kind, 1).unwrap();
        let inputs = [
            MergeInput {
                group: 0,
                split: &a,
            },
            MergeInput {
                group: 0,
                split: &b,
            },
        ];
        // Piece 0 of `a` ("foo") is dead, so `b`'s "foo" leads.
        let mut live = BitSet::new(4);
        for bit in 1..4 {
            live.insert(bit);
        }
        let merged = merge_split_sections(&[group], &inputs, Some(&live)).unwrap();
        assert_eq!(contents(&merged, 0), b"bar\0foo\0");
        assert_eq!(merged.output_offset(0, 1), None);
        assert_eq!(merged.output_offset(0, 5), Some(1));
        assert_eq!(merged.output_offset(1, 6), Some(6));
        assert!(matches!(
            merge_split_sections(&[group], &inputs, Some(&BitSet::new(3))),
            Err(InputError::Mismatch { .. })
        ));
        let wide = MergeGroup {
            kind: MergeKind::Strings { char_size: 2 },
            ..group
        };
        assert!(matches!(
            merge_split_sections(&[wide], &inputs, None),
            Err(InputError::Mismatch { .. })
        ));
        let aligned = split_section(b"x\0", group.kind, 4).unwrap();
        let input = [MergeInput {
            group: 0,
            split: &aligned,
        }];
        assert!(merge_split_sections(&[group], &input, None).is_err());
        let wider = MergeGroup {
            alignment: 8,
            ..group
        };
        assert!(merge_split_sections(&[wider], &input, None).is_ok());
    }

    #[test]
    fn tail_merges_suffixes() {
        let a = MergeSection {
            group: 0,
            data: b"bar\0foobar\0ar\0\0",
        };
        let merged = merge_sections(&[strings(true)], &[a]).unwrap();
        assert_eq!(contents(&merged, 0), b"foobar\0");
        assert_eq!(merged.output_offset(0, 0), Some(3));
        assert_eq!(merged.output_offset(0, 4), Some(0));
        assert_eq!(merged.output_offset(0, 11), Some(4));
        assert_eq!(merged.output_offset(0, 14), Some(6));
    }

    #[test]
    fn fixed_entries_respect_alignment() {
        let group = MergeGroup {
            kind: MergeKind::Fixed { entry_size: 2 },
            alignment: 4,
            tail_merge: true,
        };
        let a = MergeSection {
            group: 0,
            data: &[1, 0, 2, 0, 1, 0],
        };
        let merged = merge_sections(&[group], &[a]).unwrap();
        assert_eq!(merged.group(0).unwrap().size(), 6);
        assert_eq!(contents(&merged, 0), vec![1, 0, 0, 0, 2, 0]);
        assert_eq!(merged.output_offset(0, 5), Some(1));
        assert_eq!(merged.output_offset(0, 2), Some(4));
    }

    #[test]
    fn groups_do_not_share_pieces() {
        let groups = [strings(false), strings(false)];
        let sections = [
            MergeSection {
                group: 1,
                data: b"x\0",
            },
            MergeSection {
                group: 0,
                data: b"x\0",
            },
        ];
        let merged = merge_sections(&groups, &sections).unwrap();
        assert_eq!(merged.group(0).unwrap().size(), 2);
        assert_eq!(merged.group(1).unwrap().size(), 2);
        assert_eq!(merged.num_pieces(), 2);
    }

    #[test]
    fn wide_strings() {
        let group = MergeGroup {
            kind: MergeKind::Strings { char_size: 2 },
            alignment: 2,
            tail_merge: true,
        };
        // Pieces are "a" and "ba" in 2-byte characters; "a" is a suffix.
        let data = [b'a', 0, 0, 0, b'b', 0, b'a', 0, 0, 0];
        let merged = merge_sections(
            &[group],
            &[MergeSection {
                group: 0,
                data: &data,
            }],
        )
        .unwrap();
        assert_eq!(contents(&merged, 0), vec![b'b', 0, b'a', 0, 0, 0]);
        assert_eq!(merged.output_offset(0, 0), Some(2));
    }

    #[test]
    fn reports_malformed_sections() {
        let unterminated = MergeSection {
            group: 0,
            data: b"ok\0bad",
        };
        let odd = MergeSection {
            group: 1,
            data: &[0, 0, 0],
        };
        let wide = MergeGroup {
            kind: MergeKind::Strings { char_size: 2 },
            alignment: 1,
            tail_merge: false,
        };
        let err = merge_sections(&[strings(false), wide], &[odd, unterminated]).unwrap_err();
        assert_eq!(
            err,
            MergeError::Malformed {
                section: 0,
                malformed: MalformedMerge {
                    offset: 2,
                    problem: MergeProblem::SizeNotMultiple { unit: 2 },
                },
            }
        );
        let err = merge_sections(&[strings(false)], &[unterminated]).unwrap_err();
        assert_eq!(
            err,
            MergeError::Malformed {
                section: 0,
                malformed: MalformedMerge {
                    offset: 3,
                    problem: MergeProblem::UnterminatedString,
                },
            }
        );
        let fixed = MergeGroup {
            kind: MergeKind::Fixed { entry_size: 0 },
            alignment: 1,
            tail_merge: false,
        };
        assert!(matches!(
            merge_sections(&[fixed], &[]),
            Err(MergeError::Input(_))
        ));
        let bad_group = MergeSection {
            group: 3,
            data: b"",
        };
        assert!(matches!(
            merge_sections(&[strings(false)], &[bad_group]),
            Err(MergeError::Input(_))
        ));
        if let MergeError::Malformed { malformed, .. } =
            merge_sections(&[strings(false)], &[unterminated]).unwrap_err()
        {
            let error = malformed.into_error("a.o", 0x40);
            assert!(error.to_string().contains("0x43"));
        }
    }
}
