//! Phase 1 of section merging: splitting one input section into pieces.
//!
//! Runs at parse time (pipeline stage 4), once per mergeable input section,
//! from inside the backend's parallel per-file parsing. Each call is
//! independent of every other section, so the backend can split sections on
//! whatever thread parses their file.
//!
//! A [`SplitSection`] borrows the section's bytes and stores, per piece, its
//! start offset (`u32`, strings only; fixed-size entries compute it) and a
//! fixed-seed hash of its bytes (`u64`): 12 bytes per string piece, 8 per
//! fixed-size entry, in two allocations per section at most. Before any
//! deduplication it already maps an input offset to (piece, offset within
//! the piece), which is what the relocation scan (stage 6) records.

use std::fmt;
use std::hash::Hasher;
use std::path::PathBuf;

use rayon::prelude::*;

use super::MergeKind;
use crate::passes::hash::hasher;

/// Minimum number of pieces per parallel task within one section. Sections
/// with fewer pieces are split sequentially on the calling thread.
pub(super) const MIN_PIECES_PER_TASK: usize = 1024;

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
    /// The character size is not 1, 2 or 4, or the entry size is zero.
    InvalidEntrySize {
        /// The character or entry size given.
        size: u64,
    },
    /// The alignment is not a power of two.
    InvalidAlignment {
        /// The alignment given.
        alignment: u64,
    },
    /// The section is larger than 4 GiB, which piece offsets cannot express.
    TooLarge {
        /// The section size in bytes.
        size: u64,
    },
}

/// A malformed mergeable input section, as found by [`split_section`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MalformedMerge {
    /// Offset within the section where the problem starts (0 for problems
    /// with the section as a whole: its entry size, alignment or size).
    pub offset: u64,
    /// What is wrong.
    pub problem: MergeProblem,
}

impl MalformedMerge {
    /// Converts into a fatal [`crate::Error`] for `file`, given the file
    /// offset at which the section's data starts.
    ///
    /// This is [`crate::Error::Malformed`], except for
    /// [`MergeProblem::TooLarge`], which is a [`crate::Error::Limit`]. There
    /// is no `From` conversion, because the error must name the file.
    #[must_use]
    pub fn into_error(self, file: impl Into<PathBuf>, section_file_offset: u64) -> crate::Error {
        let file = file.into();
        match self.problem {
            MergeProblem::TooLarge { .. } => {
                crate::Error::Limit(format!("{}: {self}", file.display()))
            }
            _ => crate::Error::malformed(
                file,
                section_file_offset.saturating_add(self.offset),
                self.to_string(),
            ),
        }
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
            MergeProblem::InvalidEntrySize { size } => {
                write!(f, "mergeable section: invalid entry size {size}")
            }
            MergeProblem::InvalidAlignment { alignment } => write!(
                f,
                "mergeable section: alignment {alignment} is not a power of two"
            ),
            MergeProblem::TooLarge { size } => {
                write!(f, "mergeable section of {size} bytes is larger than 4 GiB")
            }
        }
    }
}

impl std::error::Error for MalformedMerge {}

/// A location inside a piece of one split section.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PieceRef {
    /// Zero-based index of the piece within its section.
    pub piece: u32,
    /// Offset of the location within the piece.
    pub addend: u64,
}

/// One mergeable input section, split into pieces (phase 1).
///
/// Built by [`split_section`]; consumed by
/// [`merge_split_sections`](super::merge_split_sections). Borrows the
/// section's bytes, which stay zero-copy in the input mapping.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SplitSection<'a> {
    data: &'a [u8],
    kind: MergeKind,
    alignment: u64,
    /// Start offset of each piece, increasing. Strings only: empty for
    /// fixed-size entries, whose starts are multiples of the entry size.
    starts: Box<[u32]>,
    /// Fixed-seed hash of each piece's bytes.
    hashes: Box<[u64]>,
}

