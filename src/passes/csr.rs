//! Compressed sparse rows: many variable-length lists in two flat vectors.
//!
//! The passes consume per-section lists (graph edges, ICF relocations) in
//! this form. A [`Csr`] is one offset table plus one value vector, so walking
//! row `i` is two array reads and a slice, and building it allocates twice no
//! matter how many rows there are.
//!
//! Backends can build one three ways:
//!
//! - [`Csr::build_parallel`]: count each row in parallel, then fill each row
//!   in parallel into its own disjoint slice. This is the fast path for a
//!   relocation scan that already runs per section.
//! - [`CsrBuilder`]: push `(row, value)` pairs in any order, sequentially.
//! - [`Csr::from_parts`]: hand over offset and value vectors built elsewhere;
//!   they are validated.

use std::fmt;

use rayon::prelude::*;

/// Rows (plus values) below which parallel helpers stop splitting.
const LEAF_WEIGHT: usize = 4096;

/// The caller handed a pass inconsistent data.
///
/// These are interface errors, reported as values so the passes never panic.
/// Backends validate input files before handing data to a pass, so an
/// `InputError` means a bug in qld: it converts into
/// [`crate::Error::Internal`] with `?`.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InputError {
    /// A CSR offset table is empty, does not start at zero, decreases, or
    /// does not end at the number of values.
    BadOffsets,
    /// Two tables that must describe the same rows have different lengths.
    RowCount {
        /// The number of rows expected.
        expected: usize,
        /// The number of rows found.
        found: usize,
    },
    /// An index (a row, or a section referenced by an edge or relocation) is
    /// out of range.
    OutOfRange {
        /// What the index refers to.
        what: &'static str,
        /// The offending index.
        index: u64,
        /// The number of valid entries.
        len: usize,
    },
    /// A size or count does not fit the pass's index types.
    TooLarge(&'static str),
    /// Two things that must agree do not, such as a merge section's kind and
    /// its group's.
    Mismatch {
        /// The two things that disagree.
        what: &'static str,
        /// The index of the offending item, or the offending length.
        index: u64,
    },
}

impl fmt::Display for InputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadOffsets => f.write_str("inconsistent row offset table"),
            Self::RowCount { expected, found } => {
                write!(f, "expected {expected} rows, found {found}")
            }
            Self::OutOfRange { what, index, len } => {
                write!(f, "{what} index {index} out of range (len {len})")
            }
            Self::TooLarge(what) => write!(f, "{what} too large"),
            Self::Mismatch { what, index } => write!(f, "{what} do not match ({index})"),
        }
    }
}

impl std::error::Error for InputError {}

impl From<InputError> for crate::Error {
    /// A pass was given inconsistent data by its caller: an internal error.
    fn from(error: InputError) -> Self {
        Self::Internal(format!("inconsistent pass input: {error}"))
    }
}

/// A list of variable-length rows stored in two flat vectors.
///
/// Invariant: `offsets` has `rows + 1` entries, starts at 0, never decreases
/// and ends at `values.len()`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Csr<T> {
    offsets: Vec<usize>,
    values: Vec<T>,
}

impl<T> Default for Csr<T> {
    fn default() -> Self {
        Self::empty(0)
    }
}

impl<T> Csr<T> {
    /// Creates `rows` empty rows.
    #[must_use]
    pub fn empty(rows: usize) -> Self {
        Self {
            offsets: vec![0; rows.saturating_add(1)],
            values: Vec::new(),
        }
    }

    /// Wraps an offset table and a value vector, after checking that the
    /// offsets start at 0, never decrease, and end at `values.len()`.
    ///
    /// # Errors
    ///
    /// Returns [`InputError::BadOffsets`] when they do not.
    pub fn from_parts(offsets: Vec<usize>, values: Vec<T>) -> Result<Self, InputError> {
        let well_formed = offsets.first() == Some(&0)
            && offsets.last() == Some(&values.len())
            && offsets.windows(2).all(|pair| pair[0] <= pair[1]);
        if well_formed {
            Ok(Self { offsets, values })
        } else {
            Err(InputError::BadOffsets)
        }
    }

    /// Number of rows.
    #[inline]
    #[must_use]
    pub fn rows(&self) -> usize {
        self.offsets.len().saturating_sub(1)
    }

    /// Total number of values across all rows.
    #[inline]
    #[must_use]
    pub fn num_values(&self) -> usize {
        self.values.len()
    }

