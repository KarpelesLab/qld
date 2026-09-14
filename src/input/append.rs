//! An append-only vector whose elements never move.
//!
//! [`AppendVec`] lets any thread push through `&self` while other threads keep
//! `&T` references into existing elements. It is built from safe parts:
//! a fixed array of lazily allocated buckets whose sizes double, each slot a
//! [`OnceLock`]. Because a bucket is never reallocated, a reference into it
//! lives as long as the vector.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};

/// Size of the first bucket, as a power of two.
const FIRST_BUCKET_BITS: u32 = 6;

/// Number of buckets: enough for `u32::MAX` elements.
const BUCKETS: usize = 27;

/// The largest number of elements an [`AppendVec`] holds. Matches the range
/// of the 32-bit IDs in [`crate::ids`].
pub(crate) const MAX_LEN: usize = u32::MAX as usize;

/// An append-only, thread-safe vector with stable element addresses.
pub(crate) struct AppendVec<T> {
    buckets: [OnceLock<Box<[OnceLock<T>]>>; BUCKETS],
    len: AtomicUsize,
    push_lock: Mutex<()>,
}

impl<T> Default for AppendVec<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> std::fmt::Debug for AppendVec<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppendVec")
            .field("len", &self.len())
            .finish_non_exhaustive()
    }
}

/// Maps an index to its bucket and the position inside that bucket.
fn locate(index: usize) -> Option<(usize, usize, usize)> {
    if index >= MAX_LEN {
        return None;
    }
    let first = 1u64 << FIRST_BUCKET_BITS;
    let biased = u64::try_from(index).ok()?.checked_add(first)?;
    let bit = 63u32.checked_sub(biased.leading_zeros())?;
    let bucket = bit.checked_sub(FIRST_BUCKET_BITS)?;
    let bucket_start = 1u64.checked_shl(bit)?;
    let offset = usize::try_from(biased.checked_sub(bucket_start)?).ok()?;
    let size = usize::try_from(bucket_start).ok()?;
    Some((usize::try_from(bucket).ok()?, offset, size))
}

impl<T> AppendVec<T> {
    /// Creates an empty vector. Allocates nothing.
    pub(crate) const fn new() -> Self {
        Self {
            buckets: [const { OnceLock::new() }; BUCKETS],
            len: AtomicUsize::new(0),
            push_lock: Mutex::new(()),
        }
    }

    /// The number of elements.
    pub(crate) fn len(&self) -> usize {
        self.len.load(Ordering::Acquire)
    }

    /// Appends `value` and returns its index, or gives the value back when
    /// the vector is full.
    pub(crate) fn push(&self, value: T) -> Result<usize, T> {
        let _guard = self
            .push_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let index = self.len.load(Ordering::Acquire);
        let Some((bucket, offset, size)) = locate(index) else {
            return Err(value);
        };
        let Some(bucket) = self.buckets.get(bucket) else {
            return Err(value);
        };
        let bucket = bucket.get_or_init(|| (0..size).map(|_| OnceLock::new()).collect());
        let Some(slot) = bucket.get(offset) else {
            return Err(value);
        };
        slot.set(value)?;
        // `index < MAX_LEN`, so this cannot overflow.
        self.len.store(index.saturating_add(1), Ordering::Release);
        Ok(index)
    }

    /// Returns the element at `index`, if it has been pushed.
    pub(crate) fn get(&self, index: usize) -> Option<&T> {
        let (bucket, offset, _) = locate(index)?;
        self.buckets.get(bucket)?.get()?.get(offset)?.get()
    }
}

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)] // Test code builds fixtures, not parses input.
mod tests {
    use super::*;

    #[test]
    fn locate_covers_bucket_boundaries() {
        assert_eq!(locate(0), Some((0, 0, 64)));
        assert_eq!(locate(63), Some((0, 63, 64)));
        assert_eq!(locate(64), Some((1, 0, 128)));
        assert_eq!(locate(191), Some((1, 127, 128)));
        assert_eq!(locate(192), Some((2, 0, 256)));
        let (bucket, _, _) = locate(MAX_LEN - 1).unwrap();
        assert!(bucket < BUCKETS);
        assert_eq!(locate(MAX_LEN), None);
    }

    #[test]
    fn references_survive_concurrent_pushes() {
        let vec = AppendVec::new();
        vec.push(String::from("first")).unwrap();
        let first = vec.get(0).unwrap();
        std::thread::scope(|scope| {
            for thread in 0..4 {
                let vec = &vec;
                scope.spawn(move || {
                    for i in 0..500 {
                        vec.push(format!("{thread}-{i}")).unwrap();
                    }
                });
            }
        });
        assert_eq!(first, "first");
        assert_eq!(vec.len(), 2001);
        assert!(vec.get(2000).is_some());
        assert!(vec.get(2001).is_none());
    }
}