impl<'a> SplitSection<'a> {
    /// The section's bytes.
    #[must_use]
    pub fn data(&self) -> &'a [u8] {
        self.data
    }

    /// How the section was split.
    #[must_use]
    pub fn kind(&self) -> MergeKind {
        self.kind
    }

    /// The section's alignment, as given to [`split_section`].
    #[must_use]
    pub fn alignment(&self) -> u64 {
        self.alignment
    }

    /// Number of pieces.
    #[must_use]
    pub fn num_pieces(&self) -> usize {
        self.hashes.len()
    }

    /// The fixed-seed hash of every piece's bytes, in piece order. Equal
    /// bytes always have equal hashes, in any section.
    #[must_use]
    pub fn hashes(&self) -> &[u64] {
        &self.hashes
    }

    /// Start offset of `piece` in the section, or `None` if there is no such
    /// piece.
    #[must_use]
    pub fn piece_start(&self, piece: usize) -> Option<u64> {
        if piece >= self.num_pieces() {
            return None;
        }
        Some(self.bounds(piece).0 as u64)
    }

    /// The bytes of `piece`, including a string's terminator, or `None` if
    /// there is no such piece.
    #[must_use]
    pub fn piece_bytes(&self, piece: usize) -> Option<&'a [u8]> {
        if piece >= self.num_pieces() {
            return None;
        }
        Some(self.bytes_of(piece))
    }

    /// Finds the piece containing `offset` and the offset within that piece.
    /// `None` if `offset` is at or past the end of the section.
    ///
    /// This needs no deduplication: the relocation scan uses it to record a
    /// reference into a merge section as (piece, addend within piece).
    #[must_use]
    pub fn piece_at(&self, offset: u64) -> Option<PieceRef> {
        if offset >= self.data.len() as u64 {
            return None;
        }
        match self.kind {
            MergeKind::Strings { .. } => {
                // The section fits in 4 GiB, so the offset fits in u32.
                let offset = u32::try_from(offset).ok()?;
                let piece = self
                    .starts
                    .partition_point(|&start| start <= offset)
                    .checked_sub(1)?;
                let start = *self.starts.get(piece)?;
                Some(PieceRef {
                    piece: u32::try_from(piece).ok()?,
                    addend: u64::from(offset - start),
                })
            }
            MergeKind::Fixed { entry_size } => Some(PieceRef {
                piece: u32::try_from(offset / entry_size).ok()?,
                addend: offset % entry_size,
            }),
        }
    }

    /// Byte range of `piece`; `(0, 0)` if it does not exist. Validated at
    /// construction, so never out of the section's bounds.
    #[inline]
    pub(super) fn bounds(&self, piece: usize) -> (usize, usize) {
        let len = self.data.len();
        match self.kind {
            MergeKind::Strings { .. } => match self.starts.get(piece) {
                Some(&start) => {
                    let end = self
                        .starts
                        .get(piece.wrapping_add(1))
                        .map_or(len, |&next| next as usize);
                    (start as usize, end)
                }
                None => (0, 0),
            },
            MergeKind::Fixed { entry_size } => {
                // `entry_size` divides `len`, which fits in u32.
                let unit = entry_size as usize;
                let start = piece.saturating_mul(unit).min(len);
                (start, start.saturating_add(unit).min(len))
            }
        }
    }

    /// The bytes of `piece`; empty if it does not exist.
    #[inline]
    pub(super) fn bytes_of(&self, piece: usize) -> &'a [u8] {
        let (start, end) = self.bounds(piece);
        self.data.get(start..end).unwrap_or(&[])
    }
}

/// Hashes one piece's bytes with the passes' fixed seed.
#[inline]
fn hash_piece(bytes: &[u8]) -> u64 {
    let mut h = hasher();
    h.write(bytes);
    h.finish()
}

