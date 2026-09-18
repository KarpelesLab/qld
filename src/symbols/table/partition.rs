//! Large batches, interned by partitioning names by shard.
//!
//! A batch whose jobs have distinct positions is a sequence of names in
//! first-occurrence order: its jobs, sorted by position, laid end to end.
//! Name `g` of that sequence is new to the table exactly when no name before
//! it is equal, and its ID is then the table length plus the number of new
//! names before it. So IDs need no sort, only a count:
//!
//! 1. **Partition.** The sequence is cut into chunks. One parallel pass
//!    counts each chunk's names per shard, a prefix sum turns the counts into
//!    cursors, and a second pass writes each name's `(g, low hash bits)` to
//!    its shard's region of one array. Within a region, entries stay in
//!    sequence order (a stable counting sort).
//! 2. **Probe.** One task per shard walks its region in order and looks
//!    each name up in the shard's table, with no lock: the tasks own
//!    disjoint shards. A name not found is inserted with a provisional
//!    value, `PENDING | g`; since the region is in sequence order, the first
//!    occurrence of a name is the one that inserts it. The result (a final
//!    ID, or `PENDING` and the first occurrence) replaces the entry, in the
//!    task's own region.
//! 3. **Number.** Each chunk walks its names again, finding their results
//!    by the same cursors, and marks the names that are their own first
//!    occurrence in a bitmap. A prefix sum of its popcounts ranks the new
//!    names. Then, side by side: each shard replaces its provisional slots
//!    with final IDs, the chunks write every name's ID to the jobs, and the
//!    new names are appended to the table in sequence order.
//!
//! Compared with the three passes of the module above, no shard lock is
//! taken (their atomic operations and cache-line transfers dominated the
//! lookup on 16 threads), each shard's table stays in one core's cache while
//! it is probed, and the ordering sort becomes a prefix sum. Every pass
//! writes only memory its task owns, in order: on the development machine,
//! page faults and cache-line transfers from many threads writing to the
//! same pages cost more than the lookups.
//!
//! Batches into a table that already holds names (later resolution rounds)
//! mostly repeat known names. They are looked up first, in parallel and
//! without locks (the table is not modified during the lookup), and only
//! the names not found are interned, in order.

use core::ops::Range;
use core::sync::atomic::{AtomicU64, Ordering};

use hashbrown::hash_table::Entry;
use rayon::prelude::*;

use super::{
    Definitions, InternJob, MIN_PARALLEL_CHUNK, PENDING, SHARD_COUNT, Shard, Slot, SymbolTable,
    get_mut, low32, overflow, shard_of, table_hash,
};
use crate::error::Result;
use crate::ids::SymbolId;
use crate::symbols::definition::Definition;
use crate::symbols::name::SymbolName;

/// The partitioning passes split the sequence into chunks of at least this
/// many names (a multiple of 64, so that chunks own whole bitmap words).
const MIN_CHUNK: usize = 4096;
/// Misses of a lookup pass below this many are interned on the calling
/// thread.
const MIN_PARALLEL_MISSES: usize = 1 << 14;

/// A batch's names in first-occurrence order: segments laid end to end.
struct Sequence<'s, 'a> {
    segments: Vec<&'s [SymbolName<'a>]>,
    /// `starts[k]` is the index of `segments[k][0]` in the sequence; the
    /// last entry is the length of the sequence.
    starts: Vec<usize>,
}

impl<'s, 'a> Sequence<'s, 'a> {
    fn new(segments: Vec<&'s [SymbolName<'a>]>) -> Self {
        let mut starts = Vec::with_capacity(segments.len() + 1);
        let mut total = 0usize;
        for segment in &segments {
            starts.push(total);
            total += segment.len();
        }
        starts.push(total);
        Self { segments, starts }
    }

    fn len(&self) -> usize {
        self.starts[self.segments.len()]
    }

    /// The segment holding name `g`.
    #[inline]
    fn segment_of(&self, g: usize) -> usize {
        // The last segment starting at or before `g`; empty segments share
        // their start with the next one, so this skips them.
        self.starts[..self.segments.len()].partition_point(|&start| start <= g) - 1
    }

    /// Name `g` of the sequence.
    #[inline]
    fn name(&self, g: usize) -> &'s SymbolName<'a> {
        let k = self.segment_of(g);
        &self.segments[k][g - self.starts[k]]
    }

    /// Calls `f(g, name)` for every name in `range`, in order.
    #[inline]
    fn for_each(&self, range: Range<usize>, mut f: impl FnMut(usize, &'s SymbolName<'a>)) {
        if range.is_empty() {
            return;
        }
        let mut k = self.segment_of(range.start);
        let mut g = range.start;
        while g < range.end {
            let (segment, start) = (self.segments[k], self.starts[k]);
            let end = (start + segment.len()).min(range.end);
            for (offset, name) in segment[g - start..end - start].iter().enumerate() {
                f(g + offset, name);
            }
            g = end;
            k += 1;
        }
    }
}

