//! The sharded concurrent symbol table.
//!
//! # Interning and deterministic IDs
//!
//! Symbol IDs are dense (`0..len`) and must not depend on thread scheduling.
//! A concurrent map that hands out the next free ID on insertion cannot
//! promise that: the first thread to reach a new name would decide its ID.
//! So interning is a batch operation, [`SymbolTable::intern_batch`], in three
//! passes:
//!
//! 1. **Parallel lookup.** Every name of every job is looked up in its shard
//!    (one lock per shard, shard chosen by the top hash bits). A name already
//!    in the table yields its final ID. A new name is appended to the shard's
//!    *pending* list, together with its first occurrence: the lowest
//!    `(job position, index in job)` that interned it, kept up to date under
//!    the shard lock. The job's output slot receives a provisional handle
//!    (bit 31 set, low bits = index in the shard's pending list).
//! 2. **Ordering.** All pending names are sorted by first occurrence (ties,
//!    which only arise when two jobs share a position, fall back to the name
//!    bytes) and numbered from the current table length. The minimum over
//!    occurrences does not depend on which thread saw a name first, so
//!    neither does the order.
//! 3. **Parallel rewrite.** Every provisional handle in the jobs' outputs is
//!    replaced by its final ID. The shard is recomputed from the name's hash,
//!    so the handle does not need to store it.
//!
//! The result is exactly what a single-threaded loop interning names in
//! position order would produce: IDs follow first occurrence.
//!
//! **Cost** over a scheduling-dependent design: the per-shard pending record
//! (56 bytes per *new* name, freed at the end of the batch), one parallel
//! sort of the new names (`O(U log U)` for `U` new names), one hash probe per
//! new name to replace its provisional slot, and one linear pass over the job
//! outputs. Names that already exist cost nothing extra. The alternative of
//! interning in parallel with scheduling-dependent IDs and renumbering later
//! would need the same sort, plus remapping every ID-indexed structure built
//! in between.
//!
//! IDs across batches follow batch order: every ID from one batch is lower
//! than every ID from the next. The resolution driver makes each round one
//! batch, and the set of files in each round is itself deterministic.
//!
//! # Limits
//!
//! A table holds at most [`MAX_SYMBOLS`] names. [`SymbolTable::try_intern`]
//! and [`SymbolTable::try_intern_batch`] return [`Error::Limit`] when a name
//! would exceed it, and leave the table as it was before the call.
//!
//! # Shards and hashes
//!
//! Each shard holds a [`hashbrown::HashTable`] of 8-byte slots: the low 32
//! bits of the name hash and a 32-bit ID or pending index. The shard is chosen
//! by the top [`SHARD_BITS`] bits of the 64-bit hash, and the table is fed a
//! multiplicative expansion of the low 32 bits, so the bits hashbrown uses
//! for bucket selection and for its 7-bit control tags are independent of the
//! shard selector, and no name bytes are rehashed on lookup or growth.
//!
//! # Per-symbol state
//!
//! State lives in struct-of-arrays vectors indexed by [`SymbolId`]: names,
//! flags (`AtomicU32`, lock-free), and the current definition split into one
//! atomic vector per field. Updating a definition compares and writes several
//! fields, so it takes one of [`DEFINITION_LOCKS`] striped locks chosen by
//! symbol ID; reading a single field after resolution is a plain atomic load.

use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use hashbrown::HashTable;
use hashbrown::hash_table::Entry;
use rayon::prelude::*;

use super::definition::{Definition, DefinitionKind, Resolver, takes_precedence};
use super::flags::{self, SymbolFlags};
use super::name::{InputPosition, SymbolName};
use crate::error::{Error, Result};
use crate::ids::{FileId, SymbolId};

/// Number of bits of the name hash that select a shard.
///
/// Chosen by measurement (the `stress_intern_millions_of_symbols` test, 12M
/// references to 4M names on 64 threads): 256 shards spent twice as long in
/// the lookup pass as 2048, and 4096 gained nothing more. Shards are cheap:
/// an empty one does not allocate.
pub const SHARD_BITS: u32 = 11;
/// Number of shards in the table, each behind its own lock.
pub const SHARD_COUNT: usize = 1 << SHARD_BITS;
/// Number of striped locks that serialize definition updates.
pub const DEFINITION_LOCKS: usize = 1 << 12;
/// The largest number of distinct symbols a table can hold (2³¹). Bit 31 of
/// a raw slot value marks a provisional handle during interning.
pub const MAX_SYMBOLS: usize = PENDING as usize;

const PENDING: u32 = 1 << 31;
/// Jobs are split into parallel chunks no smaller than this.
const MIN_PARALLEL_CHUNK: usize = 1024;

fn overflow(limit: usize) -> Error {
    Error::Limit(format!("more than {limit} distinct symbol names"))
}

/// A name's first occurrence: job position, then index within the job.
type Occurrence = (InputPosition, u32);