    /// The values of row `row`; empty if `row` is out of range.
    #[inline]
    #[must_use]
    pub fn row(&self, row: usize) -> &[T] {
        match (self.offsets.get(row), self.offsets.get(row.wrapping_add(1))) {
            (Some(&start), Some(&end)) => self.values.get(start..end).unwrap_or(&[]),
            _ => &[],
        }
    }

    /// The offset table (`rows() + 1` entries).
    #[inline]
    #[must_use]
    pub fn offsets(&self) -> &[usize] {
        &self.offsets
    }

    /// All values, row after row.
    #[inline]
    #[must_use]
    pub fn values(&self) -> &[T] {
        &self.values
    }

    /// Splits into the offset table and the value vector.
    #[must_use]
    pub fn into_parts(self) -> (Vec<usize>, Vec<T>) {
        (self.offsets, self.values)
    }
}

impl<T: Copy + Send + Sync> Csr<T> {
    /// Builds rows in parallel, in two passes.
    ///
    /// First `count(row)` gives an upper bound on the length of each row
    /// (called in parallel). Then `fill(row, slot)` is called in parallel
    /// with a disjoint slice of exactly that many entries, prefilled with
    /// `filler`; it writes the row's values at the front and returns how many
    /// it wrote. Returned lengths larger than the slice are clamped. Rows that
    /// wrote fewer values than counted are compacted afterwards.
    ///
    /// The result depends only on what `count` and `fill` return, never on
    /// thread scheduling. Must run inside the caller's rayon pool.
    ///
    /// # Errors
    ///
    /// Returns [`InputError::TooLarge`] if the counts overflow `usize`.
    pub fn build_parallel<C, F>(
        rows: usize,
        filler: T,
        count: C,
        fill: F,
    ) -> Result<Self, InputError>
    where
        C: Fn(usize) -> usize + Sync,
        F: Fn(usize, &mut [T]) -> usize + Sync,
    {
        let mut lengths: Vec<usize> = (0..rows).into_par_iter().map(&count).collect();
        let mut offsets = Vec::with_capacity(rows.saturating_add(1));
        offsets.push(0usize);
        let mut total = 0usize;
        for &length in &lengths {
            total = total
                .checked_add(length)
                .ok_or(InputError::TooLarge("row lengths"))?;
            offsets.push(total);
        }
        let mut values = vec![filler; total];
        for_each_row_mut(
            &offsets,
            &mut values,
            &mut lengths,
            &|row, slot, written| {
                *written = fill(row, slot).min(slot.len());
            },
        );
        let short = lengths
            .iter()
            .zip(offsets.windows(2))
            .any(|(&written, bounds)| written != bounds[1] - bounds[0]);
        if short {
            // Sequential compaction: a single memmove-like sweep.
            let mut write = 0usize;
            for (row, &written) in lengths.iter().enumerate() {
                let start = offsets[row];
                values.copy_within(start..start + written, write);
                offsets[row] = write;
                write += written;
            }
            offsets[rows] = write;
            values.truncate(write);
        }
        Ok(Self { offsets, values })
    }
}

/// Collects `(row, value)` pairs in any order and builds a [`Csr`].
///
/// Within a row, values keep the order they were pushed in.
#[derive(Clone, Debug)]
pub struct CsrBuilder<T> {
    rows: usize,
    pairs: Vec<(usize, T)>,
}

impl<T> CsrBuilder<T> {
    /// Creates a builder for `rows` rows.
    #[must_use]
    pub fn new(rows: usize) -> Self {
        Self {
            rows,
            pairs: Vec::new(),
        }
    }

    /// Creates a builder for `rows` rows with room for `values` values.
    #[must_use]
    pub fn with_capacity(rows: usize, values: usize) -> Self {
        Self {
            rows,
            pairs: Vec::with_capacity(values),
        }
    }

    /// Number of rows the result will have.
    #[must_use]
    pub fn rows(&self) -> usize {
        self.rows
    }

    /// Appends `value` to row `row`. The row index is checked by
    /// [`CsrBuilder::build`].
    pub fn push(&mut self, row: usize, value: T) {
        self.pairs.push((row, value));
    }