/// Lock-free lookups into a table that nothing modifies meanwhile; see
/// [`SymbolTable::lookup_view`].
pub struct LookupView<'t, 'a> {
    shards: Vec<&'t Shard<'a>>,
    names: &'t [SymbolName<'a>],
    definitions: Definitions<'t>,
}

impl<'a> LookupView<'_, 'a> {
    /// The current definition of `id` (see [`SymbolTable::definition`]).
    ///
    /// # Panics
    ///
    /// Panics if `id` was not issued by the table.
    #[inline]
    #[must_use]
    pub fn definition(&self, id: SymbolId) -> Definition {
        self.definitions.get(id.index())
    }

    /// Whether `id` is an ID [`find_all`](Self::find_all) found, rather
    /// than its placeholder for a name the table does not hold.
    #[inline]
    #[must_use]
    pub fn is_found(id: SymbolId) -> bool {
        id.as_u32() & PENDING == 0
    }

    /// The ID of `name`, or `PENDING` if the table does not hold it.
    #[inline]
    fn find_raw(&self, name: &SymbolName<'a>) -> u32 {
        let h32 = low32(name.hash());
        self.shards[shard_of(name.hash())]
            .table
            .find(table_hash(h32), |slot| {
                slot.h32 == h32 && self.names[slot.value as usize] == *name
            })
            .map_or(PENDING, |slot| slot.value)
    }

    /// Writes the ID of each of `names` to `ids` (which must be as long),
    /// or a placeholder for the names the table does not hold, to be
    /// interned by [`SymbolTable::try_intern_missing`]. Returns the number
    /// of names not found.
    pub fn find_all(&self, names: &[SymbolName<'a>], ids: &mut [SymbolId]) -> usize {
        let mut missing = 0usize;
        for (id, name) in ids.iter_mut().zip(names) {
            let value = self.find_raw(name);
            missing += usize::from(value == PENDING);
            *id = SymbolId::from_u32(value);
        }
        missing
    }
}

fn atomics_u64(len: usize) -> Vec<AtomicU64> {
    if len < 1 << 20 {
        (0..len).map(|_| AtomicU64::new(0)).collect()
    } else {
        (0..len)
            .into_par_iter()
            .map(|_| AtomicU64::new(0))
            .collect()
    }
}

/// Cuts `outputs`, laid end to end, at multiples of `chunk`.
fn split_outputs<'o>(
    outputs: Vec<&'o mut [SymbolId]>,
    chunk: usize,
    chunks: usize,
) -> Vec<Vec<&'o mut [SymbolId]>> {
    let mut pieces: Vec<Vec<&'o mut [SymbolId]>> = (0..chunks).map(|_| Vec::new()).collect();
    let mut g = 0usize;
    for output in outputs {
        let mut rest = output;
        while !rest.is_empty() {
            let c = g / chunk;
            let take = ((c + 1) * chunk - g).min(rest.len());
            let (head, tail) = core::mem::take(&mut rest).split_at_mut(take);
            pieces[c].push(head);
            g += take;
            rest = tail;
        }
    }
    pieces
}

impl<'a> SymbolTable<'a> {
    /// A view for looking names up from many threads without locks, while
    /// the `&mut` borrow keeps the table unchanged.
    pub fn lookup_view(&mut self) -> LookupView<'_, 'a> {
        LookupView {
            shards: self.shards.iter_mut().map(|s| &*get_mut(s)).collect(),
            names: &self.names,
            definitions: Definitions {
                kind: &self.def_kind,
                file: &self.def_file,
                index: &self.def_index,
                position: &self.def_position,
                aux: &self.def_aux,
            },
        }
    }