#[derive(Clone, Copy)]
struct Slot {
    /// Low 32 bits of the name hash.
    h32: u32,
    /// Final symbol ID, or `PENDING | index into Shard::pending`.
    value: u32,
}

struct PendingName<'a> {
    name: SymbolName<'a>,
    first: Occurrence,
}

struct Shard<'a> {
    table: HashTable<Slot>,
    /// Names first seen in the current batch.
    pending: Vec<PendingName<'a>>,
    /// Final IDs for the current batch's pending names, by pending index;
    /// only populated between passes 2 and 3.
    assigned: Vec<AtomicU32>,
}

impl Shard<'_> {
    fn new() -> Self {
        Self {
            table: HashTable::new(),
            pending: Vec::new(),
            assigned: Vec::new(),
        }
    }
}

#[inline]
fn shard_of(hash: u64) -> usize {
    (hash >> (64 - SHARD_BITS)) as usize
}

#[inline]
fn low32(hash: u64) -> u32 {
    hash as u32
}

/// Expands the stored 32 bits into the 64-bit value hashbrown sees. The
/// multiplication spreads all 32 bits into the top 7 bits (control tags),
/// while the low bits (bucket index) stay a bijection of the low hash bits.
#[inline]
fn table_hash(h32: u32) -> u64 {
    u64::from(h32).wrapping_mul(0x9e37_79b9_7f4a_7c15)
}

#[inline]
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // Critical sections here never run caller code, so a poisoned lock only
    // means another thread panicked on an unrelated bug; the data is intact.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[inline]
fn get_mut<T>(mutex: &mut Mutex<T>) -> &mut T {
    mutex.get_mut().unwrap_or_else(PoisonError::into_inner)
}

/// One unit of work for [`SymbolTable::intern_batch`]: usually the global
/// symbols of one input file.
#[derive(Debug)]
pub struct InternJob<'a, 's> {
    /// The input position of the file the names come from. First occurrence
    /// is decided by `(position, index in names)`.
    pub position: InputPosition,
    /// The names to intern.
    pub names: &'s [SymbolName<'a>],
    /// Receives the ID of each name. Must be as long as `names`; its previous
    /// contents are ignored.
    pub ids: &'s mut [SymbolId],
}

/// The global symbol table: interned names plus per-symbol state.
///
/// See the [module documentation](self) for the design. In short:
/// [`intern_batch`](Self::intern_batch) (`&mut self`, parallel inside) turns
/// names into IDs deterministically; [`insert_definition`](Self::insert_definition)
/// and [`set_flags`](Self::set_flags) (`&self`) may then be called from any
/// number of threads.
pub struct SymbolTable<'a> {
    shards: Box<[Mutex<Shard<'a>>]>,
    names: Vec<SymbolName<'a>>,
    flags: Vec<AtomicU32>,
    def_kind: Vec<AtomicU8>,
    def_file: Vec<AtomicU32>,
    def_index: Vec<AtomicU32>,
    def_position: Vec<AtomicU64>,
    def_aux: Vec<AtomicU64>,
    def_locks: Box<[Mutex<()>]>,
    /// The most symbols this table may hold: [`MAX_SYMBOLS`], lowered only
    /// by tests.
    limit: usize,
}

impl Default for SymbolTable<'_> {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for SymbolTable<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SymbolTable")
            .field("len", &self.len())
            .finish_non_exhaustive()
    }
}

