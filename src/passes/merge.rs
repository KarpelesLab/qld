//! Mergeable sections (`SHF_MERGE`, `SHF_STRINGS`; Mach-O `cstring_literals`
//! and literal pools).
//!
//! # Input
//!
//! The backend sorts mergeable input sections into [`MergeGroup`]s, one per
//! output merged section: same piece kind, same alignment, same name and
//! flags as its format requires. Each input section is a [`MergeSection`]:
//! its bytes plus its group number. Sections are numbered by their position
//! in the slice passed to [`merge_sections`], which must be input order.
//!
//! # Algorithm
//!
//! 1. **Split** every section into pieces in parallel: NUL-terminated strings
//!    (the terminator is part of the piece; characters of 1, 2 or 4 bytes)
//!    or fixed-size entries. Malformed sections (an unterminated last string,
//!    a size that is not a multiple of the character or entry size) give a
//!    [`MergeError`]; the one in the lowest-numbered section is reported.
//! 2. **Deduplicate** in parallel through a sharded hash table (hashbrown
//!    behind per-shard locks, shard picked from a fixed-seed foldhash of the
//!    piece). Each distinct content keeps the lowest piece number, so the
//!    *leader* of every piece is its first occurrence in input order no
//!    matter which thread inserted first.
//! 3. **Tail merge** (optional, strings only, `-O2`): per group, sort the
//!    leaders by reversed content, in parallel. A string that is a suffix of
//!    the string before it in descending order shares that string's storage,
//!    if the offset of the suffix is a multiple of the alignment. Cost:
//!    `O(U log U)` comparisons for `U` distinct strings, each comparison
//!    `O(common suffix length)`.
//! 4. **Assign offsets** per group, sequentially in first-occurrence order:
//!    every piece that owns storage starts at the next multiple of the
//!    group's alignment. Tail-merged strings point into their owner. Every
//!    other piece takes its leader's offset (in parallel).
//!
//! The result maps any `(section, input offset)`, including offsets in the
//! middle of a piece, to an output offset or to a piece plus an addend within
//! it, and lists each group's pieces for writing.

use std::fmt;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use hashbrown::HashTable;
use hashbrown::hash_table::Entry;
use rayon::prelude::*;

use super::csr::{CsrBuilder, InputError, for_each_row_mut};
use super::hash::hasher;
use std::hash::Hasher;

/// Number of dedup table shards. Does not affect results.
const SHARDS: usize = 256;

/// Minimum number of pieces per parallel task within one section.
const MIN_PIECES_PER_TASK: usize = 1024;

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
    /// Alignment of every piece in the output; a power of two.
    pub alignment: u64,
    /// Whether strings may share storage with strings they are a suffix of
    /// (`-O2`). Ignored for fixed-size entries.
    pub tail_merge: bool,
}

/// One mergeable input section.
#[derive(Clone, Copy, Debug)]
pub struct MergeSection<'a> {
    /// Index of the section's group in the slice of [`MergeGroup`]s.
    pub group: u32,
    /// The section's bytes, zero-copy from the input mapping.
    pub data: &'a [u8],
}

/// What is wrong with a malformed mergeable section.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeProblem {
    /// The last string has no terminator.
    UnterminatedString,
    /// The section size is not a multiple of the character or entry size.
    SizeNotMultiple {
        /// The character or entry size.
        unit: u64,
    },
}

/// A malformed mergeable input section.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MalformedMerge {
    /// Index of the section in the input slice.
    pub section: usize,
    /// Offset within the section where the problem starts.
    pub offset: u64,
    /// What is wrong.
    pub problem: MergeProblem,
}

impl MalformedMerge {
    /// Converts into a fatal [`crate::Error::Malformed`] for `file`, given
    /// the file offset at which the section's data starts.
    #[must_use]
    pub fn into_error(self, file: impl Into<PathBuf>, section_file_offset: u64) -> crate::Error {
        crate::Error::malformed(
            file,
            section_file_offset.saturating_add(self.offset),
            self.to_string(),
        )
    }
}

impl fmt::Display for MalformedMerge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.problem {
            MergeProblem::UnterminatedString => {
                f.write_str("mergeable string section: string is not null terminated")
            }
            MergeProblem::SizeNotMultiple { unit } => write!(
                f,
                "mergeable section: size is not a multiple of the entry size {unit}"
            ),
        }
    }
}