    /// Interns the names that [`LookupView::find_all`] did not find in
    /// `jobs`, whose `ids` it filled (and which the table has not changed
    /// since), numbering new names by first occurrence as
    /// [`try_intern_batch`](Self::try_intern_batch) does. The jobs'
    /// positions must be distinct.
    ///
    /// # Errors
    ///
    /// As for [`try_intern_batch`](Self::try_intern_batch).
    ///
    /// # Panics
    ///
    /// Panics if two jobs share a position.
    pub fn try_intern_missing(&mut self, jobs: &mut [InternJob<'a, '_>]) -> Result<()> {
        let mut order: Vec<usize> = (0..jobs.len()).collect();
        order.sort_unstable_by_key(|&j| jobs[j].position);
        assert!(
            order
                .windows(2)
                .all(|pair| jobs[pair[0]].position != jobs[pair[1]].position),
            "try_intern_missing: jobs share a position"
        );
        self.intern_missing(jobs, &order)
    }

    /// Whether [`intern_partitioned`](Self::intern_partitioned) can take a
    /// batch of `total` names: sequence indices must fit beside the
    /// provisional mark.
    pub(super) fn can_partition(total: usize) -> bool {
        total < PENDING as usize
    }

    /// Interns a batch whose jobs have distinct positions (`order` lists
    /// them by position), by partitioning; see the [module
    /// documentation](self). `total` is the number of names.
    pub(super) fn intern_partitioned(
        &mut self,
        jobs: &mut [InternJob<'a, '_>],
        order: &[usize],
        total: usize,
    ) -> Result<()> {
        if self.names.is_empty() {
            let sequence = Sequence::new(order.iter().map(|&j| jobs[j].names).collect());
            let mut outputs: Vec<Option<&mut [SymbolId]>> =
                jobs.iter_mut().map(|job| Some(&mut *job.ids)).collect();
            let ordered = order
                .iter()
                .map(|&j| outputs[j].take().unwrap_or_default())
                .collect();
            return self.number_sequence(&sequence, ordered);
        }

        if self.look_up_known(jobs, total) == 0 {
            return Ok(());
        }
        self.intern_missing(jobs, order)
    }

    /// Interns the names of `jobs` whose `ids` hold `PENDING` (the others
    /// hold the IDs a lookup found), in first-occurrence order. `order`
    /// lists the jobs by position; positions are distinct.
    pub(super) fn intern_missing(
        &mut self,
        jobs: &mut [InternJob<'a, '_>],
        order: &[usize],
    ) -> Result<()> {
        // The names not found, in first-occurrence order, and where their
        // IDs go.
        let missing: Vec<(usize, Vec<u32>)> = order
            .par_iter()
            .map(|&j| {
                let job = &jobs[j];
                let indices = job
                    .ids
                    .iter()
                    .enumerate()
                    .filter(|(_, id)| id.as_u32() & PENDING != 0)
                    .map(|(index, _)| index as u32)
                    .collect();
                (j, indices)
            })
            .collect();
        let names: Vec<SymbolName<'a>> = missing
            .iter()
            .flat_map(|(j, indices)| {
                let names = jobs[*j].names;
                indices.iter().map(move |&index| names[index as usize])
            })
            .collect();
        let mut ids = vec![SymbolId::from_u32(0); names.len()];
        if names.len() < MIN_PARALLEL_MISSES {
            let base = self.names.len();
            for (id, name) in ids.iter_mut().zip(&names) {
                match self.intern_name(*name) {
                    Some(interned) => *id = interned,
                    None => {
                        self.forget_names_from(base);
                        return Err(overflow(self.limit));
                    }
                }
            }
            self.grow_state(self.names.len() - base);
        } else {
            let sequence = Sequence::new(vec![names.as_slice()]);
            self.number_sequence(&sequence, vec![ids.as_mut_slice()])?;
        }
        let mut next = ids.into_iter();
        for (j, indices) in missing {
            let job = &mut jobs[j];
            for (index, id) in indices.into_iter().zip(next.by_ref()) {
                job.ids[index as usize] = id;
            }
        }
        Ok(())
    }

