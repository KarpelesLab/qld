//! Splitting an output buffer into disjoint mutable chunks.
//!
//! Layout assigns every output chunk (a section, a header, a synthetic
//! table) a file offset and size. [`split_chunks`] turns that list into
//! non-overlapping `&mut [u8]` slices with [`slice::split_at_mut`], so each
//! chunk can be written from its own rayon task without `unsafe` and without
//! locks. A bad layout is reported as a [`LayoutError`], never a panic.
//!
//! Bytes that fall between chunks are not handed out. They stay zero,
//! because every buffer [`crate::output::OutputFile`] creates starts zeroed:
//! a new file after `set_len` reads as zeros, and the in-memory buffer is
//! zero-filled when it is allocated.

#![deny(clippy::arithmetic_side_effects)]

use rayon::prelude::*;
use std::fmt;

/// The position of one output chunk in the file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChunkRange {
    /// File offset of the first byte.
    pub offset: u64,
    /// Length in bytes. Zero-sized chunks are allowed.
    pub size: u64,
}

impl ChunkRange {
    /// Creates a range from an offset and a size.
    #[must_use]
    pub const fn new(offset: u64, size: u64) -> Self {
        Self { offset, size }
    }

    /// One past the last byte, or `None` if that overflows `u64`.
    #[must_use]
    pub const fn end(&self) -> Option<u64> {
        self.offset.checked_add(self.size)
    }
}

impl From<(u64, u64)> for ChunkRange {
    fn from((offset, size): (u64, u64)) -> Self {
        Self { offset, size }
    }
}

/// Why a chunk layout could not be split.
///
/// These indicate a bug in layout rather than bad input, but they are still
/// returned as errors so a broken layout cannot crash the linker.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LayoutError {
    /// A chunk extends past the end of the buffer (or its end overflows).
    OutOfBounds {
        /// Index of the chunk in the layout.
        index: usize,
        /// The chunk.
        range: ChunkRange,
        /// Size of the buffer.
        len: u64,
    },
    /// A chunk starts before the previous chunk's offset.
    Unsorted {
        /// Index of the chunk in the layout.
        index: usize,
        /// The chunk.
        range: ChunkRange,
        /// Offset of the chunk before it.
        previous_offset: u64,
    },
    /// A chunk starts inside the previous chunk.
    Overlap {
        /// Index of the chunk in the layout.
        index: usize,
        /// The chunk.
        range: ChunkRange,
        /// End of the chunk before it.
        previous_end: u64,
    },
    /// A fixed-size field to patch after writing (such as the build-id)
    /// does not fit in the image.
    FieldOutOfBounds {
        /// The field.
        range: ChunkRange,
        /// Size of the image.
        len: u64,
    },
}

impl fmt::Display for LayoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutOfBounds { index, range, len } => write!(
                f,
                "output chunk {index} (offset {:#x}, size {:#x}) extends past the end of \
                 the {len:#x}-byte output",
                range.offset, range.size
            ),
            Self::Unsorted {
                index,
                range,
                previous_offset,
            } => write!(
                f,
                "output chunk {index} at offset {:#x} precedes the previous chunk at {previous_offset:#x}",
                range.offset
            ),
            Self::Overlap {
                index,
                range,
                previous_end,
            } => write!(
                f,
                "output chunk {index} at offset {:#x} overlaps the previous chunk, which ends at \
                 {previous_end:#x}",
                range.offset
            ),
            Self::FieldOutOfBounds { range, len } => write!(
                f,
                "{}-byte field at offset {:#x} does not fit in the {len:#x}-byte output",
                range.size, range.offset
            ),
        }
    }
}

impl std::error::Error for LayoutError {}

/// Checks that `ranges` is sorted by offset, non-overlapping and inside a
/// buffer of `len` bytes.
///
/// # Errors
///
/// Returns the first problem found, in layout order.
pub fn validate_layout(ranges: &[ChunkRange], len: u64) -> Result<(), LayoutError> {
    let mut previous = ChunkRange::default();
    let mut previous_end = 0u64;
    for (index, &range) in ranges.iter().enumerate() {
        let end = match range.end() {
            Some(end) if end <= len => end,
            _ => return Err(LayoutError::OutOfBounds { index, range, len }),
        };
        if index != 0 {
            if range.offset < previous.offset {
                return Err(LayoutError::Unsorted {
                    index,
                    range,
                    previous_offset: previous.offset,
                });
            }
            if range.offset < previous_end {
                return Err(LayoutError::Overlap {
                    index,
                    range,
                    previous_end,
                });
            }
        }
        previous = range;
        previous_end = end;
    }
    Ok(())
}

