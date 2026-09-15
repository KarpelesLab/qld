//! The write backing: chunks are rendered into heap buffers owned by the
//! worker and written to the output with positional writes (`pwrite`).
//!
//! # Regions
//!
//! Writing every chunk with its own system call would cost more than the
//! copy for the many small input sections of a typical link, so consecutive
//! chunks are grouped into *regions* written from one buffer: a region holds
//! the chunks whose last byte falls in the same build-id block
//! ([`BLOCK_SIZE`], 1 MiB). A region's buffer spans its first chunk's start
//! to its last chunk's end, gaps included (they stay zero), so regions are
//! about 1 MiB, or one large chunk plus whatever ends in the same block.
//! Regions are filled in parallel, and the chunks of a region in parallel
//! within it; buffers over [`WRITE_PIECE`] bytes are written in parallel
//! pieces. Bytes outside every region are never written and read as zeros,
//! since the file was just created with `set_len`.
//!
//! # Hashing while writing
//!
//! A build-id is a hash of 1 MiB block digests over the finished image with
//! the build-id field zeroed (see [`super::build_id`]). With the mapped and
//! buffered backings the image is in memory afterwards; here it is not. When
//! the build-id is announced before the chunks are written
//! ([`super::OutputFile::reserve_build_id`]), each region worker hashes its
//! buffer after writing it, so the image never has to be read back:
//!
//! - The first region that has bytes in a block *leads* it: everything before
//!   its bytes in that block is a gap, hence zeros. It hashes the zeros, its
//!   bytes, and the zeros up to the next region's start. If that reaches the
//!   end of the block, the block digest is done; otherwise it keeps the
//!   incremental hasher state (a few hundred bytes).
//! - A later region with bytes in the same block keeps a copy of those bytes.
//!   Since regions are grouped by the block of their last byte, this only
//!   happens when a region's first chunk straddles a block boundary, so the
//!   copy is the head of that one chunk. When a copy would exceed
//!   [`RETAIN_LIMIT`], the block is read back from the file instead; that is
//!   decided from the layout before writing, so neither region hashes it.
//!
//! Once all regions are written, each unfinished block resumes its leader's
//! state with the kept bytes (and zeros between them) in offset order. The
//! digests are exactly the ones the one-shot tree hash computes, so the
//! build-id is identical to the other backings'. Without an announcement,
//! or when anything else was written to the file, the build-id is computed
//! by reading the blocks back in parallel.

#![deny(clippy::arithmetic_side_effects)]

use super::build_id::{BLOCK_SIZE, BlockHasher, combine_digests};
use super::chunks::{ChunkRange, split_chunks};
use super::positional::{read_exact_at, write_all_at};
use crate::args::BuildId;
use crate::error::{Error, Result};
use rayon::prelude::*;
use std::fs::File;
use std::io;
use std::path::Path;

const BLOCK: u64 = BLOCK_SIZE as u64;

/// Buffers larger than this are written (and read) in parallel pieces of
/// this size.
pub(super) const WRITE_PIECE: usize = 8 << 20;

/// Largest copy of a straddling chunk's head kept for the build-id; above
/// it, the block is read back from the file.
const RETAIN_LIMIT: u64 = 64 << 10;

/// A content-derived build-id to compute while writing: its mode and the
/// field that is zeroed while hashing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct HashPlan {
    pub(super) kind: BuildId,
    pub(super) field: ChunkRange,
}

/// Block digests computed while writing, to be combined by [`finish_build_id`].
#[derive(Debug)]
pub(super) struct Precomputed {
    pub(super) plan: HashPlan,
    parts: Vec<BlockPart>,
}

/// What a region worker learned about one block.
#[derive(Debug)]
enum BlockPart {
    /// The block's digest.
    Digest(u64, Vec<u8>),
    /// The leader's hasher, having consumed the block up to file offset `at`.
    Open {
        block: u64,
        at: u64,
        hasher: BlockHasher,
    },
    /// Bytes of a non-leading region, at file offset `offset`.
    Piece {
        block: u64,
        offset: u64,
        bytes: Vec<u8>,
    },
    /// The block must be read back from the file.
    Missing(u64),
}

/// Consecutive chunks written from one buffer.
#[derive(Clone, Copy, Debug)]
struct Region {
    start: u64,
    end: u64,
    first: usize,
    count: usize,
}

