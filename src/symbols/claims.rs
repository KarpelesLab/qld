//! Deterministic first-come claims on named groups, such as COMDAT groups.
//!
//! A backend that deduplicates groups before symbol insertion (see
//! [`RoundHook`](super::RoundHook)) needs to decide, for each group
//! signature, which live file keeps its copy. The rule is:
//!
//! - among the files that become live in the same round, the one with the
//!   lowest `(InputPosition, FileId)` wins;
//! - a claim made in an earlier round is final, even against a file with a
//!   lower position that becomes live later.
//!
//! [`GroupClaims`] implements it as a sharded concurrent map, so a round's
//! files can offer their groups in parallel and in any order: within a round
//! an offer replaces the current claim only if it is lower, which makes the
//! outcome the minimum over the round's offers, whatever the scheduling.
//! [`GroupClaims::begin_round`] seals the claims of earlier rounds.

use core::fmt;
use std::sync::{Mutex, MutexGuard, PoisonError};

use hashbrown::HashTable;
use hashbrown::hash_table::Entry;

use super::name::{InputPosition, SymbolName};
use crate::ids::FileId;

/// Number of bits of the key hash that select a shard.
const CLAIM_SHARD_BITS: u32 = 8;

struct Claim<'a> {
    key: SymbolName<'a>,
    position: InputPosition,
    owner: FileId,
    /// The round the claim was made in; claims from earlier rounds are final.
    round: u32,
}

#[inline]
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // Critical sections here never run caller code.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[inline]
fn shard_of(hash: u64) -> usize {
    (hash >> (64 - CLAIM_SHARD_BITS)) as usize
}

/// The hash hashbrown sees: rotated so that its top bits (the control tags)
/// are not the shard selector, which is constant within a shard.
#[inline]
fn table_hash(hash: u64) -> u64 {
    hash.rotate_left(CLAIM_SHARD_BITS)
}

/// Claims on named groups (such as COMDAT group signatures), made while
/// resolution rounds load files.
///
/// - Among the files that become live in the same round, the one with the
///   lowest `(InputPosition, FileId)` wins.
/// - A claim made in an earlier round is final, even against a file with a
///   lower position that becomes live later.
///
/// It is a sharded concurrent map: a round's files can offer their groups in
/// parallel and in any order, since within a round an offer replaces the
/// current claim only if it is lower, so the outcome is the minimum over the
/// round's offers whatever the scheduling. [`begin_round`](Self::begin_round)
/// seals the claims of earlier rounds.
///
/// Keys are [`SymbolName`]s, so their hash is computed once (for ELF, build
/// the key with `SymbolName::new(signature)`). Claims persist across rounds:
/// keep one `GroupClaims` for the whole resolution.
pub struct GroupClaims<'a> {
    shards: Box<[Mutex<HashTable<Claim<'a>>>]>,
    round: u32,
}

impl Default for GroupClaims<'_> {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for GroupClaims<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GroupClaims")
            .field("round", &self.round)
            .field("len", &self.len())
            .finish_non_exhaustive()
    }
}

impl<'a> GroupClaims<'a> {
    /// Creates an empty claim table. It allocates only the shard array.
    #[must_use]
    pub fn new() -> Self {
        Self {
            shards: (0..1usize << CLAIM_SHARD_BITS)
                .map(|_| Mutex::new(HashTable::new()))
                .collect(),
            round: 0,
        }
    }

    /// Starts a new round of offers, sealing every claim made so far.
    ///
    /// Call it once per resolution round, before the round's offers; the
    /// returned [`ClaimRound`] takes the offers (from any number of threads)
    /// and answers ownership queries.
    pub fn begin_round(&mut self) -> ClaimRound<'_, 'a> {
        self.round = self.round.saturating_add(1);
        ClaimRound {
            claims: self,
            round: self.round,
        }
    }

    /// Takes offers for round `round` (numbered from 1) without `&mut`
    /// access, so that offers can be made while files load.
    ///
    /// Use either this or [`begin_round`](Self::begin_round) for a whole
    /// resolution, with increasing round numbers: an offer can replace only
    /// a claim made in its own round.
    #[must_use]
    pub fn in_round(&self, round: u32) -> ClaimRound<'_, 'a> {
        ClaimRound {
            claims: self,
            round,
        }
    }

    /// Returns the file that holds `key`, if any file claimed it.
    #[must_use]
    pub fn owner(&self, key: &SymbolName<'_>) -> Option<FileId> {
        let shard = lock(&self.shards[shard_of(key.hash())]);
        shard
            .find(table_hash(key.hash()), |claim| claim.key == *key)
            .map(|claim| claim.owner)
    }

    /// Returns the number of claimed keys.
    #[must_use]
    pub fn len(&self) -> usize {
        self.shards.iter().map(|shard| lock(shard).len()).sum()
    }