    /// Looks up every name of `jobs` (`total` names) without modifying the
    /// table, in parallel and without locks. Found names get their IDs;
    /// the others get `PENDING`. Returns the number of names not found.
    fn look_up_known(&mut self, jobs: &mut [InternJob<'a, '_>], total: usize) -> usize {
        let view = self.lookup_view();
        let find = |name: &SymbolName<'a>| -> u32 { view.find_raw(name) };
        let find = &find;
        let jobs_per_task = (MIN_PARALLEL_CHUNK * jobs.len() / total.max(1)).max(1);
        jobs.par_iter_mut()
            .with_min_len(jobs_per_task)
            .map(|job| {
                let one = |(id, name): (&mut SymbolId, &SymbolName<'a>)| {
                    let value = find(name);
                    *id = SymbolId::from_u32(value);
                    usize::from(value == PENDING)
                };
                if job.ids.len() >= 2 * MIN_PARALLEL_CHUNK {
                    job.ids
                        .par_iter_mut()
                        .zip(job.names.par_iter())
                        .with_min_len(MIN_PARALLEL_CHUNK)
                        .map(one)
                        .sum::<usize>()
                } else {
                    job.ids.iter_mut().zip(job.names).map(one).sum()
                }
            })
            .sum()
    }

    /// Interns `sequence`, numbering new names in sequence order, and writes
    /// the ID of every name to `outputs` (one per segment, as long as it);
    /// see the [module documentation](self). On error the table is
    /// unchanged.
    fn number_sequence(
        &mut self,
        sequence: &Sequence<'_, 'a>,
        outputs: Vec<&mut [SymbolId]>,
    ) -> Result<()> {
        let total = sequence.len();
        debug_assert!(Self::can_partition(total));
        let threads = rayon::current_num_threads().max(1);
        let chunk = total
            .div_ceil(threads * 4)
            .max(MIN_CHUNK)
            .next_multiple_of(64);
        let chunks = total.div_ceil(chunk).max(1);
        let range = |c: usize| c * chunk..((c + 1) * chunk).min(total);

        // Pass 1: count each chunk's names per shard, then turn the counts
        // into each chunk's cursors into the shard regions. Counts are below
        // 2^31: the batch holds fewer names.
        let mut cursors = vec![0u32; chunks * SHARD_COUNT];
        cursors
            .par_chunks_mut(SHARD_COUNT)
            .enumerate()
            .for_each(|(c, row)| {
                sequence.for_each(range(c), |_, name| row[shard_of(name.hash())] += 1);
            });
        let mut running = vec![0u32; SHARD_COUNT];
        for row in cursors.chunks(SHARD_COUNT) {
            for (sum, &count) in running.iter_mut().zip(row) {
                *sum += count;
            }
        }
        let mut region = Vec::with_capacity(SHARD_COUNT + 1);
        let mut start = 0u32;
        for sum in &mut running {
            region.push(start);
            start += *sum;
            *sum = region[region.len() - 1];
        }
        region.push(start);
        for row in cursors.chunks_mut(SHARD_COUNT) {
            for (next, cursor) in running.iter_mut().zip(row) {
                let count = *cursor;
                *cursor = *next;
                *next += count;
            }
        }
        drop(running);

        // Pass 1, continued: scatter `(g, low hash bits)` into the regions.
        // Afterwards row `c` of `cursors` is where chunk `c + 1` started.
        let entries = atomics_u64(total);
        cursors
            .par_chunks_mut(SHARD_COUNT)
            .enumerate()
            .for_each(|(c, cursor)| {
                sequence.for_each(range(c), |g, name| {
                    let slot = &mut cursor[shard_of(name.hash())];
                    entries[*slot as usize].store(
                        (g as u64) << 32 | u64::from(low32(name.hash())),
                        Ordering::Relaxed,
                    );
                    *slot += 1;
                });
            });
        let first_cursors = |c: usize| -> Vec<u32> {
            if c == 0 {
                region[..SHARD_COUNT].to_vec()
            } else {
                cursors[(c - 1) * SHARD_COUNT..c * SHARD_COUNT].to_vec()
            }
        };

        // Pass 2: probe each shard's region in order; each entry becomes its
        // result.
        {
            let Self { shards, names, .. } = self;
            let names = &*names;
            shards
                .par_iter_mut()
                .enumerate()
                .with_min_len(SHARD_COUNT / 256)
                .for_each(|(s, shard)| {
                    let region = region[s] as usize..region[s + 1] as usize;
                    if region.is_empty() {
                        return;
                    }
                    let table = &mut get_mut(shard).table;
                    if table.is_empty() {
                        table.reserve(region.len(), |slot| table_hash(slot.h32));
                    }
                    for entry in &entries[region] {
                        let value = entry.load(Ordering::Relaxed);
                        let (g, h32) = ((value >> 32) as usize, value as u32);
                        let mut name: Option<&SymbolName<'a>> = None;
                        let found = table.entry(
                            table_hash(h32),
                            |slot| {
                                slot.h32 == h32 && {
                                    let other = if slot.value & PENDING == 0 {
                                        &names[slot.value as usize]
                                    } else {
                                        sequence.name((slot.value & !PENDING) as usize)
                                    };
                                    *other == **name.get_or_insert_with(|| sequence.name(g))
                                }
                            },
                            |slot| table_hash(slot.h32),
                        );
                        let result = match found {
                            Entry::Occupied(occupied) => occupied.get().value,
                            Entry::Vacant(vacant) => {
                                // g < 2^31, see `can_partition`.
                                let value = PENDING | g as u32;
                                vacant.insert(Slot { h32, value });
                                value
                            }
                        };
                        entry.store(u64::from(result), Ordering::Relaxed);
                    }
                });
        }
        let result = |cursor: &mut [u32], name: &SymbolName<'_>| -> u32 {
            let slot = &mut cursor[shard_of(name.hash())];
            let value = entries[*slot as usize].load(Ordering::Relaxed) as u32;
            *slot += 1;
            value
        };