impl<'a> SymbolTable<'a> {
    /// Creates an empty table.
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(0)
    }

    /// Creates an empty table sized for about `symbols` distinct names.
    ///
    /// Sizing up front avoids growing shard tables while their lock is held,
    /// which stalls every thread hashing into that shard.
    #[must_use]
    pub fn with_capacity(symbols: usize) -> Self {
        let per_shard = symbols.div_ceil(SHARD_COUNT);
        let shards = (0..SHARD_COUNT)
            .map(|_| {
                let mut shard = Shard::new();
                shard.table.reserve(per_shard, |slot| table_hash(slot.h32));
                Mutex::new(shard)
            })
            .collect();
        let def_locks = (0..DEFINITION_LOCKS).map(|_| Mutex::new(())).collect();
        Self {
            shards,
            names: Vec::with_capacity(symbols),
            flags: Vec::with_capacity(symbols),
            def_kind: Vec::with_capacity(symbols),
            def_file: Vec::with_capacity(symbols),
            def_index: Vec::with_capacity(symbols),
            def_position: Vec::with_capacity(symbols),
            def_aux: Vec::with_capacity(symbols),
            def_locks,
            limit: MAX_SYMBOLS,
        }
    }

    /// Lowers the symbol limit, so tests can reach it.
    #[cfg(test)]
    pub(crate) fn set_limit(&mut self, limit: usize) {
        self.limit = limit.min(MAX_SYMBOLS);
    }

    /// Returns the number of distinct symbols.
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.names.len()
    }

    /// Returns `true` if no symbol has been interned.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    /// Returns every symbol ID, in order.
    pub fn ids(&self) -> impl ExactSizeIterator<Item = SymbolId> + use<> {
        (0..self.len()).map(SymbolId::new)
    }

    /// Returns the name of a symbol.
    ///
    /// # Panics
    ///
    /// Panics if `id` was not issued by this table.
    #[inline]
    #[must_use]
    pub fn name(&self, id: SymbolId) -> SymbolName<'a> {
        self.names[id.index()]
    }

    /// Returns all names, indexed by symbol ID.
    #[inline]
    #[must_use]
    pub fn names(&self) -> &[SymbolName<'a>] {
        &self.names
    }

    /// Looks up a name without interning it.
    #[must_use]
    pub fn lookup(&self, name: &SymbolName<'_>) -> Option<SymbolId> {
        let h32 = low32(name.hash());
        let shard = lock(&self.shards[shard_of(name.hash())]);
        let names = &self.names;
        shard
            .table
            .find(table_hash(h32), |slot| {
                slot.h32 == h32 && slot.value & PENDING == 0 && names[slot.value as usize] == *name
            })
            .map(|slot| SymbolId::from_u32(slot.value))
    }

    /// Interns one name, returning its ID. A new name gets the next ID.
    ///
    /// This is the sequential path, for the handful of names the linker
    /// creates itself (`-u`, the entry point, linker-defined symbols). Use
    /// [`intern_batch`](Self::intern_batch) for input files.
    ///
    /// # Panics
    ///
    /// Panics if the table already holds [`MAX_SYMBOLS`] symbols; see
    /// [`try_intern`](Self::try_intern) for the fallible version.
    pub fn intern(&mut self, name: SymbolName<'a>) -> SymbolId {
        match self.try_intern(name) {
            Ok(id) => id,
            Err(error) => panic!("{error}"),
        }
    }

    /// Interns one name like [`intern`](Self::intern), but returns an error
    /// instead of panicking when the table is full.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Limit`] if `name` is new and the table already holds
    /// [`MAX_SYMBOLS`] symbols. The table is unchanged.
    pub fn try_intern(&mut self, name: SymbolName<'a>) -> Result<SymbolId> {
        let h32 = low32(name.hash());
        let next = self.names.len();
        let limit = self.limit;
        let shard = get_mut(&mut self.shards[shard_of(name.hash())]);
        let names = &self.names;
        let entry = shard.table.entry(
            table_hash(h32),
            |slot| slot.h32 == h32 && names[slot.value as usize] == name,
            |slot| table_hash(slot.h32),
        );
        match entry {
            Entry::Occupied(occupied) => Ok(SymbolId::from_u32(occupied.get().value)),
            Entry::Vacant(vacant) => {
                if next >= limit {
                    return Err(overflow(limit));
                }
                let id = SymbolId::new(next);
                vacant.insert(Slot {
                    h32,
                    value: id.as_u32(),
                });
                self.names.push(name);
                self.grow_state(1);
                Ok(id)
            }
        }
    }

    /// Interns the names of many jobs in parallel and writes their IDs.
    ///
    /// The IDs are the same for every thread count and scheduling: new names
    /// are numbered, from [`len`](Self::len) upward, in order of their first
    /// occurrence `(job.position, index in job.names)`. Jobs are processed in
    /// the current rayon pool.
    ///
    /// # Panics
    ///
    /// Panics if a job's `ids` and `names` differ in length, or if the table
    /// would exceed [`MAX_SYMBOLS`] symbols; see
    /// [`try_intern_batch`](Self::try_intern_batch) for the version that
    /// returns the latter as an error.
    pub fn intern_batch(&mut self, jobs: &mut [InternJob<'a, '_>]) {
        if let Err(error) = self.try_intern_batch(jobs) {
            panic!("{error}");
        }
    }

    /// Interns a batch like [`intern_batch`](Self::intern_batch), but returns
    /// an error instead of panicking when the table would overflow.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Limit`] if the batch's new names would take the table
    /// past [`MAX_SYMBOLS`] symbols. The table is then unchanged: no name of
    /// the batch is added, and the jobs' `ids` hold unspecified values.
    ///
    /// # Panics
    ///
    /// Panics if a job's `ids` and `names` differ in length.
    pub fn try_intern_batch(&mut self, jobs: &mut [InternJob<'a, '_>]) -> Result<()> {
        for job in jobs.iter() {
            assert_eq!(
                job.names.len(),
                job.ids.len(),
                "intern job output length mismatch"
            );
        }

        // Pass 1: look up or provisionally insert, in parallel.
        let overflowed = AtomicBool::new(false);
        {
            let this = &*self;
            jobs.par_iter_mut().for_each(|job| {
                let position = job.position;
                job.ids
                    .par_iter_mut()
                    .zip(job.names.par_iter())
                    .enumerate()
                    .with_min_len(MIN_PARALLEL_CHUNK)
                    .for_each(|(index, (id, name))| {
                        let index = u32::try_from(index).unwrap_or(u32::MAX);
                        *id = SymbolId::from_u32(this.lookup_or_pend(
                            name,
                            (position, index),
                            &overflowed,
                        ));
                    });
            });
        }

        let new_count: usize = self
            .shards
            .iter_mut()
            .map(|shard| get_mut(shard).pending.len())
            .sum();
        if new_count == 0 {
            return Ok(());
        }
        if overflowed.into_inner() || new_count > self.limit.saturating_sub(self.names.len()) {
            self.discard_pending();
            return Err(overflow(self.limit));
        }

        // Pass 2: number the pending names by first occurrence.
        self.assign_pending(new_count);

        // Pass 3: replace provisional handles with final IDs.
        let shards: Vec<&Shard<'a>> = self
            .shards
            .iter_mut()
            .map(|shard| &*get_mut(shard))
            .collect();
        jobs.par_iter_mut().for_each(|job| {
            job.ids
                .par_iter_mut()
                .zip(job.names.par_iter())
                .with_min_len(MIN_PARALLEL_CHUNK)
                .for_each(|(id, name)| {
                    let raw = id.as_u32();
                    if raw & PENDING != 0 {
                        let local = (raw & !PENDING) as usize;
                        let assigned = &shards[shard_of(name.hash())].assigned[local];
                        *id = SymbolId::from_u32(assigned.load(Ordering::Relaxed));
                    }
                });
        });
        drop(shards);
        self.shards.par_iter_mut().for_each(|shard| {
            get_mut(shard).assigned = Vec::new();
        });
        Ok(())
    }

    /// Pass 1 of interning for one name. Returns a final ID or a pending
    /// handle. If the shard's pending list is full, sets `overflowed` and
    /// returns a handle that must not be resolved.
    #[inline]
    fn lookup_or_pend(
        &self,
        name: &SymbolName<'a>,
        occurrence: Occurrence,
        overflowed: &AtomicBool,
    ) -> u32 {
        let h32 = low32(name.hash());
        let mut guard = lock(&self.shards[shard_of(name.hash())]);
        let Shard { table, pending, .. } = &mut *guard;
        let names = &self.names;
        let entry = table.entry(
            table_hash(h32),
            |slot| {
                slot.h32 == h32
                    && if slot.value & PENDING == 0 {
                        names[slot.value as usize] == *name
                    } else {
                        pending[(slot.value & !PENDING) as usize].name == *name
                    }
            },
            |slot| table_hash(slot.h32),
        );
        match entry {
            Entry::Occupied(occupied) => {
                let value = occupied.get().value;
                if value & PENDING != 0 {
                    let record = &mut pending[(value & !PENDING) as usize];
                    if occurrence < record.first {
                        record.first = occurrence;
                    }
                }
                value
            }
            Entry::Vacant(vacant) => {
                // A shard's pending list holds at most 2^31 names, which is
                // also the table limit, so a full list means overflow.
                let Some(local) = u32::try_from(pending.len())
                    .ok()
                    .filter(|&local| local < PENDING)
                else {
                    overflowed.store(true, Ordering::Relaxed);
                    return u32::MAX;
                };
                pending.push(PendingName {
                    name: *name,
                    first: occurrence,
                });
                let value = PENDING | local;
                vacant.insert(Slot { h32, value });
                value
            }
        }
    }

    /// Undoes pass 1 after an overflow: removes the provisional slots and
    /// the pending names.
    fn discard_pending(&mut self) {
        for shard in self.shards.iter_mut() {
            let shard = get_mut(shard);
            if !shard.pending.is_empty() {
                shard.table.retain(|slot| slot.value & PENDING == 0);
                shard.pending = Vec::new();
            }
        }
    }

    /// Pass 2 of interning: numbers the `new_count` pending names, which the
    /// caller checked fit in the table.
    fn assign_pending(&mut self, new_count: usize) {
        let base = self.names.len();
        let mut shards: Vec<&mut Shard<'a>> = self.shards.iter_mut().map(get_mut).collect();

        // Gather (first occurrence, shard, pending index) and sort.
        let view: Vec<&Shard<'a>> = shards.iter().map(|shard| &**shard).collect();
        let mut order: Vec<(Occurrence, u32, u32)> = Vec::with_capacity(new_count);
        order.par_extend(
            (0..new_count)
                .into_par_iter()
                .map(|_| ((InputPosition::default(), 0), 0, 0)),
        );
        {
            // Carve `order` into one disjoint chunk per shard and fill them in
            // parallel.
            let mut chunks = Vec::with_capacity(view.len());
            let mut rest = order.as_mut_slice();
            for shard in &view {
                let (chunk, tail) = rest.split_at_mut(shard.pending.len());
                chunks.push(chunk);
                rest = tail;
            }
            chunks
                .into_par_iter()
                .zip(view.par_iter())
                .enumerate()
                .for_each(|(s, (chunk, shard))| {
                    for (l, (slot, record)) in chunk.iter_mut().zip(&shard.pending).enumerate() {
                        *slot = (record.first, s as u32, l as u32);
                    }
                });
        }
        order.par_sort_unstable_by(|a, b| {
            a.0.cmp(&b.0).then_with(|| {
                let name_a = &view[a.1 as usize].pending[a.2 as usize].name;
                let name_b = &view[b.1 as usize].pending[b.2 as usize].name;
                name_a.cmp_contents(name_b)
            })
        });

        // Names in ID order.
        self.names.par_extend(
            order
                .par_iter()
                .map(|&(_, s, l)| view[s as usize].pending[l as usize].name),
        );
        drop(view);

        // Pending index -> final ID, per shard.
        shards.par_iter_mut().for_each(|shard| {
            let len = shard.pending.len();
            shard.assigned.clear();
            shard.assigned.resize_with(len, || AtomicU32::new(u32::MAX));
        });
        {
            let view: Vec<&Shard<'a>> = shards.iter().map(|shard| &**shard).collect();
            order
                .par_iter()
                .enumerate()
                .with_min_len(MIN_PARALLEL_CHUNK)
                .for_each(|(rank, &(_, s, l))| {
                    // base + rank < limit <= MAX_SYMBOLS, checked by the caller.
                    let id = (base + rank) as u32;
                    view[s as usize].assigned[l as usize].store(id, Ordering::Relaxed);
                });
        }
        drop(order);

        // Replace provisional slots with final IDs.
        shards.par_iter_mut().for_each(|shard| {
            let Shard {
                table,
                pending,
                assigned,
            } = &mut **shard;
            for (local, record) in pending.iter().enumerate() {
                let h32 = low32(record.name.hash());
                let handle = PENDING | local as u32;
                let slot = table.find_mut(table_hash(h32), |slot| slot.value == handle);
                debug_assert!(slot.is_some(), "pending slot missing");
                if let Some(slot) = slot {
                    slot.value = assigned[local].load(Ordering::Relaxed);
                }
            }
            // Release the memory, not just the length: the first batch of a
            // link is by far the largest.
            *pending = Vec::new();
        });

        self.grow_state(new_count);
    }

    /// Appends default per-symbol state for `count` new symbols.
    fn grow_state(&mut self, count: usize) {
        fn grow<T: Send>(vec: &mut Vec<T>, count: usize, make: impl Fn() -> T + Sync + Send) {
            if count < MIN_PARALLEL_CHUNK {
                vec.extend((0..count).map(|_| make()));
            } else {
                vec.par_extend((0..count).into_par_iter().map(|_| make()));
            }
        }
        let Self {
            flags,
            def_kind,
            def_file,
            def_index,
            def_position,
            def_aux,
            ..
        } = self;
        rayon::join(
            || {
                rayon::join(
                    || grow(flags, count, || AtomicU32::new(0)),
                    || {
                        grow(def_kind, count, || {
                            AtomicU8::new(DefinitionKind::Undefined as u8)
                        });
                    },
                )
            },
            || {
                rayon::join(
                    || grow(def_file, count, || AtomicU32::new(0)),
                    || {
                        rayon::join(
                            || grow(def_index, count, || AtomicU32::new(0)),
                            || {
                                rayon::join(
                                    || grow(def_position, count, || AtomicU64::new(0)),
                                    || grow(def_aux, count, || AtomicU64::new(0)),
                                )
                            },
                        )
                    },
                )
            },
        );
    }

    // ----- flags -----------------------------------------------------------

    /// Returns a symbol's flags.
    ///
    /// # Panics
    ///
    /// Panics if `id` was not issued by this table.
    #[inline]
    #[must_use]
    pub fn flags(&self, id: SymbolId) -> SymbolFlags {
        SymbolFlags::from_bits(self.flags[id.index()].load(Ordering::Relaxed))
    }

    /// Sets `flags` on a symbol, lock-free, and returns the flags it had
    /// before. Callable from any thread.
    ///
    /// # Panics
    ///
    /// Panics if `id` was not issued by this table.
    #[inline]
    pub fn set_flags(&self, id: SymbolId, flags: SymbolFlags) -> SymbolFlags {
        flags::set(&self.flags[id.index()], flags)
    }

    /// Clears `flags` on a symbol, lock-free, and returns the flags it had
    /// before.
    ///
    /// Unlike setting, clearing concurrently with setting the same bit is
    /// order-dependent; only clear bits in a phase that nothing else writes.
    ///
    /// # Panics
    ///
    /// Panics if `id` was not issued by this table.
    #[inline]
    pub fn clear_flags(&self, id: SymbolId, flags: SymbolFlags) -> SymbolFlags {
        flags::clear(&self.flags[id.index()], flags)
    }

    // ----- definitions -----------------------------------------------------

    /// Offers `candidate` as a definition of `id`, and keeps it if it takes
    /// precedence over the current one under `resolver` (see
    /// [`takes_precedence`]). Returns `true` if it did.
    ///
    /// Callable from any thread. Because the precedence order is total, the
    /// definition left after any set of concurrent insertions is the same as
    /// for any sequential order.
    ///
    /// # Panics
    ///
    /// Panics if `id` was not issued by this table.
    pub fn insert_definition<R: Resolver + ?Sized>(
        &self,
        resolver: &R,
        id: SymbolId,
        candidate: &Definition,
    ) -> bool {
        let index = id.index();
        let _guard = lock(&self.def_locks[index & (DEFINITION_LOCKS - 1)]);
        let current = self.load_definition(index);
        if !takes_precedence(resolver, candidate, &current) {
            return false;
        }
        self.store_definition(index, candidate);
        true
    }

    /// Replaces the definition of `id` unconditionally (for example with a
    /// linker-defined symbol). Callable from any thread.
    ///
    /// # Panics
    ///
    /// Panics if `id` was not issued by this table.
    pub fn replace_definition(&self, id: SymbolId, definition: &Definition) {
        let index = id.index();
        let _guard = lock(&self.def_locks[index & (DEFINITION_LOCKS - 1)]);
        self.store_definition(index, definition);
    }

    /// Returns the current definition of a symbol.
    ///
    /// This reads the fields without locking. The result is consistent
    /// whenever no insertion for the same symbol runs concurrently, which is
    /// the case in every phase after resolution. Use
    /// [`definition_synchronized`](Self::definition_synchronized) during it.
    ///
    /// # Panics
    ///
    /// Panics if `id` was not issued by this table.
    #[inline]
    #[must_use]
    pub fn definition(&self, id: SymbolId) -> Definition {
        self.load_definition(id.index())
    }

    /// Returns the current definition of a symbol, taking its update lock so
    /// the fields are consistent even while other threads insert.
    ///
    /// # Panics
    ///
    /// Panics if `id` was not issued by this table.
    #[must_use]
    pub fn definition_synchronized(&self, id: SymbolId) -> Definition {
        let index = id.index();
        let _guard = lock(&self.def_locks[index & (DEFINITION_LOCKS - 1)]);
        self.load_definition(index)
    }

    /// Returns the kind of a symbol's current definition (one atomic load).
    ///
    /// # Panics
    ///
    /// Panics if `id` was not issued by this table.
    #[inline]
    #[must_use]
    pub fn definition_kind(&self, id: SymbolId) -> DefinitionKind {
        DefinitionKind::from_u8(self.def_kind[id.index()].load(Ordering::Relaxed))
    }

    /// Returns the file of a symbol's current definition, or `None` if it is
    /// undefined.
    ///
    /// # Panics
    ///
    /// Panics if `id` was not issued by this table.
    #[inline]
    #[must_use]
    pub fn definition_file(&self, id: SymbolId) -> Option<FileId> {
        let index = id.index();
        (self.def_kind[index].load(Ordering::Relaxed) != DefinitionKind::Undefined as u8)
            .then(|| FileId::from_u32(self.def_file[index].load(Ordering::Relaxed)))
    }

    #[inline]
    fn load_definition(&self, index: usize) -> Definition {
        let kind = DefinitionKind::from_u8(self.def_kind[index].load(Ordering::Relaxed));
        if kind == DefinitionKind::Undefined {
            return Definition::undefined();
        }
        Definition {
            kind,
            file: FileId::from_u32(self.def_file[index].load(Ordering::Relaxed)),
            index: self.def_index[index].load(Ordering::Relaxed),
            position: InputPosition::from_raw(self.def_position[index].load(Ordering::Relaxed)),
            aux: self.def_aux[index].load(Ordering::Relaxed),
        }
    }

    #[inline]
    fn store_definition(&self, index: usize, definition: &Definition) {
        self.def_file[index].store(definition.file.as_u32(), Ordering::Relaxed);
        self.def_index[index].store(definition.index, Ordering::Relaxed);
        self.def_position[index].store(definition.position.raw(), Ordering::Relaxed);
        self.def_aux[index].store(definition.aux, Ordering::Relaxed);
        self.def_kind[index].store(definition.kind as u8, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::symbols::elf_reference::ElfReferenceRules;

    fn names_from(strings: &[&'static str]) -> Vec<SymbolName<'static>> {
        strings
            .iter()
            .map(|s| SymbolName::new(s.as_bytes()))
            .collect()
    }

    #[test]
    fn sequential_intern_numbers_by_first_sight() {
        let mut table = SymbolTable::new();
        let a = table.intern(SymbolName::new(b"a"));
        let b = table.intern(SymbolName::new(b"b"));
        assert_eq!(table.intern(SymbolName::new(b"a")), a);
        assert_eq!((a.index(), b.index()), (0, 1));
        assert_eq!(table.len(), 2);
        assert_eq!(table.name(b).bytes(), b"b");
        assert_eq!(table.lookup(&SymbolName::new(b"b")), Some(b));
        assert_eq!(table.lookup(&SymbolName::new(b"c")), None);
    }

    #[test]
    fn versions_are_distinct_symbols() {
        let mut table = SymbolTable::new();
        let plain = table.intern(SymbolName::new(b"foo"));
        let v1 = table.intern(SymbolName::with_version(b"foo", Some(b"V1")));
        let v2 = table.intern(SymbolName::with_version(b"foo", Some(b"V2")));
        assert_ne!(plain, v1);
        assert_ne!(v1, v2);
        assert_eq!(
            table.lookup(&SymbolName::with_version(b"foo", Some(b"V1"))),
            Some(v1)
        );
    }

    #[test]
    fn batch_ids_follow_first_occurrence_not_job_order() {
        let late = names_from(&["shared", "late_only", "zzz"]);
        let early = names_from(&["early_only", "shared", "aaa"]);
        let mut late_ids = vec![SymbolId::new(0); late.len()];
        let mut early_ids = vec![SymbolId::new(0); early.len()];

        let mut table = SymbolTable::new();
        table.intern(SymbolName::new(b"pre"));
        // The job for the later position comes first in the slice.
        let mut jobs = [
            InternJob {
                position: InputPosition::new(5, 0),
                names: &late,
                ids: &mut late_ids,
            },
            InternJob {
                position: InputPosition::new(2, 0),
                names: &early,
                ids: &mut early_ids,
            },
        ];
        table.intern_batch(&mut jobs);

        let id = |s: &str| table.lookup(&SymbolName::new(s.as_bytes())).unwrap();
        assert_eq!(id("pre").index(), 0);
        assert_eq!(id("early_only").index(), 1);
        assert_eq!(id("shared").index(), 2);
        assert_eq!(id("aaa").index(), 3);
        assert_eq!(id("late_only").index(), 4);
        assert_eq!(id("zzz").index(), 5);
        assert_eq!(late_ids, [id("shared"), id("late_only"), id("zzz")]);
        assert_eq!(early_ids, [id("early_only"), id("shared"), id("aaa")]);

        // A second batch mixes old and new names.
        let again = names_from(&["new", "shared"]);
        let mut again_ids = vec![SymbolId::new(0); 2];
        table.intern_batch(&mut [InternJob {
            position: InputPosition::new(0, 0),
            names: &again,
            ids: &mut again_ids,
        }]);
        let shared = table.lookup(&SymbolName::new(b"shared")).unwrap();
        assert_eq!(again_ids, [SymbolId::new(6), shared]);
        assert_eq!(table.len(), 7);
    }

    #[test]
    fn equal_positions_fall_back_to_name_order() {
        let a = names_from(&["m", "b"]);
        let b = names_from(&["k", "a"]);
        let mut a_ids = vec![SymbolId::new(0); 2];
        let mut b_ids = vec![SymbolId::new(0); 2];
        let mut table = SymbolTable::new();
        let position = InputPosition::new(1, 0);
        table.intern_batch(&mut [
            InternJob {
                position,
                names: &a,
                ids: &mut a_ids,
            },
            InternJob {
                position,
                names: &b,
                ids: &mut b_ids,
            },
        ]);
        // (pos, 0): "k" < "m"; (pos, 1): "a" < "b".
        assert_eq!(b_ids, [SymbolId::new(0), SymbolId::new(2)]);
        assert_eq!(a_ids, [SymbolId::new(1), SymbolId::new(3)]);
    }

    #[test]
    fn flags_and_definitions_are_per_symbol() {
        let mut table = SymbolTable::new();
        let a = table.intern(SymbolName::new(b"a"));
        let b = table.intern(SymbolName::new(b"b"));
        assert!(table.set_flags(a, SymbolFlags::NEEDS_GOT).is_empty());
        assert!(table.flags(b).is_empty());
        assert_eq!(table.definition(a), Definition::undefined());
        assert_eq!(table.definition_file(a), None);

        let rules = ElfReferenceRules;
        let weak = Definition {
            kind: DefinitionKind::Weak,
            file: FileId::new(1),
            index: 4,
            position: InputPosition::new(1, 0),
            aux: 0,
        };
        let strong = Definition {
            kind: DefinitionKind::Regular,
            file: FileId::new(2),
            index: 9,
            position: InputPosition::new(2, 0),
            aux: 0,
        };
        assert!(table.insert_definition(&rules, a, &weak));
        assert!(table.insert_definition(&rules, a, &strong));
        assert!(!table.insert_definition(&rules, a, &weak));
        assert_eq!(table.definition(a), strong);
        assert_eq!(table.definition_synchronized(a), strong);
        assert_eq!(table.definition_kind(a), DefinitionKind::Regular);
        assert_eq!(table.definition_file(a), Some(FileId::new(2)));
        table.replace_definition(a, &weak);
        assert_eq!(table.definition(a), weak);
        assert_eq!(table.definition(b), Definition::undefined());
    }

    #[test]
    fn many_shards_and_growth() {
        let storage: Vec<String> = (0..50_000).map(|i| format!("sym{i}")).collect();
        let names: Vec<SymbolName<'_>> = storage
            .iter()
            .map(|s| SymbolName::new(s.as_bytes()))
            .collect();
        let mut table = SymbolTable::new();
        let mut ids = vec![SymbolId::new(0); names.len()];
        let (first, second) = names.split_at(25_000);
        let (first_ids, second_ids) = ids.split_at_mut(25_000);
        table.intern_batch(&mut [
            InternJob {
                position: InputPosition::new(1, 0),
                names: second,
                ids: second_ids,
            },
            InternJob {
                position: InputPosition::new(0, 0),
                names: first,
                ids: first_ids,
            },
        ]);
        for (i, id) in ids.iter().enumerate() {
            assert_eq!(id.index(), i);
            assert_eq!(table.name(*id), names[i]);
            assert_eq!(table.lookup(&names[i]), Some(*id));
        }
    }

    /// Interns `names` as one batch of jobs of `chunk` names each, at
    /// positions `0, 1, ..` (or all at position 0), and returns the IDs.
    fn intern_chunks(
        table: &mut SymbolTable<'static>,
        names: &[SymbolName<'static>],
        chunk: usize,
        same_position: bool,
    ) -> Result<Vec<SymbolId>> {
        let mut ids = vec![SymbolId::new(0); names.len()];
        let mut jobs: Vec<InternJob<'static, '_>> = names
            .chunks(chunk)
            .zip(ids.chunks_mut(chunk))
            .enumerate()
            .map(|(j, (names, ids))| InternJob {
                position: InputPosition::new(if same_position { 0 } else { j as u32 }, 0),
                names,
                ids,
            })
            .collect();
        table.try_intern_batch(&mut jobs)?;
        drop(jobs);
        Ok(ids)
    }

    fn leaked_names(prefix: &str, count: usize) -> Vec<SymbolName<'static>> {
        (0..count)
            .map(|i| {
                let text: &'static str = Box::leak(format!("{prefix}{i}").into_boxed_str());
                SymbolName::new(text.as_bytes())
            })
            .collect()
    }

    fn pool(threads: usize) -> rayon::ThreadPool {
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .unwrap()
    }

    #[test]
    fn overflow_is_an_error_and_leaves_the_table_unchanged() {
        // Small and large batches, with distinct and shared positions.
        for (count, chunk, same_position) in [
            (100, 7, false),
            (100, 7, true),
            (100_000, 5000, false),
            (100_000, 5000, true),
        ] {
            let what = format!("{count} names, same position: {same_position}");
            let first = leaked_names("first", 10);
            let names = leaked_names("batch", count);
            pool(4).install(|| {
                let mut table = SymbolTable::new();
                table.set_limit(count);
                intern_chunks(&mut table, &first, 3, false).unwrap();
                let error = intern_chunks(&mut table, &names, chunk, same_position).unwrap_err();
                assert!(matches!(error, Error::Limit(_)), "{what}: {error}");
                assert_eq!(table.len(), 10, "{what}");
                assert!(table.lookup(&names[0]).is_none(), "{what}");
                assert!(table.lookup(&names[count - 1]).is_none(), "{what}");
                assert_eq!(table.lookup(&first[9]), Some(SymbolId::new(9)), "{what}");
                assert_eq!(table.try_intern(names[0]).unwrap(), SymbolId::new(10));

                // The table still works, up to the limit.
                let rest = &names[1..count - 10];
                let ids = intern_chunks(&mut table, rest, chunk, same_position).unwrap();
                assert_eq!(table.len(), count, "{what}");
                assert_eq!(table.lookup(&rest[0]), Some(SymbolId::new(11)), "{what}");
                assert_eq!(table.lookup(&rest[rest.len() - 1]), ids.last().copied());
                assert!(matches!(
                    table.try_intern(names[count - 1]),
                    Err(Error::Limit(_))
                ));
                assert_eq!(table.try_intern(first[0]).unwrap(), SymbolId::new(0));
            });
        }
    }

    #[test]
    #[should_panic(expected = "limit exceeded")]
    fn intern_panics_on_overflow() {
        let mut table = SymbolTable::new();
        table.set_limit(1);
        table.intern(SymbolName::new(b"a"));
        table.intern(SymbolName::new(b"b"));
    }
}