/// Splits `buf` into one mutable slice per range, in layout order.
///
/// `ranges` must be sorted by offset and must not overlap; gaps between them
/// are allowed and are simply not handed out. The returned slices are
/// disjoint, so they can be written concurrently:
///
/// ```
/// use qld::output::{ChunkRange, split_chunks};
/// use rayon::prelude::*;
///
/// let mut image = vec![0u8; 16];
/// let layout = [ChunkRange::new(0, 4), ChunkRange::new(8, 8)];
/// let chunks = split_chunks(&mut image, &layout)?;
/// chunks
///     .into_par_iter()
///     .enumerate()
///     .for_each(|(i, chunk)| chunk.fill(i as u8 + 1));
/// assert_eq!(image, [1, 1, 1, 1, 0, 0, 0, 0, 2, 2, 2, 2, 2, 2, 2, 2]);
/// # Ok::<(), qld::output::LayoutError>(())
/// ```
///
/// # Errors
///
/// Returns a [`LayoutError`] if a range is out of bounds, out of order, or
/// overlaps its predecessor. Nothing is split in that case.
pub fn split_chunks<'a>(
    buf: &'a mut [u8],
    ranges: &[ChunkRange],
) -> Result<Vec<&'a mut [u8]>, LayoutError> {
    let len = buf.len() as u64;
    validate_layout(ranges, len)?;

    let mut out = Vec::with_capacity(ranges.len());
    let mut rest = buf;
    // File offset of `rest[0]`.
    let mut cursor = 0u64;
    for (index, &range) in ranges.iter().enumerate() {
        let oob = || LayoutError::OutOfBounds { index, range, len };
        let gap = range
            .offset
            .checked_sub(cursor)
            .and_then(|gap| usize::try_from(gap).ok())
            .ok_or_else(oob)?;
        let size = usize::try_from(range.size).map_err(|_| oob())?;
        let (_, after_gap) = std::mem::take(&mut rest)
            .split_at_mut_checked(gap)
            .ok_or_else(oob)?;
        let (chunk, after_chunk) = after_gap.split_at_mut_checked(size).ok_or_else(oob)?;
        out.push(chunk);
        rest = after_chunk;
        cursor = range.end().ok_or_else(oob)?;
    }
    Ok(out)
}

/// Splits `buf` by `ranges` and calls `write(index, chunk)` for every chunk
/// on the current rayon pool.
///
/// The layout is validated before any chunk is written. Results do not
/// depend on the number of threads as long as `write` only touches its own
/// chunk.
///
/// # Errors
///
/// Returns a [`LayoutError`] for an invalid layout, or the first error (in
/// layout order) returned by `write`.
pub fn write_chunks<E, F>(buf: &mut [u8], ranges: &[ChunkRange], write: F) -> Result<(), E>
where
    E: From<LayoutError> + Send,
    F: Fn(usize, &mut [u8]) -> Result<(), E> + Sync,
{
    let chunks = split_chunks(buf, ranges)?;
    let results: Vec<Result<(), E>> = chunks
        .into_par_iter()
        .enumerate()
        .map(|(index, chunk)| write(index, chunk))
        .collect();
    results.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(pairs: &[(u64, u64)]) -> Vec<ChunkRange> {
        pairs.iter().copied().map(ChunkRange::from).collect()
    }

    #[test]
    fn splits_with_gaps() {
        let mut buf: Vec<u8> = (0..20).collect();
        let chunks = split_chunks(&mut buf, &layout(&[(2, 3), (5, 0), (5, 1), (10, 10)])).unwrap();
        let lens: Vec<usize> = chunks.iter().map(|c| c.len()).collect();
        assert_eq!(lens, [3, 0, 1, 10]);
        assert_eq!(chunks[0], [2, 3, 4]);
        assert_eq!(chunks[2], [5]);
        assert_eq!(chunks[3][0], 10);
    }

    #[test]
    fn empty_layout_and_empty_buffer() {
        let mut buf = [0u8; 0];
        assert!(split_chunks(&mut buf, &[]).unwrap().is_empty());
        assert_eq!(split_chunks(&mut buf, &layout(&[(0, 0)])).unwrap().len(), 1);
    }

    #[test]
    fn rejects_bad_layouts() {
        let mut buf = [0u8; 16];
        assert!(matches!(
            split_chunks(&mut buf, &layout(&[(0, 17)])),
            Err(LayoutError::OutOfBounds { index: 0, .. })
        ));
        assert!(matches!(
            split_chunks(&mut buf, &layout(&[(0, 4), (17, 0)])),
            Err(LayoutError::OutOfBounds { index: 1, .. })
        ));
        assert!(matches!(
            split_chunks(&mut buf, &layout(&[(u64::MAX, 2)])),
            Err(LayoutError::OutOfBounds { index: 0, .. })
        ));
        assert!(matches!(
            split_chunks(&mut buf, &layout(&[(0, 8), (4, 4)])),
            Err(LayoutError::Overlap {
                index: 1,
                previous_end: 8,
                ..
            })
        ));
        assert!(matches!(
            split_chunks(&mut buf, &layout(&[(8, 4), (0, 4)])),
            Err(LayoutError::Unsorted { index: 1, .. })
        ));
        // A zero-sized chunk inside another chunk still overlaps it.
        assert!(matches!(
            split_chunks(&mut buf, &layout(&[(0, 8), (3, 0)])),
            Err(LayoutError::Overlap { index: 1, .. })
        ));
    }

    #[test]
    fn errors_display() {
        let err = LayoutError::Overlap {
            index: 3,
            range: ChunkRange::new(0x10, 4),
            previous_end: 0x12,
        };
        assert!(err.to_string().contains("overlaps"));
    }

    #[test]
    fn write_chunks_reports_first_error_in_layout_order() {
        #[derive(Debug, PartialEq)]
        enum E {
            Layout,
            Chunk(usize),
        }
        impl From<LayoutError> for E {
            fn from(_: LayoutError) -> Self {
                E::Layout
            }
        }
        let mut buf = vec![0u8; 64];
        let ranges: Vec<ChunkRange> = (0..16).map(|i| ChunkRange::new(i * 4, 3)).collect();
        let result = write_chunks(&mut buf, &ranges, |i, chunk| {
            chunk.fill(0xaa);
            if i % 5 == 4 { Err(E::Chunk(i)) } else { Ok(()) }
        });
        assert_eq!(result, Err(E::Chunk(4)));
        let result = write_chunks(&mut buf, &layout(&[(0, 65)]), |_, _| Ok::<(), E>(()));
        assert_eq!(result, Err(E::Layout));
    }
}
