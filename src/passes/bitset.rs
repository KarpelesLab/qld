//! Dense bitsets indexed by section number.
//!
//! [`AtomicBitSet`] is written concurrently by the parallel GC mark; once the
//! mark is over it becomes a plain [`BitSet`]. Both ignore out-of-range
//! indices instead of panicking: a query past the end reads as unset.

use std::sync::atomic::{AtomicU64, Ordering};

const WORD_BITS: usize = 64;

#[inline]
fn words_for(len: usize) -> usize {
    len.div_ceil(WORD_BITS)
}

/// A fixed-size bitset that many threads can set bits in at once.
///
/// Setting a bit is lock-free. Bits are only ever set, never cleared, so a
/// bit observed as set stays set.
#[derive(Debug)]
pub struct AtomicBitSet {
    words: Vec<AtomicU64>,
    len: usize,
}

impl AtomicBitSet {
    /// Creates a bitset of `len` bits, all clear.
    #[must_use]
    pub fn new(len: usize) -> Self {
        let words = std::iter::repeat_with(|| AtomicU64::new(0))
            .take(words_for(len))
            .collect();
        Self { words, len }
    }

    /// Number of bits in the set.
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Returns `true` if the set holds no bits at all.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Returns whether bit `index` is set. Out-of-range bits read as clear.
    #[inline]
    #[must_use]
    pub fn get(&self, index: usize) -> bool {
        match self.words.get(index / WORD_BITS) {
            Some(word) if index < self.len => {
                word.load(Ordering::Relaxed) & (1u64 << (index % WORD_BITS)) != 0
            }
            _ => false,
        }
    }

    /// Sets bit `index`. Returns `true` if this call changed it from clear to
    /// set, which exactly one of several racing callers observes.
    ///
    /// Out-of-range indices are ignored and return `false`.
    #[inline]
    pub fn set(&self, index: usize) -> bool {
        if index >= self.len {
            return false;
        }
        let Some(word) = self.words.get(index / WORD_BITS) else {
            return false;
        };
        let mask = 1u64 << (index % WORD_BITS);
        // Test first: most visits in a dense graph find the bit already set,
        // and a plain load does not dirty the cache line for other threads.
        if word.load(Ordering::Relaxed) & mask != 0 {
            return false;
        }
        word.fetch_or(mask, Ordering::AcqRel) & mask == 0
    }

    /// Converts into a plain [`BitSet`] once no thread writes any more.
    #[must_use]
    pub fn into_bitset(self) -> BitSet {
        BitSet {
            words: self.words.into_iter().map(AtomicU64::into_inner).collect(),
            len: self.len,
        }
    }
}

/// A fixed-size bitset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BitSet {
    words: Vec<u64>,
    len: usize,
}

impl BitSet {
    /// Creates a bitset of `len` bits, all clear.
    #[must_use]
    pub fn new(len: usize) -> Self {
        Self {
            words: vec![0; words_for(len)],
            len,
        }
    }

    /// Number of bits in the set.
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Returns `true` if the set holds no bits at all.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Returns whether bit `index` is set. Out-of-range bits read as clear.
    #[inline]
    #[must_use]
    pub fn get(&self, index: usize) -> bool {
        index < self.len
            && self
                .words
                .get(index / WORD_BITS)
                .is_some_and(|word| word & (1u64 << (index % WORD_BITS)) != 0)
    }

    /// Sets bit `index`, returning `true` if it was clear. Out-of-range
    /// indices are ignored and return `false`.
    #[inline]
    pub fn insert(&mut self, index: usize) -> bool {
        if index >= self.len {
            return false;
        }
        let Some(word) = self.words.get_mut(index / WORD_BITS) else {
            return false;
        };
        let mask = 1u64 << (index % WORD_BITS);
        let was_clear = *word & mask == 0;
        *word |= mask;
        was_clear
    }

    /// Number of set bits.
    #[must_use]
    pub fn count_ones(&self) -> usize {
        self.words
            .iter()
            .map(|word| word.count_ones() as usize)
            .sum()
    }

    /// Iterates over the indices of set bits, in increasing order.
    pub fn ones(&self) -> impl Iterator<Item = usize> + '_ {
        self.iter_with(0)
    }

    /// Iterates over the indices of clear bits, in increasing order.
    pub fn zeros(&self) -> impl Iterator<Item = usize> + '_ {
        self.iter_with(u64::MAX)
    }

    fn iter_with(&self, flip: u64) -> impl Iterator<Item = usize> + '_ {
        let len = self.len;
        self.words
            .iter()
            .enumerate()
            .flat_map(move |(word_index, &word)| {
                let mut bits = word ^ flip;
                let base = word_index * WORD_BITS;
                std::iter::from_fn(move || {
                    if bits == 0 {
                        return None;
                    }
                    let bit = bits.trailing_zeros() as usize;
                    bits &= bits - 1;
                    Some(base + bit)
                })
            })
            .take_while(move |&index| index < len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_set_reports_first_setter() {
        let bits = AtomicBitSet::new(130);
        assert!(bits.set(0));
        assert!(!bits.set(0));
        assert!(bits.set(129));
        assert!(!bits.set(130));
        assert!(bits.get(129));
        assert!(!bits.get(128));
        assert!(!bits.get(1000));
        let plain = bits.into_bitset();
        assert_eq!(plain.ones().collect::<Vec<_>>(), vec![0, 129]);
        assert_eq!(plain.count_ones(), 2);
        assert_eq!(plain.zeros().count(), 128);
    }

    #[test]
    fn zeros_stop_at_len() {
        let mut bits = BitSet::new(3);
        assert!(bits.insert(1));
        assert!(!bits.insert(1));
        assert!(!bits.insert(3));
        assert_eq!(bits.zeros().collect::<Vec<_>>(), vec![0, 2]);
        assert!(BitSet::new(0).zeros().next().is_none());
    }
}