/// Splits one mergeable section into pieces and hashes each piece (phase 1).
///
/// `kind` says how to split: NUL-terminated strings of 1-, 2- or 4-byte
/// characters (the terminator is part of the piece; a character is a
/// terminator when all its bytes are zero), or fixed-size entries.
/// `alignment` is the section's alignment (map ELF's `sh_addralign` of 0 to
/// 1); it is recorded and checked against the output group in phase 2.
///
/// Independent of every other section, so it can run on any thread. Large
/// sections are hashed in parallel on the current rayon pool; the result
/// never depends on the thread count.
///
/// # Errors
///
/// [`MalformedMerge`] if the alignment is not a power of two, the character
/// size is not 1, 2 or 4, the entry size is zero, the section is larger than
/// 4 GiB, its size is not a multiple of the character or entry size, or the
/// last string is not terminated.
pub fn split_section(
    data: &[u8],
    kind: MergeKind,
    alignment: u64,
) -> Result<SplitSection<'_>, MalformedMerge> {
    let fail = |offset, problem| Err(MalformedMerge { offset, problem });
    if !alignment.is_power_of_two() {
        return fail(0, MergeProblem::InvalidAlignment { alignment });
    }
    match kind {
        MergeKind::Strings { char_size } if !matches!(char_size, 1 | 2 | 4) => {
            return fail(
                0,
                MergeProblem::InvalidEntrySize {
                    size: u64::from(char_size),
                },
            );
        }
        MergeKind::Fixed { entry_size: 0 } => {
            return fail(0, MergeProblem::InvalidEntrySize { size: 0 });
        }
        _ => {}
    }
    if u32::try_from(data.len()).is_err() {
        return fail(
            0,
            MergeProblem::TooLarge {
                size: data.len() as u64,
            },
        );
    }
    let count =
        count_pieces(kind, data).map_err(|(offset, problem)| MalformedMerge { offset, problem })?;

    let mut hashes = vec![0u64; count].into_boxed_slice();
    let starts = match kind {
        MergeKind::Strings { char_size } => {
            let mut starts = vec![0u32; count].into_boxed_slice();
            split_strings(data, usize::from(char_size), &mut starts, &mut hashes);
            starts
        }
        MergeKind::Fixed { entry_size } => {
            // Validated by `count_pieces`: fits usize and divides the length.
            let unit = entry_size as usize;
            if count <= MIN_PIECES_PER_TASK {
                for (slot, bytes) in hashes.iter_mut().zip(data.chunks_exact(unit)) {
                    *slot = hash_piece(bytes);
                }
            } else {
                hashes
                    .par_iter_mut()
                    .with_min_len(MIN_PIECES_PER_TASK)
                    .zip(data.par_chunks_exact(unit))
                    .for_each(|(slot, bytes)| *slot = hash_piece(bytes));
            }
            Box::default()
        }
    };
    Ok(SplitSection {
        data,
        kind,
        alignment,
        starts,
        hashes,
    })
}