/// An error from [`merge_sections`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MergeError {
    /// An input section's contents are malformed.
    Malformed(MalformedMerge),
    /// The groups or sections passed in are inconsistent.
    Input(InputError),
}

impl fmt::Display for MergeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(malformed) => malformed.fmt(f),
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

/// Identifies one piece of one input section, across all groups. Pieces are
/// numbered in input order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PieceId(u32);

impl PieceId {
    /// The zero-based piece number.
    #[must_use]
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// A location inside a merged piece.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PieceRef {
    /// The piece containing the location.
    pub piece: PieceId,
    /// Offset of the location within the piece.
    pub addend: u64,
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

/// The result of [`merge_sections`].
#[derive(Clone, Debug)]
pub struct MergedSections<'a> {
    sections: Vec<MergeSection<'a>>,
    groups: Vec<MergedGroup>,
    /// CSR offsets: pieces of section `s` are `piece_start[s]..piece_start[s + 1]`.
    piece_start: Vec<usize>,
    input_offset: Vec<u64>,
    output_offset: Vec<u64>,
}

impl<'a> MergedSections<'a> {
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

    /// Total number of pieces across all sections.
    #[must_use]
    pub fn num_pieces(&self) -> usize {
        self.input_offset.len()
    }

    /// Finds the piece containing `offset` in section `section`, and the
    /// offset within that piece. `None` if the section does not exist or the
    /// offset is at or past its end.
    #[must_use]
    pub fn piece_at(&self, section: usize, offset: u64) -> Option<PieceRef> {
        let data = self.sections.get(section)?.data;
        if offset >= data.len() as u64 {
            return None;
        }
        let start = *self.piece_start.get(section)?;
        let end = *self.piece_start.get(section.checked_add(1)?)?;
        let row = self.input_offset.get(start..end)?;
        let local = row
            .partition_point(|&piece| piece <= offset)
            .checked_sub(1)?;
        let piece_start = *row.get(local)?;
        Some(PieceRef {
            piece: PieceId(u32::try_from(start + local).ok()?),
            addend: offset - piece_start,
        })
    }

    /// Output offset of the start of `piece` in its group's merged section.
    #[must_use]
    pub fn piece_output_offset(&self, piece: PieceId) -> Option<u64> {
        self.output_offset.get(piece.index()).copied()
    }

    /// Maps `offset` in input section `section` to an offset in the merged
    /// output section of its group, keeping the position within the piece.
    #[must_use]
    pub fn output_offset(&self, section: usize, offset: u64) -> Option<u64> {
        let found = self.piece_at(section, offset)?;
        self.piece_output_offset(found.piece)?
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
        write_pieces(&self.sections, &merged.pieces, 0, out);
        Ok(())
    }
}