    /// Returns `true` if nothing has been claimed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// One round of offers on a [`GroupClaims`]; see
/// [`GroupClaims::begin_round`].
///
/// Offers may come from many threads in any order. Ownership of a key
/// offered in this round is final once every offer of the round is made, so
/// query [`owner`](Self::owner) after the offer phase (or, when offering
/// sequentially in `(position, file)` order, right after each offer).
pub struct ClaimRound<'c, 'a> {
    claims: &'c GroupClaims<'a>,
    round: u32,
}

impl fmt::Debug for ClaimRound<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClaimRound")
            .field("round", &self.round)
            .finish_non_exhaustive()
    }
}

impl<'a> ClaimRound<'_, 'a> {
    /// Offers `file`, at `position`, as the owner of `key`.
    ///
    /// The offer wins if nobody holds `key`, or if the holder claimed it in
    /// this round with a higher `(position, file)`. Returns `true` if `file`
    /// holds the key after the offer (which a later, lower offer in the same
    /// round can still change).
    pub fn offer(&self, key: SymbolName<'a>, position: InputPosition, file: FileId) -> bool {
        let mut shard = lock(&self.claims.shards[shard_of(key.hash())]);
        let entry = shard.entry(
            table_hash(key.hash()),
            |claim| claim.key == key,
            |claim| table_hash(claim.key.hash()),
        );
        match entry {
            Entry::Vacant(vacant) => {
                vacant.insert(Claim {
                    key,
                    position,
                    owner: file,
                    round: self.round,
                });
                true
            }
            Entry::Occupied(mut occupied) => {
                let claim = occupied.get_mut();
                if claim.round == self.round && (position, file) < (claim.position, claim.owner) {
                    claim.position = position;
                    claim.owner = file;
                }
                claim.owner == file
            }
        }
    }

    /// Returns the file that holds `key`, if any.
    #[must_use]
    pub fn owner(&self, key: &SymbolName<'_>) -> Option<FileId> {
        self.claims.owner(key)
    }

    /// Returns `true` if `file` holds `key`.
    #[must_use]
    pub fn is_owner(&self, key: &SymbolName<'_>, file: FileId) -> bool {
        self.owner(key) == Some(file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rayon::prelude::*;

    fn key(name: &'static str) -> SymbolName<'static> {
        SymbolName::new(name.as_bytes())
    }

    #[test]
    fn lowest_offer_in_a_round_wins_regardless_of_order() {
        let offers: Vec<(u32, usize)> = (0..200).map(|i| ((i * 37) % 200, i as usize)).collect();
        for threads in [1, 4] {
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .unwrap();
            let mut claims = GroupClaims::new();
            pool.install(|| {
                let round = claims.begin_round();
                offers.par_iter().for_each(|&(position, file)| {
                    round.offer(
                        key(if file % 2 == 0 { "even" } else { "odd" }),
                        InputPosition::new(position + 1, 0),
                        FileId::new(file),
                    );
                });
            });
            let lowest = |parity: usize| {
                offers
                    .iter()
                    .filter(|(_, file)| file % 2 == parity)
                    .min()
                    .map(|&(_, file)| FileId::new(file))
            };
            assert_eq!(claims.owner(&key("even")), lowest(0));
            assert_eq!(claims.owner(&key("odd")), lowest(1));
            assert_eq!(claims.len(), 2);
        }
    }

    #[test]
    fn claims_from_earlier_rounds_are_final() {
        let mut claims = GroupClaims::new();
        assert!(claims.is_empty());
        {
            let round = claims.begin_round();
            assert!(round.offer(key("g"), InputPosition::new(5, 0), FileId::new(5)));
            assert!(round.offer(key("g"), InputPosition::new(3, 0), FileId::new(3)));
            assert!(!round.offer(key("g"), InputPosition::new(4, 0), FileId::new(4)));
            assert!(round.is_owner(&key("g"), FileId::new(3)));
        }
        {
            let round = claims.begin_round();
            // A lower position in a later round does not take the group.
            assert!(!round.offer(key("g"), InputPosition::new(1, 0), FileId::new(1)));
            assert!(round.offer(key("h"), InputPosition::new(9, 0), FileId::new(9)));
            assert_eq!(round.owner(&key("g")), Some(FileId::new(3)));
        }
        assert_eq!(claims.owner(&key("h")), Some(FileId::new(9)));
        assert_eq!(claims.owner(&key("missing")), None);
        // Equal positions fall back to the file ID.
        let round = claims.begin_round();
        round.offer(key("tie"), InputPosition::new(2, 0), FileId::new(8));
        round.offer(key("tie"), InputPosition::new(2, 0), FileId::new(7));
        assert!(round.is_owner(&key("tie"), FileId::new(7)));
    }
}