/// Counts the pieces of one section, validating its size and terminator.
/// The kind is already validated.
fn count_pieces(kind: MergeKind, data: &[u8]) -> Result<usize, (u64, MergeProblem)> {
    let len = data.len();
    match kind {
        MergeKind::Strings { char_size } => {
            let unit = usize::from(char_size).max(1);
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
            let unit = usize::try_from(entry_size)
                .ok()
                .filter(|&unit| unit != 0)
                .ok_or(size_error)?;
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

/// Records the start and hash of every string of a validated section.
/// `starts` and `hashes` have one slot per string; the section fits in u32.
fn split_strings(data: &[u8], unit: usize, starts: &mut [u32], hashes: &mut [u64]) {
    let parallel = starts.len() > MIN_PIECES_PER_TASK;
    let mut slots = starts.iter_mut().zip(hashes.iter_mut());
    let mut start = 0usize;
    let mut record = |end: usize| {
        if let Some((slot_start, slot_hash)) = slots.next() {
            *slot_start = start as u32;
            if !parallel {
                *slot_hash = hash_piece(data.get(start..end).unwrap_or(&[]));
            }
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
    if parallel {
        let starts = &*starts;
        hashes
            .par_iter_mut()
            .with_min_len(MIN_PIECES_PER_TASK)
            .enumerate()
            .for_each(|(piece, slot)| {
                let begin = starts.get(piece).map_or(0, |&s| s as usize);
                let end = starts
                    .get(piece + 1)
                    .map_or(data.len(), |&next| next as usize);
                *slot = hash_piece(data.get(begin..end).unwrap_or(&[]));
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STRINGS: MergeKind = MergeKind::Strings { char_size: 1 };

    #[test]
    fn maps_offsets_to_pieces_before_dedup() {
        let split = split_section(b"foo\0\0bar\0", STRINGS, 1).unwrap();
        assert_eq!(split.num_pieces(), 3);
        assert_eq!(split.piece_start(1), Some(4));
        assert_eq!(split.piece_bytes(2), Some(&b"bar\0"[..]));
        assert_eq!(split.piece_bytes(3), None);
        assert_eq!(
            split.piece_at(0),
            Some(PieceRef {
                piece: 0,
                addend: 0
            })
        );
        assert_eq!(
            split.piece_at(3),
            Some(PieceRef {
                piece: 0,
                addend: 3
            })
        );
        assert_eq!(
            split.piece_at(4),
            Some(PieceRef {
                piece: 1,
                addend: 0
            })
        );
        assert_eq!(
            split.piece_at(7),
            Some(PieceRef {
                piece: 2,
                addend: 2
            })
        );
        assert_eq!(split.piece_at(9), None);
        assert_ne!(split.hashes()[0], split.hashes()[2]);
        let again = split_section(b"bar\0", STRINGS, 1).unwrap();
        assert_eq!(again.hashes()[0], split.hashes()[2]);
    }

    #[test]
    fn fixed_entries_compute_offsets() {
        let split =
            split_section(&[1, 2, 3, 4, 1, 2], MergeKind::Fixed { entry_size: 2 }, 2).unwrap();
        assert_eq!(split.num_pieces(), 3);
        assert_eq!(
            split.piece_at(5),
            Some(PieceRef {
                piece: 2,
                addend: 1
            })
        );
        assert_eq!(split.piece_start(2), Some(4));
        assert_eq!(split.hashes()[0], split.hashes()[2]);
        assert_eq!(split.piece_at(6), None);
    }

    #[test]
    fn large_sections_split_in_parallel_identically() {
        let mut data = Vec::new();
        for i in 0..10_000u32 {
            data.extend_from_slice(format!("s{}", i % 777).as_bytes());
            data.push(0);
        }
        let split = split_section(&data, STRINGS, 1).unwrap();
        assert_eq!(split.num_pieces(), 10_000);
        for piece in 0..split.num_pieces() {
            let bytes = split.piece_bytes(piece).unwrap();
            assert_eq!(split.hashes()[piece], hash_piece(bytes));
        }
        let fixed = split_section(&data[..8000], MergeKind::Fixed { entry_size: 4 }, 1).unwrap();
        for piece in 0..fixed.num_pieces() {
            assert_eq!(
                fixed.hashes()[piece],
                hash_piece(fixed.piece_bytes(piece).unwrap())
            );
        }
    }

    #[test]
    fn rejects_malformed_sections() {
        let err = |data: &[u8], kind, alignment| split_section(data, kind, alignment).unwrap_err();
        assert_eq!(
            err(b"ok\0bad", STRINGS, 1),
            MalformedMerge {
                offset: 3,
                problem: MergeProblem::UnterminatedString
            }
        );
        assert_eq!(
            err(&[0, 0, 0], MergeKind::Strings { char_size: 2 }, 1).problem,
            MergeProblem::SizeNotMultiple { unit: 2 }
        );
        assert_eq!(
            err(b"", MergeKind::Strings { char_size: 3 }, 1).problem,
            MergeProblem::InvalidEntrySize { size: 3 }
        );
        assert_eq!(
            err(b"", MergeKind::Fixed { entry_size: 0 }, 1).problem,
            MergeProblem::InvalidEntrySize { size: 0 }
        );
        assert_eq!(
            err(b"\0", STRINGS, 3).problem,
            MergeProblem::InvalidAlignment { alignment: 3 }
        );
        let error = err(b"ok\0bad", STRINGS, 1).into_error("a.o", 0x40);
        assert!(error.to_string().contains("0x43"));
        let limit = MalformedMerge {
            offset: 0,
            problem: MergeProblem::TooLarge { size: 1 << 33 },
        };
        assert!(matches!(limit.into_error("a.o", 0), crate::Error::Limit(_)));
    }
}