/// Groups `ranges` (a validated layout) into regions by the block of each
/// chunk's last byte.
fn plan_regions(ranges: &[ChunkRange]) -> Vec<Region> {
    let mut regions: Vec<Region> = Vec::new();
    let mut previous_key = None;
    for (index, range) in ranges.iter().enumerate() {
        // A zero-sized chunk counts as its offset.
        let last = range.offset.saturating_add(range.size.saturating_sub(1));
        let key = last.checked_div(BLOCK).unwrap_or(0);
        let end = range.offset.saturating_add(range.size);
        match regions.last_mut() {
            Some(region) if previous_key == Some(key) => {
                region.end = region.end.max(end);
                region.count = region.count.saturating_add(1);
            }
            _ => regions.push(Region {
                start: range.offset,
                end,
                first: index,
                count: 1,
            }),
        }
        previous_key = Some(key);
    }
    regions
}

/// Zeroes the part of `field` that falls in `buf`, which starts at file
/// offset `start`.
fn zero_overlap(buf: &mut [u8], start: u64, field: ChunkRange) {
    let Some(field_end) = field.end() else {
        return;
    };
    let buf_end = start.saturating_add(buf.len() as u64);
    let lo = field.offset.max(start);
    let hi = field_end.min(buf_end);
    if lo >= hi {
        return;
    }
    let (Ok(lo), Ok(hi)) = (
        usize::try_from(lo.saturating_sub(start)),
        usize::try_from(hi.saturating_sub(start)),
    ) else {
        return;
    };
    if let Some(bytes) = buf.get_mut(lo..hi) {
        bytes.fill(0);
    }
}

fn io_error(path: &Path, error: io::Error) -> Error {
    Error::io(path, error)
}

fn too_large() -> io::Error {
    io::Error::new(
        io::ErrorKind::OutOfMemory,
        "output region is larger than the address space",
    )
}

/// Writes `buf` at `offset`, in parallel pieces when it is large.
pub(super) fn write_buffer(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    if buf.len() <= WRITE_PIECE {
        return write_all_at(file, buf, offset);
    }
    buf.par_chunks(WRITE_PIECE)
        .enumerate()
        .try_for_each(|(index, piece)| {
            let at = (index as u64)
                .checked_mul(WRITE_PIECE as u64)
                .and_then(|delta| offset.checked_add(delta))
                .ok_or_else(too_large)?;
            write_all_at(file, piece, at)
        })
}

/// Fills `buf` from the file at `offset`, in parallel pieces when it is
/// large.
pub(super) fn read_buffer(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    buf.par_chunks_mut(WRITE_PIECE)
        .enumerate()
        .try_for_each(|(index, piece)| {
            let at = (index as u64)
                .checked_mul(WRITE_PIECE as u64)
                .and_then(|delta| offset.checked_add(delta))
                .ok_or_else(too_large)?;
            read_exact_at(file, piece, at)
        })
}

/// Result of one region.
struct RegionOutcome {
    /// The first chunk error in layout order.
    chunk_error: Option<Error>,
    io_error: Option<io::Error>,
    parts: Vec<BlockPart>,
}