/// Writes `pieces` (sorted by output offset, non-overlapping, all starting at
/// or after `base`) into `out`, which starts at output offset `base`.
fn write_pieces(sections: &[MergeSection<'_>], pieces: &[OutputPiece], base: u64, out: &mut [u8]) {
    if pieces.len() > MIN_PIECES_PER_TASK {
        let mid = pieces.len() / 2;
        let split = pieces[mid].output_offset;
        let at = usize::try_from(split - base)
            .unwrap_or(out.len())
            .min(out.len());
        let (left_out, right_out) = out.split_at_mut(at);
        let (left, right) = pieces.split_at(mid);
        rayon::join(
            || write_pieces(sections, left, base, left_out),
            || write_pieces(sections, right, split, right_out),
        );
        return;
    }
    let mut cursor = 0usize;
    for piece in pieces {
        let bytes = sections
            .get(piece.section as usize)
            .and_then(|section| {
                let start = usize::try_from(piece.input_offset).ok()?;
                let len = usize::try_from(piece.len).ok()?;
                section.data.get(start..start.checked_add(len)?)
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

/// A piece during splitting.
#[derive(Clone, Copy, Debug, Default)]
struct Piece {
    offset: u64,
    hash: u64,
}

/// The split pieces of all sections.
struct Split<'s, 'a> {
    sections: &'s [MergeSection<'a>],
    piece_start: &'s [usize],
    pieces: &'s [Piece],
}

impl<'s, 'a> Split<'s, 'a> {
    /// The pieces of `section`.
    fn row(&self, section: usize) -> &'s [Piece] {
        &self.pieces[self.piece_start[section]..self.piece_start[section + 1]]
    }

    /// The bytes of piece `local` of `section`, whose pieces are `row`.
    fn bytes(&self, section: usize, local: usize, row: &[Piece]) -> &'a [u8] {
        let data = self.sections[section].data;
        let start = row[local].offset as usize;
        let end = row
            .get(local + 1)
            .map_or(data.len(), |next| next.offset as usize);
        &data[start..end]
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
    /// Index of this content's leader in [`Shard::leaders`].
    slot: u32,
    bytes: &'a [u8],
}

/// One shard of the dedup table.
struct Shard<'a> {
    table: HashTable<Unique<'a>>,
    /// Lowest piece number seen for each distinct content, by slot.
    leaders: Vec<u32>,
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

/// Counts the pieces of one section, validating it.
fn count_pieces(kind: MergeKind, data: &[u8]) -> Result<usize, (u64, MergeProblem)> {
    let len = data.len();
    match kind {
        MergeKind::Strings { char_size } => {
            let unit = usize::from(char_size);
            let remainder = len % unit;
            if remainder != 0 {
                return Err((
                    (len - remainder) as u64,
                    MergeProblem::SizeNotMultiple { unit: unit as u64 },
                ));
            }
            if unit == 1 {
                if data.last().is_some_and(|&last| last != 0) {
                    let start = data.iter().rposition(|&b| b == 0).map_or(0, |p| p + 1);
                    return Err((start as u64, MergeProblem::UnterminatedString));
                }
                return Ok(data.iter().filter(|&&b| b == 0).count());
            }
            let is_nul = |c: &[u8]| c.iter().all(|&b| b == 0);
            if data
                .chunks_exact(unit)
                .next_back()
                .is_some_and(|c| !is_nul(c))
            {
                let start = data
                    .chunks_exact(unit)
                    .rposition(is_nul)
                    .map_or(0, |p| (p + 1) * unit);
                return Err((start as u64, MergeProblem::UnterminatedString));
            }
            Ok(data.chunks_exact(unit).filter(|c| is_nul(c)).count())
        }
        MergeKind::Fixed { entry_size } => {
            let size_error = (0, MergeProblem::SizeNotMultiple { unit: entry_size });
            let unit = usize::try_from(entry_size).map_err(|_| size_error)?;
            if !len.is_multiple_of(unit) {
                return Err((
                    (len - len % unit) as u64,
                    MergeProblem::SizeNotMultiple { unit: entry_size },
                ));
            }
            Ok(len / unit)
        }
    }
}

/// Writes the start offset and hash of each piece of a validated section.
fn split_pieces(kind: MergeKind, group: u32, data: &[u8], pieces: &mut [Piece]) {
    let hash = |bytes: &[u8]| {
        let mut h = hasher();
        h.write_u32(group);
        h.write(bytes);
        h.finish()
    };
    match kind {
        MergeKind::Strings { char_size } => {
            // Find the pieces sequentially, temporarily storing each piece's
            // end in `hash`, then hash them in parallel.
            let unit = usize::from(char_size);
            let mut start = 0usize;
            let mut slots = pieces.iter_mut();
            let mut record = |end: usize| {
                if let Some(slot) = slots.next() {
                    *slot = Piece {
                        offset: start as u64,
                        hash: end as u64,
                    };
                }
                start = end;
            };
            if unit == 1 {
                for (position, _) in data.iter().enumerate().filter(|&(_, &b)| b == 0) {
                    record(position + 1);
                }
            } else {
                for (index, _) in data
                    .chunks_exact(unit)
                    .enumerate()
                    .filter(|(_, c)| c.iter().all(|&b| b == 0))
                {
                    record((index + 1) * unit);
                }
            }
            pieces
                .par_iter_mut()
                .with_min_len(MIN_PIECES_PER_TASK)
                .for_each(|slot| {
                    let bytes = data
                        .get(slot.offset as usize..slot.hash as usize)
                        .unwrap_or(&[]);
                    slot.hash = hash(bytes);
                });
        }
        MergeKind::Fixed { entry_size } => {
            // Validated to fit usize by `count_pieces`.
            let unit = entry_size as usize;
            pieces
                .par_iter_mut()
                .with_min_len(MIN_PIECES_PER_TASK)
                .zip(data.par_chunks_exact(unit))
                .enumerate()
                .for_each(|(index, (slot, bytes))| {
                    *slot = Piece {
                        offset: (index * unit) as u64,
                        hash: hash(bytes),
                    };
                });
        }
    }
}

fn shard_of(hash: u64) -> usize {
    // hashbrown uses the low bits for buckets and the top 7 for tags.
    (hash >> 32) as usize & (SHARDS - 1)
}

fn align_to(value: u64, alignment: u64) -> Option<u64> {
    let mask = alignment - 1;
    value.checked_add(mask).map(|v| v & !mask)
}

/// Splits, deduplicates and lays out mergeable sections.
///
/// `sections` must be in input order: that order decides which copy of a
/// piece is kept and where it goes. Must run inside the caller's rayon pool;
/// the result is the same for any thread count.
///
/// # Errors
///
/// [`MergeError::Malformed`] for the lowest-numbered malformed section, or
/// [`MergeError::Input`] if a group is invalid (a character size other than
/// 1, 2 or 4, a zero entry size, an alignment that is not a power of two), a
/// section names a group that does not exist, or there are more than
/// `u32::MAX` pieces.
pub fn merge_sections<'a>(
    groups: &[MergeGroup],
    sections: &[MergeSection<'a>],
) -> Result<MergedSections<'a>, MergeError> {
    check_groups(groups)?;
    if let Some(section) = sections
        .iter()
        .find(|section| section.group as usize >= groups.len())
    {
        return Err(InputError::OutOfRange {
            what: "merge group",
            index: u64::from(section.group),
            len: groups.len(),
        }
        .into());
    }
    if u32::try_from(sections.len()).is_err() {
        return Err(InputError::TooLarge("merge section count").into());
    }
    let kind_of = |section: &MergeSection<'_>| groups[section.group as usize].kind;

    // 1. Split.
    let counts: Vec<Result<usize, (u64, MergeProblem)>> = sections
        .par_iter()
        .map(|section| count_pieces(kind_of(section), section.data))
        .collect();
    let mut piece_start = Vec::with_capacity(sections.len() + 1);
    piece_start.push(0usize);
    let mut total = 0usize;
    for (index, count) in counts.iter().enumerate() {
        match *count {
            Ok(count) => {
                total = total
                    .checked_add(count)
                    .ok_or(InputError::TooLarge("merge piece count"))?;
                piece_start.push(total);
            }
            Err((offset, problem)) => {
                return Err(MergeError::Malformed(MalformedMerge {
                    section: index,
                    offset,
                    problem,
                }));
            }
        }
    }
    drop(counts);
    if u32::try_from(total).is_err() {
        return Err(InputError::TooLarge("merge piece count").into());
    }
    let mut pieces = vec![Piece::default(); total];
    let mut unit = vec![(); sections.len()];
    for_each_row_mut(&piece_start, &mut pieces, &mut unit, &|row, slots, ()| {
        let section = &sections[row];
        split_pieces(kind_of(section), section.group, section.data, slots);
    });

    let split = Split {
        sections,
        piece_start: &piece_start,
        pieces: &pieces,
    };

    // 2. Deduplicate: keep the lowest piece number for each content. Each
    // distinct content gets a slot in its shard's `leaders` vector; a piece
    // records its (shard, slot) so that finding its leader afterwards needs
    // no second hash probe.
    let shards: Vec<Mutex<Shard<'a>>> = (0..SHARDS)
        .map(|_| {
            let capacity = total / SHARDS / 2;
            Mutex::new(Shard {
                table: HashTable::with_capacity(capacity),
                leaders: Vec::with_capacity(capacity),
            })
        })
        .collect();
    let mut slot_of = vec![0u64; total];
    for_each_row_mut(
        &piece_start,
        &mut slot_of,
        &mut unit,
        &|section, slots, ()| {
            let row = split.row(section);
            let group = sections[section].group;
            let base = split.piece_start[section];
            slots
                .par_iter_mut()
                .with_min_len(MIN_PIECES_PER_TASK)
                .enumerate()
                .for_each(|(local, slot_ref)| {
                    let bytes = split.bytes(section, local, row);
                    let hash = row[local].hash;
                    let index = (base + local) as u32;
                    let shard_index = shard_of(hash);
                    let mut guard = shards[shard_index]
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner());
                    let shard = &mut *guard;
                    let entry = shard.table.entry(
                        hash,
                        |unique| {
                            unique.hash == hash && unique.group == group && unique.bytes == bytes
                        },
                        |unique| unique.hash,
                    );
                    let slot = match entry {
                        Entry::Occupied(occupied) => {
                            let slot = occupied.get().slot;
                            let leader = &mut shard.leaders[slot as usize];
                            *leader = (*leader).min(index);
                            slot
                        }
                        Entry::Vacant(vacant) => {
                            // Fewer slots than pieces, so this fits in u32.
                            let slot = shard.leaders.len() as u32;
                            vacant.insert(Unique {
                                hash,
                                group,
                                slot,
                                bytes,
                            });
                            shard.leaders.push(index);
                            slot
                        }
                    };
                    *slot_ref = ((shard_index as u64) << 32) | u64::from(slot);
                });
        },
    );
    let shard_leaders: Vec<Vec<u32>> = shards
        .into_par_iter()
        .map(|shard| {
            shard
                .into_inner()
                .unwrap_or_else(|poison| poison.into_inner())
                .leaders
        })
        .collect();
    let leader: Vec<u32> = slot_of
        .par_iter()
        .with_min_len(MIN_PIECES_PER_TASK)
        .map(|&slot_ref| shard_leaders[(slot_ref >> 32) as usize][slot_ref as u32 as usize])
        .collect();
    drop(slot_of);
    drop(shard_leaders);

    // 3 and 4. Tail merge and lay out each group.
    let mut members = CsrBuilder::with_capacity(groups.len(), sections.len());
    for (index, section) in sections.iter().enumerate() {
        members.push(section.group as usize, index);
    }
    let members = members.build()?;
    let out_offset: Vec<AtomicU64> = (0..total)
        .into_par_iter()
        .map(|_| AtomicU64::new(0))
        .collect();
    let merged: Vec<Result<MergedGroup, InputError>> = groups
        .par_iter()
        .enumerate()
        .map(|(group_index, group)| {
            layout_group(
                group,
                members.row(group_index),
                &split,
                &leader,
                &out_offset,
            )
        })
        .collect();
    let groups = merged.into_iter().collect::<Result<Vec<_>, _>>()?;

    // Every other piece takes its leader's offset. Leaders are only read.
    (0..total)
        .into_par_iter()
        .with_min_len(MIN_PIECES_PER_TASK)
        .for_each(|index| {
            let lead = leader[index] as usize;
            if lead != index {
                let value = out_offset[lead].load(Ordering::Relaxed);
                out_offset[index].store(value, Ordering::Relaxed);
            }
        });

    Ok(MergedSections {
        sections: sections.to_vec(),
        groups,
        piece_start,
        input_offset: pieces.par_iter().map(|piece| piece.offset).collect(),
        output_offset: out_offset
            .into_par_iter()
            .map(AtomicU64::into_inner)
            .collect(),
    })
}

/// Lays out one group: collects its leaders in input order, tail merges
/// them if asked, assigns offsets, and records them in `out_offset`.
fn layout_group(
    group: &MergeGroup,
    members: &[usize],
    split: &Split<'_, '_>,
    leader: &[u32],
    out_offset: &[AtomicU64],
) -> Result<MergedGroup, InputError> {
    // Leaders of this group, in input order.
    let mut leaders: Vec<Leader<'_>> = Vec::new();
    for &section in members {
        let row = split.row(section);
        let base = split.piece_start[section];
        for (local, piece) in row.iter().enumerate() {
            let index = base + local;
            if leader[index] as usize == index {
                leaders.push(Leader {
                    piece: index as u32,
                    section: section as u32,
                    input_offset: piece.offset,
                    bytes: split.bytes(section, local, row),
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

    fn contents(merged: &MergedSections<'_>, group: usize) -> Vec<u8> {
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
        assert_eq!(found.addend, 2);
        assert_eq!(merged.piece_output_offset(found.piece), Some(0));
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
        // "a\0" is a byte suffix of "\0a\0\0"? No: pieces are "a", "ba".
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
            MergeError::Malformed(MalformedMerge {
                section: 0,
                offset: 2,
                problem: MergeProblem::SizeNotMultiple { unit: 2 },
            })
        );
        let err = merge_sections(&[strings(false)], &[unterminated]).unwrap_err();
        assert_eq!(
            err,
            MergeError::Malformed(MalformedMerge {
                section: 0,
                offset: 3,
                problem: MergeProblem::UnterminatedString,
            })
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
        if let MergeError::Malformed(malformed) =
            merge_sections(&[strings(false)], &[unterminated]).unwrap_err()
        {
            let error = malformed.into_error("a.o", 0x40);
            assert!(error.to_string().contains("0x43"));
        }
    }
}