    /// Builds the rows.
    ///
    /// # Errors
    ///
    /// Returns [`InputError::OutOfRange`] if a pushed row index is not below
    /// the row count.
    pub fn build(mut self) -> Result<Csr<T>, InputError> {
        if let Some(&(row, _)) = self.pairs.iter().find(|(row, _)| *row >= self.rows) {
            return Err(InputError::OutOfRange {
                what: "row",
                index: row as u64,
                len: self.rows,
            });
        }
        // Stable, and linear when rows were pushed in order (the usual case).
        self.pairs.sort_by_key(|&(row, _)| row);
        let mut offsets = vec![0usize; self.rows + 1];
        for &(row, _) in &self.pairs {
            offsets[row + 1] += 1;
        }
        for row in 0..self.rows {
            offsets[row + 1] += offsets[row];
        }
        let values = self.pairs.into_iter().map(|(_, value)| value).collect();
        Ok(Csr { offsets, values })
    }
}

/// Calls `f(row, values_of_row, per_row_entry)` for every row, in parallel,
/// with disjoint mutable slices.
///
/// `offsets` must be a valid CSR offset table for `values` (starting at 0),
/// and `per_row` must have one entry per row.
pub(crate) fn for_each_row_mut<T, A, F>(
    offsets: &[usize],
    values: &mut [T],
    per_row: &mut [A],
    f: &F,
) where
    T: Send,
    A: Send,
    F: Fn(usize, &mut [T], &mut A) + Sync,
{
    debug_assert_eq!(offsets.len(), per_row.len() + 1);
    split_rows(offsets, 0, values, per_row, f);
}

fn split_rows<T, A, F>(
    offsets: &[usize],
    base_row: usize,
    values: &mut [T],
    per_row: &mut [A],
    f: &F,
) where
    T: Send,
    A: Send,
    F: Fn(usize, &mut [T], &mut A) + Sync,
{
    let rows = per_row.len();
    if rows <= 1 || values.len() + rows <= LEAF_WEIGHT {
        let mut rest = values;
        for (row, extra) in per_row.iter_mut().enumerate() {
            let len = offsets[row + 1] - offsets[row];
            let (head, tail) = rest.split_at_mut(len);
            rest = tail;
            f(base_row + row, head, extra);
        }
        return;
    }
    let mid = rows / 2;
    let (left_values, right_values) = values.split_at_mut(offsets[mid] - offsets[0]);
    let (left_rows, right_rows) = per_row.split_at_mut(mid);
    rayon::join(
        || split_rows(&offsets[..=mid], base_row, left_values, left_rows, f),
        || split_rows(&offsets[mid..], base_row + mid, right_values, right_rows, f),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_parts_validates() {
        assert!(Csr::from_parts(vec![0, 1, 3], vec![1, 2, 3]).is_ok());
        assert_eq!(
            Csr::from_parts(vec![0, 2, 1], vec![1]).unwrap_err(),
            InputError::BadOffsets
        );
        assert!(Csr::<u8>::from_parts(vec![], vec![]).is_err());
        assert!(Csr::from_parts(vec![1, 1], vec![1]).is_err());
    }

    #[test]
    fn builder_keeps_push_order_within_rows() {
        let mut builder = CsrBuilder::new(3);
        builder.push(2, 'a');
        builder.push(0, 'b');
        builder.push(2, 'c');
        let csr = builder.build().unwrap();
        assert_eq!(csr.row(0), &['b']);
        assert!(csr.row(1).is_empty());
        assert_eq!(csr.row(2), &['a', 'c']);
        assert!(csr.row(3).is_empty());

        let mut bad = CsrBuilder::new(1);
        bad.push(1, 0u8);
        assert!(bad.build().is_err());
    }

    #[test]
    fn parallel_build_compacts_short_rows() {
        let rows = 20_000;
        let csr = Csr::build_parallel(
            rows,
            0u32,
            |row| row % 5,
            |row, slot| {
                let keep = slot.len() / 2;
                for (i, value) in slot.iter_mut().take(keep).enumerate() {
                    *value = (row * 10 + i) as u32;
                }
                keep
            },
        )
        .unwrap();
        assert_eq!(csr.rows(), rows);
        for row in 0..rows {
            let expected: Vec<u32> = (0..(row % 5) / 2).map(|i| (row * 10 + i) as u32).collect();
            assert_eq!(csr.row(row), expected.as_slice());
        }
        let checked = Csr::from_parts(csr.offsets().to_vec(), csr.values().to_vec());
        assert!(checked.is_ok());
    }

    #[test]
    fn input_errors_are_internal_errors() {
        fn convert() -> crate::Result<()> {
            Err(InputError::TooLarge("row lengths"))?
        }
        let error = convert().unwrap_err();
        assert!(matches!(error, crate::Error::Internal(_)));
        assert_eq!(
            error.to_string(),
            "internal error: inconsistent pass input: row lengths too large"
        );
    }
}