/// Runs `write` for every chunk of a validated layout and writes the chunks
/// to `file` (of `len` bytes). With a `plan`, also computes the build-id's
/// block digests, which is only valid if nothing else was written to the
/// file.
///
/// # Errors
///
/// The first chunk error in layout order; otherwise the first I/O error, in
/// layout order, as [`Error::Io`] naming `path`.
pub(super) fn write_chunks<F>(
    file: &File,
    path: &Path,
    len: u64,
    ranges: &[ChunkRange],
    write: F,
    plan: Option<&HashPlan>,
) -> Result<Option<Precomputed>>
where
    F: Fn(usize, &mut [u8]) -> Result<()> + Sync,
{
    let regions = plan_regions(ranges);
    // Ends and starts of the neighbouring regions that hold bytes.
    let mut previous_end = vec![0u64; regions.len()];
    let mut next_start = vec![len; regions.len()];
    let mut end = 0u64;
    for (index, region) in regions.iter().enumerate() {
        if let Some(slot) = previous_end.get_mut(index) {
            *slot = end;
        }
        if region.end > region.start {
            end = region.end;
        }
    }
    let mut start = len;
    for (index, region) in regions.iter().enumerate().rev() {
        if let Some(slot) = next_start.get_mut(index) {
            *slot = start;
        }
        if region.end > region.start {
            start = region.start;
        }
    }

    // Blocks whose non-leading bytes are too large to keep are read back;
    // that is known from the layout, so nobody hashes them while writing.
    let mut missing = Vec::new();
    if plan.is_some() {
        missing = vec![false; usize::try_from(len.div_ceil(BLOCK)).unwrap_or(0)];
        for (index, region) in regions.iter().enumerate() {
            if region.end <= region.start {
                continue;
            }
            let block = region.start.checked_div(BLOCK).unwrap_or(0);
            let (block_start, block_end) = block_span(block, len);
            let previous = previous_end.get(index).copied().unwrap_or(0);
            let head = block_end.min(region.end).saturating_sub(region.start);
            if previous > block_start
                && head > RETAIN_LIMIT
                && let Some(slot) = usize::try_from(block).ok().and_then(|b| missing.get_mut(b))
            {
                *slot = true;
            }
        }
    }

    let outcomes: Vec<RegionOutcome> = regions
        .par_iter()
        .enumerate()
        .map(|(index, region)| {
            let context = RegionContext {
                file,
                ranges,
                len,
                previous_end: previous_end.get(index).copied().unwrap_or(0),
                next_start: next_start.get(index).copied().unwrap_or(len),
                missing: &missing,
                plan,
            };
            run_region(&context, *region, &write)
        })
        .collect();

    let mut io_failure = None;
    let mut parts = Vec::new();
    for outcome in outcomes {
        if let Some(error) = outcome.chunk_error {
            return Err(error);
        }
        if io_failure.is_none() {
            io_failure = outcome.io_error;
        }
        parts.extend(outcome.parts);
    }
    if let Some(error) = io_failure {
        return Err(io_error(path, error));
    }
    Ok(plan.map(|plan| Precomputed {
        plan: plan.clone(),
        parts,
    }))
}

struct RegionContext<'a> {
    file: &'a File,
    ranges: &'a [ChunkRange],
    len: u64,
    previous_end: u64,
    next_start: u64,
    /// Blocks to read back instead of hashing, by block index.
    missing: &'a [bool],
    plan: Option<&'a HashPlan>,
}