        // Pass 3: mark the names that are their own first occurrence, and
        // rank them.
        let mut words = vec![0u64; total.div_ceil(64)];
        words
            .par_chunks_mut(chunk / 64)
            .enumerate()
            .for_each(|(c, words)| {
                let mut cursor = first_cursors(c);
                let first = c * chunk;
                sequence.for_each(range(c), |g, name| {
                    if result(&mut cursor, name) == PENDING | g as u32 {
                        words[(g - first) / 64] |= 1u64 << (g % 64);
                    }
                });
            });
        let mut before = Vec::with_capacity(words.len());
        let mut new_count = 0u32;
        for word in &words {
            before.push(new_count);
            new_count += word.count_ones();
        }
        let new_count = new_count as usize;
        let base = self.names.len();
        if new_count > self.limit.saturating_sub(base) {
            self.shards
                .par_iter_mut()
                .enumerate()
                .filter(|(s, _)| region[*s] != region[*s + 1])
                .for_each(|(_, shard)| {
                    get_mut(shard)
                        .table
                        .retain(|slot| slot.value & PENDING == 0);
                });
            return Err(overflow(self.limit));
        }
        // base + new_count <= limit <= MAX_SYMBOLS < 2^32.
        let base32 = base as u32;
        let id_of = |value: u32| -> SymbolId {
            if value & PENDING == 0 {
                return SymbolId::from_u32(value);
            }
            let g = (value & !PENDING) as usize;
            let (word, bit) = (g / 64, g % 64);
            let rank = before[word] + (words[word] & ((1u64 << bit) - 1)).count_ones();
            SymbolId::from_u32(base32 + rank)
        };

        // Side by side: final IDs in the shards, IDs to the outputs, and the
        // new names to the table.
        let pieces = split_outputs(outputs, chunk, chunks);
        let Self { shards, names, .. } = self;
        rayon::join(
            || {
                rayon::join(
                    || {
                        shards
                            .par_iter_mut()
                            .enumerate()
                            .with_min_len(SHARD_COUNT / 256)
                            .filter(|(s, _)| region[*s] != region[*s + 1])
                            .for_each(|(_, shard)| {
                                for slot in get_mut(shard).table.iter_mut() {
                                    slot.value = id_of(slot.value).as_u32();
                                }
                            });
                    },
                    || {
                        pieces.into_par_iter().enumerate().for_each(|(c, piece)| {
                            let mut cursor = first_cursors(c);
                            let mut out = piece.into_iter().flat_map(|piece| piece.iter_mut());
                            sequence.for_each(range(c), |_, name| {
                                if let Some(id) = out.next() {
                                    *id = id_of(result(&mut cursor, name));
                                }
                            });
                        });
                    },
                );
            },
            || {
                names.reserve(new_count);
                sequence.for_each(0..total, |g, name| {
                    if words[g / 64] & (1u64 << (g % 64)) != 0 {
                        names.push(*name);
                    }
                });
            },
        );
        self.grow_state(new_count);
        Ok(())
    }
}