fn run_region<F>(context: &RegionContext<'_>, region: Region, write: &F) -> RegionOutcome
where
    F: Fn(usize, &mut [u8]) -> Result<()> + Sync,
{
    let failed = |error: io::Error| RegionOutcome {
        chunk_error: None,
        io_error: Some(error),
        parts: Vec::new(),
    };
    let Ok(size) = usize::try_from(region.end.saturating_sub(region.start)) else {
        return failed(too_large());
    };
    let chunks = region
        .first
        .checked_add(region.count)
        .and_then(|end| context.ranges.get(region.first..end))
        .unwrap_or(&[]);
    let local: Vec<ChunkRange> = chunks
        .iter()
        .map(|range| ChunkRange::new(range.offset.saturating_sub(region.start), range.size))
        .collect();
    let mut buf = vec![0u8; size];
    let slices = match split_chunks(&mut buf, &local) {
        Ok(slices) => slices,
        Err(error) => {
            return RegionOutcome {
                chunk_error: Some(Error::Internal(format!("invalid output layout: {error}"))),
                io_error: None,
                parts: Vec::new(),
            };
        }
    };
    let results: Vec<Result<()>> = slices
        .into_par_iter()
        .enumerate()
        .map(|(index, chunk)| write(region.first.saturating_add(index), chunk))
        .collect();
    if let Some(error) = results.into_iter().find_map(Result::err) {
        return RegionOutcome {
            chunk_error: Some(error),
            io_error: None,
            parts: Vec::new(),
        };
    }
    if let Err(error) = write_buffer(context.file, &buf, region.start) {
        return failed(error);
    }
    let parts = match context.plan {
        Some(plan) if size > 0 => {
            zero_overlap(&mut buf, region.start, plan.field);
            hash_region(context, plan, &buf, region.start)
        }
        _ => Vec::new(),
    };
    RegionOutcome {
        chunk_error: None,
        io_error: None,
        parts,
    }
}

/// File offsets `[start, end)` of block `block` in an image of `len` bytes.
fn block_span(block: u64, len: u64) -> (u64, u64) {
    let start = block.saturating_mul(BLOCK);
    (start, start.saturating_add(BLOCK).min(len))
}

/// Hashes a region's non-empty buffer, which starts at file offset `start`,
/// following the leader rules in the module documentation.
fn hash_region(
    context: &RegionContext<'_>,
    plan: &HashPlan,
    buf: &[u8],
    start: u64,
) -> Vec<BlockPart> {
    let end = start.saturating_add(buf.len() as u64);
    let first = start.checked_div(BLOCK).unwrap_or(0);
    let last = end.saturating_sub(1).checked_div(BLOCK).unwrap_or(0);
    (first..=last)
        .into_par_iter()
        .filter_map(|block| {
            if usize::try_from(block)
                .ok()
                .and_then(|b| context.missing.get(b))
                .copied()
                .unwrap_or(false)
            {
                return Some(BlockPart::Missing(block));
            }
            let (block_start, block_end) = block_span(block, context.len);
            let lo = block_start.max(start);
            let hi = block_end.min(end);
            let data = buf.get(
                usize::try_from(lo.saturating_sub(start)).ok()?
                    ..usize::try_from(hi.saturating_sub(start)).ok()?,
            )?;
            if context.previous_end > block_start {
                // Another region already has bytes in this block (and the
                // copy is small, or the block would be missing).
                return Some(BlockPart::Piece {
                    block,
                    offset: lo,
                    bytes: data.to_vec(),
                });
            }
            let mut hasher = BlockHasher::new(&plan.kind)?;
            hasher.update_zeros(lo.saturating_sub(block_start));
            hasher.update(data);
            if hi == block_end {
                return Some(BlockPart::Digest(block, hasher.finish()));
            }
            let until = block_end.min(context.next_start);
            hasher.update_zeros(until.saturating_sub(hi));
            Some(if until == block_end {
                BlockPart::Digest(block, hasher.finish())
            } else {
                BlockPart::Open {
                    block,
                    at: until,
                    hasher,
                }
            })
        })
        .collect()
}

/// Computes a content-derived build-id of the `len`-byte file with `field`
/// zeroed, from the parts computed while writing (when given) and by reading
/// back every other block. Returns `None` if `kind` is not content-derived.
pub(super) fn finish_build_id(
    file: &File,
    len: u64,
    kind: &BuildId,
    field: ChunkRange,
    precomputed: Option<Precomputed>,
) -> io::Result<Option<Vec<u8>>> {
    if BlockHasher::new(kind).is_none() {
        return Ok(None);
    }
    let blocks = usize::try_from(len.div_ceil(BLOCK)).map_err(|_| too_large())?;
    let mut slots: Vec<Slot> = std::iter::repeat_with(Slot::default).take(blocks).collect();
    match precomputed {
        Some(precomputed) => {
            for part in precomputed.parts {
                let block = match &part {
                    BlockPart::Digest(block, _)
                    | BlockPart::Open { block, .. }
                    | BlockPart::Piece { block, .. }
                    | BlockPart::Missing(block) => *block,
                };
                let Some(slot) = usize::try_from(block).ok().and_then(|b| slots.get_mut(b)) else {
                    continue;
                };
                match part {
                    BlockPart::Digest(_, digest) => slot.digest = Some(digest),
                    BlockPart::Open { at, hasher, .. } => slot.open = Some((at, hasher)),
                    BlockPart::Piece { offset, bytes, .. } => slot.pieces.push((offset, bytes)),
                    BlockPart::Missing(_) => slot.missing = true,
                }
            }
        }
        None => slots.iter_mut().for_each(|slot| slot.missing = true),
    }
    let digests: Vec<Vec<u8>> = slots
        .into_par_iter()
        .enumerate()
        .map(|(block, slot)| block_digest(file, len, kind, field, block as u64, slot))
        .collect::<io::Result<_>>()?;
    Ok(combine_digests(kind, &digests))
}

/// What is known about one block when combining.
#[derive(Debug, Default)]
struct Slot {
    digest: Option<Vec<u8>>,
    open: Option<(u64, BlockHasher)>,
    pieces: Vec<(u64, Vec<u8>)>,
    missing: bool,
}

fn block_digest(
    file: &File,
    len: u64,
    kind: &BuildId,
    field: ChunkRange,
    block: u64,
    mut slot: Slot,
) -> io::Result<Vec<u8>> {
    let (start, end) = block_span(block, len);
    let fresh = || BlockHasher::new(kind).ok_or_else(|| io::Error::other("build-id kind"));
    if slot.missing {
        let size = usize::try_from(end.saturating_sub(start)).map_err(|_| too_large())?;
        let mut buf = vec![0u8; size];
        read_exact_at(file, &mut buf, start)?;
        zero_overlap(&mut buf, start, field);
        let mut hasher = fresh()?;
        hasher.update(&buf);
        return Ok(hasher.finish());
    }
    if let Some(digest) = slot.digest {
        return Ok(digest);
    }
    let (mut at, mut hasher) = match slot.open {
        Some(open) => open,
        None => (start, fresh()?),
    };
    slot.pieces.sort_by_key(|(offset, _)| *offset);
    for (offset, bytes) in &slot.pieces {
        hasher.update_zeros(offset.saturating_sub(at));
        hasher.update(bytes);
        at = offset.saturating_add(bytes.len() as u64).max(at);
    }
    hasher.update_zeros(end.saturating_sub(at));
    Ok(hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regions_group_by_last_byte_block() {
        let b = BLOCK;
        let ranges = [
            ChunkRange::new(0, 10),
            ChunkRange::new(10, b - 10),
            ChunkRange::new(b, 0),
            ChunkRange::new(b + 5, b),
            ChunkRange::new(2 * b + 5, 10),
            ChunkRange::new(5 * b, 3 * b),
        ];
        let regions = plan_regions(&ranges);
        let spans: Vec<(u64, u64, usize, usize)> = regions
            .iter()
            .map(|r| (r.start, r.end, r.first, r.count))
            .collect();
        assert_eq!(
            spans,
            [
                (0, b, 0, 2),
                (b, b, 2, 1),
                (b + 5, 2 * b + 15, 3, 2),
                (5 * b, 8 * b, 5, 1)
            ]
        );
    }

    /// Every kind of block part is produced, and combining them gives the
    /// one-shot tree hash.
    #[test]
    fn hashing_while_writing_covers_every_part() {
        let b = BLOCK;
        let len = 4 * b;
        let ranges = [
            // Block 0: led by the first region, finished by a small head.
            ChunkRange::new(0, b - 1000),
            ChunkRange::new(b - 990, 5000),
            // Block 1 ends with a large head: read back.
            ChunkRange::new(2 * b - 100_000, 100_010),
            // Block 2: a digest from a leader after a gap; block 3 empty.
            ChunkRange::new(2 * b + 50, 30),
        ];
        let path =
            std::env::temp_dir().join(format!("qld-written-parts-{}.tmp", std::process::id()));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        file.set_len(len).unwrap();
        let plan = HashPlan {
            kind: BuildId::Sha1,
            field: ChunkRange::new(b - 5, 20),
        };
        let fill = |index: usize, chunk: &mut [u8]| {
            for (i, byte) in chunk.iter_mut().enumerate() {
                *byte = (index + i) as u8 | 1;
            }
            Ok(())
        };
        let pre = write_chunks(&file, &path, len, &ranges, fill, Some(&plan))
            .unwrap()
            .unwrap();
        let has = |f: fn(&BlockPart) -> bool| pre.parts.iter().any(f);
        assert!(has(|p| matches!(p, BlockPart::Digest(..))));
        assert!(has(|p| matches!(p, BlockPart::Open { .. })));
        assert!(has(|p| matches!(p, BlockPart::Piece { .. })));
        assert!(has(|p| matches!(p, BlockPart::Missing(_))));

        let mut image = std::fs::read(&path).unwrap();
        image[(b - 5) as usize..(b + 15) as usize].fill(0);
        let expected = super::super::build_id::compute_build_id(&BuildId::Sha1, &image);
        let id = finish_build_id(&file, len, &BuildId::Sha1, plan.field, Some(pre)).unwrap();
        assert_eq!(id, expected);
        let id = finish_build_id(&file, len, &BuildId::Sha1, plan.field, None).unwrap();
        assert_eq!(id, expected);
        drop(file);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn zero_overlap_clips() {
        let mut buf = [1u8; 10];
        zero_overlap(&mut buf, 100, ChunkRange::new(95, 8));
        assert_eq!(buf, [0, 0, 0, 1, 1, 1, 1, 1, 1, 1]);
        let mut buf = [1u8; 10];
        zero_overlap(&mut buf, 100, ChunkRange::new(108, 8));
        assert_eq!(buf, [1, 1, 1, 1, 1, 1, 1, 1, 0, 0]);
        zero_overlap(&mut buf, 100, ChunkRange::new(u64::MAX, 8));
    }
}
