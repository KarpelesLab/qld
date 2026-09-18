//! The format-neutral resolution driver: symbol insertion and archive
//! extraction, iterated to a fixpoint.
//!
//! Each round:
//!
//! 1. **Load** the files that became live (in parallel): initially the
//!    objects and shared libraries on the command line, later the archive
//!    members chosen in the previous round.
//! 2. **Hook**: [`RoundHook::after_load`] sees the newly loaded files, in
//!    input-position order, before any of their symbols is read. A backend
//!    uses it to claim COMDAT groups and to stop reporting definitions from
//!    group copies it discards.
//! 3. **Intern** their global symbol names, and in the first round the lazy
//!    names of every archive member, as one [`intern_batch`] (so IDs stay
//!    deterministic).
//! 4. **Insert and mark**, in parallel: offer each definition, lazy ones
//!    included, to the table, which keeps the best candidate per symbol under
//!    the [`Resolver`]; and set reference flags. A symbol whose
//!    [`REFERENCED`](SymbolFlags::REFERENCED) bit this sets for the first
//!    time is a candidate for extraction.
//! 5. **Choose members**: for each candidate whose best definition
//!    [`extracts`](Resolver::extracts) (by default: is lazy), its defining
//!    member becomes live. Because lazy candidates compete by input position,
//!    that member is the earliest one that defines the symbol.
//!
//! The loop ends when a round chooses no member.
//!
//! Every step's outcome is a function of the previous state and the set of
//! files, never of thread scheduling, so the result is deterministic. It is
//! also independent of command-line order in *whether* a referenced symbol
//! resolves: a reference from any live file extracts a member from any
//! archive, wherever the two sit on the command line (see
//! `docs/compatibility.md`, "Archive resolution order"). Input order only
//! decides *which* definition wins.
//!
//! # Cost
//!
//! A round's work is proportional to the files that became live in it (the
//! first round also covers every lazy file's index names): the driver keeps
//! the round's file indices and never walks the whole file list after the
//! first round. Small rounds, below a few thousand symbols, run on the
//! calling thread, because waking a large pool for them costs more than the
//! work. The final undefined and duplicate reports cover the live files.
//!
//! [`intern_batch`]: SymbolTable::intern_batch

use core::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};

use rayon::prelude::*;

use super::definition::{Definition, DefinitionKind, Resolver};
use super::flags::SymbolFlags;
use super::name::{InputPosition, SymbolName};
use super::report::{DuplicateSymbol, SymbolReference, UndefinedSymbol};
use super::table::{InternJob, LookupView, SymbolTable};
use super::util::select_mut;
use crate::error::{Error, Result};
use crate::ids::{FileId, SymbolId};

/// Files smaller than this many symbols are processed as one parallel task.
const MIN_PARALLEL_SYMBOLS: usize = 1024;
/// Passes over fewer symbols (or candidates) than this, in total, run on the
/// calling thread.
const MIN_PARALLEL_WORK: usize = 4096;
/// Sizing the symbol ID vectors (a memset) runs in parallel only past this
/// many entries.
const MIN_PARALLEL_SIZING: usize = 1 << 20;

/// How one entry of a live file's global symbol list takes part in
/// resolution.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SymbolUse {
    /// The file refers to the symbol without defining it.
    Reference {
        /// A weak reference neither extracts archive members nor makes an
        /// unresolved symbol an error.
        weak: bool,
    },
    /// The file defines the symbol.
    Definition {
        /// The kind of definition. [`DefinitionKind::Lazy`] and
        /// [`DefinitionKind::Undefined`] make little sense here; `Undefined`
        /// is ignored.
        kind: DefinitionKind,
        /// Format-specific precedence data, stored in
        /// [`Definition::aux`].
        aux: u64,
    },
    /// The entry needs an ID but plays no part in resolution.
    Ignore,
}

/// An input file as the resolution driver sees it.
///
/// The format backend implements this for its object, shared-library and
/// archive-member types. The driver identifies files by their index in the
/// slice passed to [`resolve_symbols`]: file `i` is `FileId::new(i)`.
///
/// Linker-created references (the entry point, `-u` options) fit in as a
/// small always-live file at the front of the command line.
pub trait ResolveFile<'a>: Send + Sync {
    /// The file's position on the command line, for tie-breaks.
    fn position(&self) -> InputPosition;

    /// `true` for files that take part from the start: objects, shared
    /// libraries, and `--whole-archive` members. `false` for archive members,
    /// which are lazy until extracted.
    fn is_live_at_start(&self) -> bool;

    /// The names a lazy file would define if extracted, typically from the
    /// archive symbol index. Called once, in the first round, and only for
    /// files that are not live at start.
    fn lazy_names(&self) -> &[SymbolName<'a>];

    /// Makes a newly live file ready for [`symbol_names`](Self::symbol_names)
    /// and [`symbol_use`](Self::symbol_use), usually by parsing it. Called
    /// once per file, in parallel with other files.
    ///
    /// # Errors
    ///
    /// Any error stops resolution. When several files fail in the same round,
    /// the error of the file with the lowest position is returned.
    fn load(&mut self) -> Result<()>;

    /// The file's global symbols, after [`load`](Self::load) and the round's
    /// [`RoundHook::after_load`].
    fn symbol_names(&self) -> &[SymbolName<'a>];

    /// What entry `index` of [`symbol_names`](Self::symbol_names) does.
    /// Called from many threads, only with `index < symbol_names().len()`,
    /// and only after the round's [`RoundHook::after_load`] (but see
    /// [`can_load_early`](Self::can_load_early)).
    fn symbol_use(&self, index: usize) -> SymbolUse;

    /// Whether this lazy file may be loaded before it becomes live, maybe
    /// never to become live: [`load`](Self::load) then has no effect beyond
    /// the file, and [`unload`](Self::unload) undoes it. The default is
    /// `false`.
    ///
    /// When the hook keeps names ([`RoundHook::keeps_names`]), the driver
    /// loads, right after the first round, every such member that the
    /// rounds may extract, in one parallel pass, and reads its names and
    /// the uses of its references before the round that makes it live (a
    /// hook may change the uses of definitions only). The rounds then only
    /// settle which of them become live. A load error is reported only if
    /// the file becomes live.
    fn can_load_early(&self) -> bool {
        false
    }

    /// Returns a file loaded early that never became live to its unloaded
    /// state, typically by dropping what [`load`](Self::load) parsed.
    /// Called once, at the end of resolution. The default does nothing.
    fn unload(&mut self) {}
}

/// A file that became live in the current round, as handed to
/// [`RoundHook::after_load`].
#[derive(Debug)]
pub struct RoundFile<'r, F> {
    /// The file's ID: its index in the slice passed to [`resolve_symbols`].
    pub id: FileId,
    /// The loaded file.
    pub file: &'r mut F,
}

/// A backend hook into each round of [`resolve_symbols_with`].
///
/// The driver calls [`after_load`](Self::after_load) once per round, on the
/// calling thread, after the round's newly live files are loaded and before
/// their [`symbol_names`](ResolveFile::symbol_names) and
/// [`symbol_use`](ResolveFile::symbol_use) are read. Whatever the hook changes
/// in those files is what the round interns and inserts.
///
/// # COMDAT groups
///
/// The intended use is claiming COMDAT groups before insertion, so that
/// definitions from discarded group copies never enter the table:
///
/// 1. For each file in `files` (input-position order), claim each of its
///    groups unless another file already holds the signature. A group
///    claimed in an earlier round stays claimed, even by a file with a
///    higher position. [`GroupClaims`](super::GroupClaims) does this
///    deterministically, and in parallel.
/// 2. In each file that lost a claim, make `symbol_use` report the
///    definitions in the discarded group's sections as
///    [`SymbolUse::Ignore`]. Their names keep their IDs, so relocations
///    against them resolve to the kept copy's definitions.
///
/// Because the kept copy's file is live in the same or an earlier round, its
/// definitions reach the table no later than the discarded ones would have,
/// and a discarded definition is never reported as a duplicate. Copies of a
/// group normally define the same symbols. If a kept copy lacks one, and an
/// archive member was extracted for it, that member's lazy candidate stays
/// in the table and references to the symbol are reported undefined, as with
/// a stale archive index.
///
/// # Determinism
///
/// `files` is sorted by `(position, id)` and holds exactly the files that
/// became live in this round, so a hook that decides by that order (or by
/// lowest position, as [`GroupClaims`](super::GroupClaims) does) gives the
/// same result for every thread count.
///
/// The unit type `()` is the no-op hook [`resolve_symbols`] uses.
pub trait RoundHook<F> {
    /// Whether [`after_load`](Self::after_load) leaves every file's
    /// [`symbol_names`](ResolveFile::symbol_names) as
    /// [`load`](ResolveFile::load) (and [`LoadHook::on_load`]) made them;
    /// it may still change [`symbol_use`](ResolveFile::symbol_use), except
    /// for references. The driver then looks the names up while files load,
    /// in the same parallel task, instead of in a pass of its own, and reads
    /// them before `after_load`; see also
    /// [`ResolveFile::can_load_early`]. The default is `false`.
    fn keeps_names(&self) -> bool {
        false
    }

    /// Work to do on each newly live file right after it loads, in
    /// parallel with the loading of the round's other files; `None` (the
    /// default) for none. See [`LoadHook`].
    fn load_hook(&self) -> Option<&dyn LoadHook<F>> {
        None
    }

    /// Called once per round after loading; see the [trait documentation](Self).
    /// `round` is 0 for the files live at start, and `r` for the members
    /// listed in [`Resolution::extracted`]`()[r - 1]`.
    ///
    /// # Errors
    ///
    /// An error stops resolution and is returned by
    /// [`resolve_symbols_with`].
    fn after_load(&mut self, round: usize, files: &mut [RoundFile<'_, F>]) -> Result<()> {
        let _ = (round, files);
        Ok(())
    }
}

impl<F> RoundHook<F> for () {
    fn keeps_names(&self) -> bool {
        true
    }
}

/// Per-file work of a [`RoundHook`] that can run while files load, in
/// parallel. Neither method may change the file's names or references
/// (they may have been read already, see [`ResolveFile::can_load_early`]).
///
/// - [`prepare`](Self::prepare) is called once per file, right after it
///   loads: in the round that makes it live, or earlier if the driver
///   loads it early. It must not depend on the round.
/// - [`on_load`](Self::on_load) is called in the round that makes the file
///   live, before the round's [`RoundHook::after_load`].
///
/// The driver uses a load hook only when all files have distinct input
/// positions (as input files always do), so that `rank` below is a total
/// order.
///
/// A COMDAT hook uses them to look each group up while the file is in
/// cache and to offer it, round by round, with one atomic operation (see
/// [`GroupSlots`](super::GroupSlots)), leaving `after_load` to settle the
/// claims.
pub trait LoadHook<F>: Sync {
    /// Called for file `id` right after it loads; see the [trait
    /// documentation](Self). The default does nothing.
    ///
    /// # Errors
    ///
    /// As for [`on_load`](Self::on_load), and only reported if the file
    /// becomes live.
    fn prepare(&self, id: FileId, file: &mut F) -> Result<()> {
        let _ = (id, file);
        Ok(())
    }

    /// Called for file `id`, newly live in round `round` (numbered as in
    /// [`RoundHook::after_load`]). `rank` is the file's index among all
    /// files ordered by input position.
    ///
    /// # Errors
    ///
    /// An error stops resolution; as with load errors, the one of the file
    /// with the lowest position is returned.
    fn on_load(&self, round: usize, id: FileId, rank: u32, file: &mut F) -> Result<()>;
}

/// The outcome of [`resolve_symbols`].
#[derive(Debug)]
pub struct Resolution<'a> {
    live: Vec<bool>,
    symbol_ids: Vec<Vec<SymbolId>>,
    extracted: Vec<Vec<FileId>>,
    undefined: Vec<UndefinedSymbol<'a>>,
    duplicates: Vec<DuplicateSymbol<'a>>,
}

impl<'a> Resolution<'a> {
    /// Returns `true` if the file was live at start or was extracted.
    ///
    /// # Panics
    ///
    /// Panics if `file` is out of range for the resolved file slice.
    #[must_use]
    pub fn is_live(&self, file: FileId) -> bool {
        self.live[file.index()]
    }

    /// Returns the live files, in `FileId` order.
    pub fn live_files(&self) -> impl Iterator<Item = FileId> + '_ {
        self.live
            .iter()
            .enumerate()
            .filter(|&(_, &live)| live)
            .map(|(index, _)| FileId::new(index))
    }

    /// Returns the symbol ID of each entry of a live file's
    /// [`symbol_names`](ResolveFile::symbol_names). Empty for files that
    /// never became live.
    ///
    /// # Panics
    ///
    /// Panics if `file` is out of range for the resolved file slice.
    #[must_use]
    pub fn symbol_ids(&self, file: FileId) -> &[SymbolId] {
        &self.symbol_ids[file.index()]
    }

    /// Returns the archive members extracted in each round, each round
    /// sorted by input position. Its length is the number of rounds that
    /// extracted something.
    #[must_use]
    pub fn extracted(&self) -> &[Vec<FileId>] {
        &self.extracted
    }

    /// Returns the symbols referenced (non-weakly) by live files that ended
    /// up without a definition, ordered by first reference.
    #[must_use]
    pub fn undefined(&self) -> &[UndefinedSymbol<'a>] {
        &self.undefined
    }

    /// Returns the duplicate definitions the resolver reported, ordered by
    /// the position of the winning definition.
    #[must_use]
    pub fn duplicates(&self) -> &[DuplicateSymbol<'a>] {
        &self.duplicates
    }
}

/// A step of a resolution round, for [`Steps`].
#[derive(Clone, Copy)]
enum Step {
    Load,
    Hook,
    Intern,
    Insert,
    Choose,
    Report,
    Prefetch,
}

const STEP_NAMES: [&str; 7] = [
    "load", "hook", "intern", "insert", "choose", "report", "prefetch",
];

/// With `QLD_TIMING` set, the time of each step, for the first round and
/// summed over the later ones, printed to stderr as `qld-lap:` lines
/// (`benches/run.py --laps` collects them).
struct Steps {
    last: Option<std::time::Instant>,
    first: [f64; 7],
    later: [f64; 7],
}

impl Steps {
    fn new() -> Self {
        Self {
            last: std::env::var_os("QLD_TIMING").map(|_| std::time::Instant::now()),
            first: [0.0; 7],
            later: [0.0; 7],
        }
    }

    fn lap(&mut self, round: usize, step: Step) {
        let Some(last) = &mut self.last else {
            return;
        };
        let now = std::time::Instant::now();
        let ms = now.duration_since(*last).as_secs_f64() * 1000.0;
        *last = now;
        let totals = if round == 0 {
            &mut self.first
        } else {
            &mut self.later
        };
        totals[step as usize] += ms;
    }

    fn print(&self, rounds: usize) {
        if self.last.is_none() {
            return;
        }
        for (name, ms) in STEP_NAMES.iter().zip(&self.first) {
            if *ms > 0.0 {
                eprintln!("qld-lap: resolve round 0 {name}: {ms:.2} ms");
            }
        }
        for (name, ms) in STEP_NAMES.iter().zip(&self.later) {
            if *ms > 0.0 {
                eprintln!("qld-lap: resolve rounds 1+ {name}: {ms:.2} ms");
            }
        }
        eprintln!("qld-lap: resolve rounds (count): {rounds} ms");
    }
}

/// One file's symbols in a pass: the file index, the IDs of its entries,
/// and whether the entries are its lazy names rather than its real symbols.
#[derive(Clone, Copy)]
struct Work<'s> {
    file: usize,
    ids: &'s [SymbolId],
    lazy: bool,
}

/// Runs `f(file, work, symbol index, symbol ID)` on every entry of `work`
/// and collects the `Some` results, in unspecified order. Parallel only when
/// the total is large; small files are grouped into tasks, and only large
/// files are split.
fn map_symbols<'s, F, T, W>(files: &[F], work: &[Work<'s>], f: W) -> Vec<T>
where
    F: Sync,
    T: Send,
    W: Fn(&F, Work<'s>, usize, SymbolId) -> Option<T> + Sync,
{
    let f = &f;
    let sequential = move |w: &Work<'s>| {
        let (file, w) = (&files[w.file], *w);
        w.ids
            .iter()
            .enumerate()
            .filter_map(move |(symbol, &id)| f(file, w, symbol, id))
    };
    let total: usize = work.iter().map(|w| w.ids.len()).sum();
    if total < MIN_PARALLEL_WORK {
        return work.iter().flat_map(sequential).collect();
    }
    let (large, small): (Vec<Work<'s>>, Vec<Work<'s>>) = work
        .iter()
        .partition(|w| w.ids.len() >= 2 * MIN_PARALLEL_SYMBOLS);
    let small_total: usize = small.iter().map(|w| w.ids.len()).sum();
    let files_per_task = (MIN_PARALLEL_SYMBOLS * small.len() / small_total.max(1)).max(1);
    small
        .par_iter()
        .with_min_len(files_per_task)
        .flat_map_iter(sequential)
        .chain(large.par_iter().flat_map(move |w| {
            let (file, w) = (&files[w.file], *w);
            w.ids
                .par_iter()
                .enumerate()
                .with_min_len(MIN_PARALLEL_SYMBOLS)
                .filter_map(move |(symbol, &id)| f(file, w, symbol, id))
        }))
        .collect()
}

fn sort_unstable_by_key<T, K, G>(items: &mut [T], key: G)
where
    T: Send,
    K: Ord,
    G: Fn(&T) -> K + Sync,
{
    if items.len() < MIN_PARALLEL_WORK {
        items.sort_unstable_by_key(key);
    } else {
        items.par_sort_unstable_by_key(key);
    }
}

/// Resolves the symbols of `files` into `table`, extracting archive members
/// until nothing changes. See the [module documentation](self).
///
/// Runs in the current rayon pool. The result, including every symbol ID, is
/// the same for any thread count.
///
/// This is [`resolve_symbols_with`] and the no-op hook `()`.
///
/// # Errors
///
/// Returns the first (by input position) error from [`ResolveFile::load`] in
/// the round where loading failed, or [`Error::Limit`] if there are more than
/// `u32::MAX` files or the table would exceed
/// [`MAX_SYMBOLS`](super::table::MAX_SYMBOLS) symbols. Undefined and
/// duplicate symbols are not errors here; they are reported in the
/// [`Resolution`].
pub fn resolve_symbols<'a, F, R>(
    table: &mut SymbolTable<'a>,
    resolver: &R,
    files: &mut [F],
) -> Result<Resolution<'a>>
where
    F: ResolveFile<'a>,
    R: Resolver + ?Sized,
{
    resolve_symbols_with(table, resolver, files, &mut ())
}

/// Resolves like [`resolve_symbols`], calling `hook` in every round between
/// loading the newly live files and reading their symbols; see
/// [`RoundHook`].
///
/// # Errors
///
/// As for [`resolve_symbols`], plus any error the hook returns.
pub fn resolve_symbols_with<'a, F, R, H>(
    table: &mut SymbolTable<'a>,
    resolver: &R,
    files: &mut [F],
    hook: &mut H,
) -> Result<Resolution<'a>>
where
    F: ResolveFile<'a>,
    R: Resolver + ?Sized,
    H: RoundHook<F> + ?Sized,
{
    if u32::try_from(files.len()).is_err() {
        return Err(Error::Limit(format!(
            "{} input files (at most {})",
            files.len(),
            u32::MAX
        )));
    }
    let count = files.len();
    let mut live = vec![false; count];
    let mut symbol_ids: Vec<Vec<SymbolId>> = (0..count).map(|_| Vec::new()).collect();
    let mut lazy_ids: Vec<Vec<SymbolId>> = (0..count).map(|_| Vec::new()).collect();
    // The files to load this round, and (first round only) the lazy files,
    // both in index order.
    let (mut load, mut lazy): (Vec<usize>, Vec<usize>) =
        (0..count).partition(|&index| files[index].is_live_at_start());
    let mut extracted = Vec::new();
    // Members loaded ahead of their round: the number of their names the
    // table did not hold then, or their load error.
    let mut early: Vec<Option<Result<usize>>> = (0..count).map(|_| None).collect();
    let mut loaded_early = Vec::new();

    // Names are looked up while files load when the hook keeps them; the
    // lookup's misses are then interned by position, which needs distinct
    // positions (as input files always have).
    // Each file's rank by position, when positions are distinct.
    let ranks: Option<Vec<u32>> = {
        let mut order: Vec<usize> = (0..count).collect();
        order.sort_unstable_by_key(|&index| files[index].position());
        order
            .windows(2)
            .all(|pair| files[pair[0]].position() != files[pair[1]].position())
            .then(|| {
                let mut ranks = vec![0u32; count];
                for (rank, &index) in order.iter().enumerate() {
                    // count <= u32::MAX, checked above.
                    ranks[index] = rank as u32;
                }
                ranks
            })
    };
    let look_up_on_load = hook.keeps_names() && ranks.is_some();
    let mut steps = Steps::new();
    for round in 0.. {
        // Round 0 fills an empty table: nothing to look up.
        let looked_up = look_up_on_load && round > 0;
        let misses = {
            let mut loaded = select_mut(files, &load);
            let view = looked_up.then(|| table.lookup_view());
            let outputs = looked_up.then(|| select_mut(&mut symbol_ids, &load));
            let misses = load_files(
                &mut loaded,
                &load,
                round,
                hook.load_hook().zip(ranks.as_deref()),
                view.as_ref().zip(outputs),
                select_mut(&mut early, &load),
            )?;
            steps.lap(round, Step::Load);
            let mut round_files: Vec<RoundFile<'_, F>> = load
                .iter()
                .zip(loaded)
                .map(|(&index, file)| RoundFile {
                    id: FileId::new(index),
                    file,
                })
                .collect();
            round_files.sort_by_key(|entry| (entry.file.position(), entry.id));
            hook.after_load(round, &mut round_files)?;
            steps.lap(round, Step::Hook);
            misses
        };
        for &index in &load {
            live[index] = true;
        }

        let files_ref = &*files;
        if looked_up {
            if misses > 0 {
                let mut jobs: Vec<InternJob<'a, '_>> = load
                    .iter()
                    .zip(select_mut(&mut symbol_ids, &load))
                    .map(|(&index, ids)| InternJob {
                        position: files_ref[index].position(),
                        names: files_ref[index].symbol_names(),
                        ids: ids.as_mut_slice(),
                    })
                    .collect();
                table.try_intern_missing(&mut jobs)?;
            }
        } else {
            intern_round(
                table,
                files_ref,
                &load,
                &lazy,
                &mut symbol_ids,
                &mut lazy_ids,
            )?;
        }
        steps.lap(round, Step::Intern);
        let work: Vec<Work<'_>> = load
            .iter()
            .map(|&file| Work {
                file,
                ids: &symbol_ids[file],
                lazy: false,
            })
            .chain(lazy.iter().map(|&file| Work {
                file,
                ids: &lazy_ids[file],
                lazy: true,
            }))
            .collect();
        let table_ref = &*table;
        let candidates = insert_and_mark(table_ref, resolver, files_ref, &work);
        steps.lap(round, Step::Insert);

        let live_ref = &live;
        let choose = |&id: &SymbolId| {
            let current = table_ref.definition(id);
            if !current.is_defined() || !resolver.extracts(&current) {
                return None;
            }
            let member = current.file.index();
            (member < count && !live_ref[member]).then_some(member)
        };
        let mut members: Vec<usize> = if candidates.len() < MIN_PARALLEL_WORK {
            candidates.iter().filter_map(choose).collect()
        } else {
            candidates.par_iter().filter_map(choose).collect()
        };
        sort_unstable_by_key(&mut members, |&member| member);
        members.dedup();
        steps.lap(round, Step::Choose);
        if members.is_empty() {
            break;
        }
        if round == 0 && look_up_on_load {
            loaded_early = prefetch(
                files,
                table,
                resolver,
                hook.load_hook().filter(|_| ranks.is_some()),
                &live,
                &members,
                &mut symbol_ids,
                &mut early,
            );
            steps.lap(round, Step::Prefetch);
        }

        for &member in &members {
            // The member's lazy candidates are superseded by its real symbols.
            lazy_ids[member] = Vec::new();
        }
        let mut round_members: Vec<FileId> = members.iter().copied().map(FileId::new).collect();
        round_members.sort_by_key(|file| (files[file.index()].position(), *file));
        extracted.push(round_members);
        load = members;
        lazy = Vec::new();
    }
    drop(lazy_ids);
    for index in loaded_early {
        if !live[index] {
            files[index].unload();
            symbol_ids[index] = Vec::new();
        }
    }

    let live_work: Vec<Work<'_>> = live
        .iter()
        .enumerate()
        .filter(|&(_, &live)| live)
        .map(|(file, _)| Work {
            file,
            ids: &symbol_ids[file],
            lazy: false,
        })
        .collect();
    let undefined = collect_undefined(table, files, &live_work);
    let duplicates = collect_duplicates(table, resolver, files, &live_work);
    steps.lap(0, Step::Report);
    steps.print(extracted.len() + 1);
    Ok(Resolution {
        live,
        symbol_ids,
        extracted,
        undefined,
        duplicates,
    })
}

/// Loads, right after the first round, the members that later rounds may
/// extract (see [`ResolveFile::can_load_early`]), in one parallel pass that
/// follows references as members load: from `start` (the members the first
/// round extracts), every lazy member that a non-weak reference of a loaded
/// file would extract under the first round's definitions. That is a
/// superset of what the rounds extract, since definitions only get better
/// than lazy ones and lazy candidates enter the table in the first round
/// only. Each member's names are looked up as it loads (the table does not
/// change meanwhile); `early` receives the number of names not found, or
/// the load error. Returns the members it loaded.
///
/// The set does not depend on scheduling, and it only decides which files
/// are loaded early, not the resolution.
#[allow(clippy::too_many_arguments)]
fn prefetch<'a, F, R>(
    files: &mut [F],
    table: &mut SymbolTable<'a>,
    resolver: &R,
    load_hook: Option<&dyn LoadHook<F>>,
    live: &[bool],
    start: &[usize],
    symbol_ids: &mut [Vec<SymbolId>],
    early: &mut [Option<Result<usize>>],
) -> Vec<usize>
where
    F: ResolveFile<'a>,
    R: Resolver + ?Sized,
{
    let claimed: Vec<AtomicBool> = live.iter().map(|&live| AtomicBool::new(live)).collect();
    let prefetcher = Prefetcher {
        slots: files
            .iter_mut()
            .zip(symbol_ids.iter_mut())
            .zip(early.iter_mut())
            .map(|((file, ids), early)| Mutex::new(Some((file, ids, early))))
            .collect(),
        claimed,
        view: table.lookup_view(),
        resolver,
        load_hook,
    };
    rayon::scope(|scope| {
        for &member in start {
            if !prefetcher.claimed[member].swap(true, Ordering::Relaxed) {
                let prefetcher = &prefetcher;
                scope.spawn(move |scope| prefetcher.visit(member, scope));
            }
        }
    });
    prefetcher
        .slots
        .iter()
        .enumerate()
        .filter(|(_, slot)| {
            slot.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .as_ref()
                .is_some_and(|(_, _, early)| early.is_some())
        })
        .map(|(index, _)| index)
        .collect()
}

/// A file as [`prefetch`] hands it to the task that loads it: the file, its
/// symbol IDs, and its early load result.
type PrefetchSlot<'p, F> = Option<(
    &'p mut F,
    &'p mut Vec<SymbolId>,
    &'p mut Option<Result<usize>>,
)>;

/// The state of [`prefetch`]: each file, whether a task claimed it, and the
/// table.
struct Prefetcher<'p, 'a, F, R: ?Sized> {
    slots: Vec<Mutex<PrefetchSlot<'p, F>>>,
    claimed: Vec<AtomicBool>,
    view: LookupView<'p, 'a>,
    resolver: &'p R,
    load_hook: Option<&'p dyn LoadHook<F>>,
}

impl<'a, F, R> Prefetcher<'_, 'a, F, R>
where
    F: ResolveFile<'a>,
    R: Resolver + ?Sized,
{
    fn visit<'s>(&'s self, index: usize, scope: &rayon::Scope<'s>) {
        let mut slot = self.slots[index]
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let Some((file, ids, early)) = slot.as_mut() else {
            return;
        };
        if !file.can_load_early() {
            return;
        }
        let loaded = file.load().and_then(|()| match self.load_hook {
            Some(load_hook) => load_hook.prepare(FileId::new(index), file),
            None => Ok(()),
        });
        if let Err(error) = loaded {
            **early = Some(Err(error));
            return;
        }
        let names = file.symbol_names();
        ids.clear();
        ids.resize(names.len(), SymbolId::from_u32(0));
        **early = Some(Ok(self.view.find_all(names, ids)));
        for (symbol, &id) in ids.iter().enumerate() {
            if !LookupView::is_found(id)
                || file.symbol_use(symbol) != (SymbolUse::Reference { weak: false })
            {
                continue;
            }
            let definition = self.view.definition(id);
            if !definition.is_defined() || !self.resolver.extracts(&definition) {
                continue;
            }
            let member = definition.file.index();
            if self
                .claimed
                .get(member)
                .is_some_and(|claimed| !claimed.swap(true, Ordering::Relaxed))
            {
                scope.spawn(move |scope| self.visit(member, scope));
            }
        }
    }
}

/// Loads `files`, whose indices are `indices`, in parallel, running the
/// load hook on each. With `lookup`, also looks each file's names up and
/// writes their IDs (or placeholders, see [`LookupView::find_all`]) to the
/// file's output. Returns the number of names not found.
fn load_files<'a, F: ResolveFile<'a>>(
    files: &mut [&mut F],
    indices: &[usize],
    round: usize,
    load_hook: Option<(&dyn LoadHook<F>, &[u32])>,
    lookup: Option<(&LookupView<'_, 'a>, Vec<&mut Vec<SymbolId>>)>,
    early: Vec<&mut Option<Result<usize>>>,
) -> Result<usize> {
    let (view, outputs): (_, Vec<Option<&mut Vec<SymbolId>>>) = match lookup {
        Some((view, outputs)) => (Some(view), outputs.into_iter().map(Some).collect()),
        None => (None, (0..files.len()).map(|_| None).collect()),
    };
    let one = |file: &mut F,
               index: usize,
               ids: Option<&mut Vec<SymbolId>>,
               early: &mut Option<Result<usize>>|
     -> Result<usize> {
        let id = FileId::new(index);
        if let Some(result) = early.take() {
            // Loaded, prepared and looked up by `prefetch`.
            let missing = result?;
            if let Some((load_hook, ranks)) = load_hook {
                load_hook.on_load(round, id, ranks[index], file)?;
            }
            return Ok(missing);
        }
        file.load()?;
        if let Some((load_hook, ranks)) = load_hook {
            load_hook.prepare(id, file)?;
            load_hook.on_load(round, id, ranks[index], file)?;
        }
        let mut missing = 0;
        if let (Some(ids), Some(view)) = (ids, view) {
            let names = file.symbol_names();
            ids.clear();
            ids.resize(names.len(), SymbolId::from_u32(0));
            missing = view.find_all(names, ids);
        }
        Ok(missing)
    };
    let results: Vec<(InputPosition, usize, Result<usize>)> = files
        .par_iter_mut()
        .zip(indices.par_iter())
        .zip(outputs)
        .zip(early)
        .map(|(((file, &index), ids), early)| {
            (file.position(), index, one(file, index, ids, early))
        })
        .collect();
    let mut missing = 0usize;
    let mut first_error: Option<(InputPosition, usize, Error)> = None;
    for (position, index, result) in results {
        match result {
            Ok(count) => missing += count,
            Err(error) => {
                if first_error
                    .as_ref()
                    .is_none_or(|(p, i, _)| (position, index) < (*p, *i))
                {
                    first_error = Some((position, index, error));
                }
            }
        }
    }
    match first_error {
        Some((_, _, error)) => Err(error),
        None => Ok(missing),
    }
}

/// Interns the symbol names of the `load` files and the lazy names of the
/// `lazy` files (both in index order) as one batch.
fn intern_round<'a, F: ResolveFile<'a>>(
    table: &mut SymbolTable<'a>,
    files: &[F],
    load: &[usize],
    lazy: &[usize],
    symbol_ids: &mut [Vec<SymbolId>],
    lazy_ids: &mut [Vec<SymbolId>],
) -> Result<()> {
    let names = |index: usize, is_lazy: bool| {
        let file = &files[index];
        if is_lazy {
            file.lazy_names()
        } else {
            file.symbol_names()
        }
    };
    // A lazy file whose names are the very same as an earlier one's (the
    // members of an archive named twice on the command line share the
    // index's bytes) gets that file's IDs instead of interning them again:
    // at a later position, its names are never a first occurrence. clang
    // names 22 archives twice, 84,500 of its 298,000 lazy names.
    let repeats = repeated_lazy_files(files, lazy);
    let interned: Vec<usize> = lazy
        .iter()
        .zip(&repeats)
        .filter(|(_, repeat)| repeat.is_none())
        .map(|(&index, _)| index)
        .collect();
    let mut outputs: Vec<(usize, bool, &mut Vec<SymbolId>)> = load
        .iter()
        .zip(select_mut(symbol_ids, load))
        .map(|(&index, ids)| (index, false, ids))
        .chain(
            interned
                .iter()
                .zip(select_mut(lazy_ids, &interned))
                .map(|(&index, ids)| (index, true, ids)),
        )
        .collect();

    // Size the output vectors (large files make this non-trivial).
    let total: usize = outputs
        .iter()
        .map(|(index, is_lazy, _)| names(*index, *is_lazy).len())
        .sum();
    let size = |(index, is_lazy, ids): &mut (usize, bool, &mut Vec<SymbolId>)| {
        ids.clear();
        ids.resize(names(*index, *is_lazy).len(), SymbolId::from_u32(0));
    };
    if total < MIN_PARALLEL_SIZING {
        outputs.iter_mut().for_each(size);
    } else {
        outputs.par_iter_mut().for_each(size);
    }

    let mut jobs: Vec<InternJob<'a, '_>> = outputs
        .into_iter()
        .map(|(index, is_lazy, ids)| InternJob {
            position: files[index].position(),
            names: names(index, is_lazy),
            ids: ids.as_mut_slice(),
        })
        .collect();
    table.try_intern_batch(&mut jobs)?;
    drop(jobs);
    for (&index, repeat) in lazy.iter().zip(&repeats) {
        if let &Some(original) = repeat {
            lazy_ids[index] = lazy_ids[original].clone();
        }
    }
    Ok(())
}

/// For each of the `lazy` files (in index order), the earlier one whose
/// lazy names are the very same (the same bytes in memory, not merely
/// equal), if any.
fn repeated_lazy_files<'a, F: ResolveFile<'a>>(files: &[F], lazy: &[usize]) -> Vec<Option<usize>> {
    let same = |a: &SymbolName<'_>, b: &SymbolName<'_>| {
        std::ptr::eq(a.bytes(), b.bytes())
            && a.version().map(<[u8]>::as_ptr) == b.version().map(<[u8]>::as_ptr)
            && a.version().map(<[u8]>::len) == b.version().map(<[u8]>::len)
    };
    let mut first: hashbrown::HashMap<(usize, usize), usize> = hashbrown::HashMap::new();
    lazy.iter()
        .map(|&index| {
            let names = files[index].lazy_names();
            let head = names.first()?;
            let key = (names.len(), head.bytes().as_ptr() as usize);
            match first.get(&key) {
                Some(&original)
                    if files[original].position() < files[index].position()
                        && files[original]
                            .lazy_names()
                            .iter()
                            .zip(names)
                            .all(|(a, b)| same(a, b)) =>
                {
                    Some(original)
                }
                Some(_) => None,
                None => {
                    first.insert(key, index);
                    None
                }
            }
        })
        .collect()
}

/// Inserts this round's lazy and live definitions and sets the reference
/// flags of its live files. Returns the extraction candidates: the symbols
/// whose `REFERENCED` bit this round set for the first time, and the
/// already referenced symbols whose new best definition extracts (only
/// possible with resolvers where a newly inserted candidate can extract).
/// May contain repeats.
///
/// Inserting and marking in one pass gives the same candidates, after
/// filtering by the final definition, as inserting everything first: a
/// symbol referenced for the first time this round is a candidate either
/// way, and a definition that ends up best was best when inserted.
fn insert_and_mark<'a, F, R>(
    table: &SymbolTable<'a>,
    resolver: &R,
    files: &[F],
    work: &[Work<'_>],
) -> Vec<SymbolId>
where
    F: ResolveFile<'a>,
    R: Resolver + ?Sized,
{
    map_symbols(files, work, |file, w, symbol, id| {
        let (kind, aux) = if w.lazy {
            (DefinitionKind::Lazy, 0)
        } else {
            match file.symbol_use(symbol) {
                SymbolUse::Definition { kind, aux } => (kind, aux),
                SymbolUse::Reference { weak: false } => {
                    let before = table.set_flags(id, SymbolFlags::REFERENCED);
                    return (!before.contains(SymbolFlags::REFERENCED)).then_some(id);
                }
                SymbolUse::Reference { weak: true } => {
                    table.set_flags(id, SymbolFlags::WEAK_REFERENCED);
                    return None;
                }
                SymbolUse::Ignore => return None,
            }
        };
        let candidate = Definition {
            kind,
            file: FileId::new(w.file),
            index: u32::try_from(symbol).unwrap_or(u32::MAX),
            position: file.position(),
            aux,
        };
        let won = table.insert_definition(resolver, id, &candidate);
        (won && resolver.extracts(&candidate) && table.flags(id).contains(SymbolFlags::REFERENCED))
            .then_some(id)
    })
}

fn collect_undefined<'a, F: ResolveFile<'a>>(
    table: &SymbolTable<'a>,
    files: &[F],
    live_work: &[Work<'_>],
) -> Vec<UndefinedSymbol<'a>> {
    let mut references: Vec<(SymbolId, SymbolReference)> =
        map_symbols(files, live_work, |file, w, symbol, id| {
            if file.symbol_use(symbol) != (SymbolUse::Reference { weak: false }) {
                return None;
            }
            let kind = table.definition_kind(id);
            if kind != DefinitionKind::Undefined && kind != DefinitionKind::Lazy {
                return None;
            }
            Some((
                id,
                SymbolReference {
                    position: file.position(),
                    file: FileId::new(w.file),
                    index: u32::try_from(symbol).unwrap_or(u32::MAX),
                },
            ))
        });
    sort_unstable_by_key(&mut references, |&entry| entry);

    let mut undefined: Vec<UndefinedSymbol<'a>> = references
        .chunk_by(|a, b| a.0 == b.0)
        .map(|group| UndefinedSymbol {
            symbol: group[0].0,
            name: table.name(group[0].0),
            references: group.iter().map(|&(_, reference)| reference).collect(),
        })
        .collect();
    undefined.sort_unstable_by_key(|entry| (entry.references[0], entry.symbol));
    undefined
}

fn collect_duplicates<'a, F, R>(
    table: &SymbolTable<'a>,
    resolver: &R,
    files: &[F],
    live_work: &[Work<'_>],
) -> Vec<DuplicateSymbol<'a>>
where
    F: ResolveFile<'a>,
    R: Resolver + ?Sized,
{
    let mut losers: Vec<(SymbolId, Definition)> =
        map_symbols(files, live_work, |file, w, symbol, id| {
            let SymbolUse::Definition { kind, aux } = file.symbol_use(symbol) else {
                return None;
            };
            if kind == DefinitionKind::Undefined {
                return None;
            }
            let definition = Definition {
                kind,
                file: FileId::new(w.file),
                index: u32::try_from(symbol).unwrap_or(u32::MAX),
                position: file.position(),
                aux,
            };
            let winner = table.definition(id);
            (winner != definition && resolver.is_duplicate(&winner, &definition))
                .then_some((id, definition))
        });
    sort_unstable_by_key(&mut losers, |(id, definition)| (*id, definition.tie_key()));

    let mut duplicates: Vec<DuplicateSymbol<'a>> = losers
        .chunk_by(|a, b| a.0 == b.0)
        .map(|group| DuplicateSymbol {
            symbol: group[0].0,
            name: table.name(group[0].0),
            winner: table.definition(group[0].0),
            others: group.iter().map(|&(_, definition)| definition).collect(),
        })
        .collect();
    duplicates.sort_unstable_by_key(|entry| (entry.winner.tie_key(), entry.symbol));
    duplicates
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;
    use crate::symbols::elf_reference::ElfReferenceRules;

    /// A synthetic input. Symbols are written as `"<tag>:<name>"`, where the
    /// tag is `D` (strong definition), `W` (weak definition), `C<size>`
    /// (common), `S` (shared definition), `U` (reference) or `u` (weak
    /// reference).
    struct Mock {
        position: InputPosition,
        live_at_start: bool,
        lazy: Vec<SymbolName<'static>>,
        names: Vec<SymbolName<'static>>,
        uses: Vec<SymbolUse>,
        loads: usize,
        fail: bool,
        /// Set by [`Recorder`]; symbols must not be read before it runs.
        hooked: bool,
        /// Whether the file may be loaded early (and its symbols read
        /// before its round's hook).
        early: bool,
        unloads: usize,
    }

    fn parse(spec: &'static str) -> (SymbolName<'static>, SymbolUse) {
        let (tag, name) = spec.split_once(':').unwrap();
        let use_ = match tag {
            "D" => SymbolUse::Definition {
                kind: DefinitionKind::Regular,
                aux: 0,
            },
            "W" => SymbolUse::Definition {
                kind: DefinitionKind::Weak,
                aux: 0,
            },
            "S" => SymbolUse::Definition {
                kind: DefinitionKind::Shared,
                aux: 0,
            },
            "U" => SymbolUse::Reference { weak: false },
            "u" => SymbolUse::Reference { weak: true },
            common => SymbolUse::Definition {
                kind: DefinitionKind::Common,
                aux: common[1..].parse().unwrap(),
            },
        };
        (SymbolName::new(name.as_bytes()), use_)
    }

    fn file(position: InputPosition, live: bool, specs: &[&'static str]) -> Mock {
        let (names, uses): (Vec<_>, Vec<_>) = specs.iter().map(|spec| parse(spec)).unzip();
        let lazy = names
            .iter()
            .zip(&uses)
            .filter(|(_, use_)| matches!(use_, SymbolUse::Definition { .. }))
            .map(|(name, _)| *name)
            .collect();
        Mock {
            position,
            live_at_start: live,
            lazy,
            names,
            uses,
            loads: 0,
            fail: false,
            hooked: false,
            early: false,
            unloads: 0,
        }
    }

    fn object(input: u32, specs: &[&'static str]) -> Mock {
        file(InputPosition::new(input, 0), true, specs)
    }

    fn member(input: u32, member: u32, specs: &[&'static str]) -> Mock {
        file(InputPosition::new(input, member), false, specs)
    }

    impl<'a> ResolveFile<'a> for Mock {
        fn position(&self) -> InputPosition {
            self.position
        }

        fn is_live_at_start(&self) -> bool {
            self.live_at_start
        }

        fn lazy_names(&self) -> &[SymbolName<'a>] {
            &self.lazy
        }

        fn load(&mut self) -> Result<()> {
            self.loads += 1;
            assert_eq!(self.loads, 1, "file loaded twice");
            if self.fail {
                return Err(Error::malformed(
                    format!("input{}", self.position.input()),
                    0,
                    "test",
                ));
            }
            Ok(())
        }

        fn symbol_names(&self) -> &[SymbolName<'a>] {
            assert!(
                self.loads == 1 && (self.hooked || self.early),
                "symbols read too early"
            );
            &self.names
        }

        fn symbol_use(&self, index: usize) -> SymbolUse {
            assert!(
                self.loads == 1 && (self.hooked || self.early),
                "symbols read too early"
            );
            self.uses[index]
        }

        fn can_load_early(&self) -> bool {
            self.early
        }

        fn unload(&mut self) {
            assert_eq!(self.loads, 1, "unloaded but not loaded");
            self.unloads += 1;
        }
    }

    /// Records each round's files and marks them hooked.
    #[derive(Default)]
    struct Recorder {
        rounds: Vec<Vec<(InputPosition, usize)>>,
        fail_in_round: Option<usize>,
    }

    impl RoundHook<Mock> for Recorder {
        fn after_load(&mut self, round: usize, files: &mut [RoundFile<'_, Mock>]) -> Result<()> {
            assert_eq!(round, self.rounds.len());
            let mut entries = Vec::new();
            for entry in files.iter_mut() {
                assert_eq!(entry.file.loads, 1, "hook before load");
                assert!(!entry.file.hooked, "file handed to the hook twice");
                entry.file.hooked = true;
                entries.push((entry.file.position, entry.id.index()));
            }
            self.rounds.push(entries);
            if self.fail_in_round == Some(round) {
                return Err(Error::Internal(format!("hook failed in round {round}")));
            }
            Ok(())
        }
    }

    fn run(files: &mut [Mock]) -> (SymbolTable<'static>, Resolution<'static>) {
        let mut table = SymbolTable::new();
        let resolution = resolve_symbols_with(
            &mut table,
            &ElfReferenceRules,
            files,
            &mut Recorder::default(),
        )
        .unwrap();
        (table, resolution)
    }

    fn id(table: &SymbolTable<'_>, name: &str) -> SymbolId {
        table.lookup(&SymbolName::new(name.as_bytes())).unwrap()
    }

    fn live(resolution: &Resolution<'_>) -> Vec<usize> {
        resolution.live_files().map(FileId::index).collect()
    }

    #[test]
    fn repeated_archive_gets_the_ids_of_the_first_copy() {
        // An archive named twice: the second copy's members either share
        // the first's names (as members of one mapped archive do, which
        // skips interning them) or have copies of them. IDs and the
        // resolution must not differ.
        let specs: [&[&'static str]; 3] = [&["D:f", "U:h"], &["D:g", "D:h"], &["D:x", "U:y"]];
        let build = |shared: bool| {
            let mut files = vec![object(0, &["U:f", "U:g", "D:main"])];
            let first: Vec<Mock> = specs
                .iter()
                .enumerate()
                .map(|(i, s)| member(1, i as u32, s))
                .collect();
            let mut second: Vec<Mock> = specs
                .iter()
                .enumerate()
                .map(|(i, s)| member(3, i as u32, s))
                .collect();
            for (copy, original) in second.iter_mut().zip(&first) {
                if shared {
                    copy.lazy = original.lazy.clone();
                    copy.names = original.names.clone();
                } else {
                    let fresh = |name: &SymbolName<'static>| {
                        SymbolName::new(Box::leak(name.bytes().to_vec().into_boxed_slice()))
                    };
                    copy.lazy = original.lazy.iter().map(fresh).collect();
                    copy.names = original.names.iter().map(fresh).collect();
                }
            }
            files.extend(first);
            files.push(object(2, &["U:x", "D:z"]));
            files.extend(second);
            files
        };
        let mut shared = build(true);
        let mut copied = build(false);
        let repeats = repeated_lazy_files(&shared, &[1, 2, 3, 5, 6, 7]);
        assert_eq!(repeats, [None, None, None, Some(1), Some(2), Some(3)]);
        let (table_a, resolution_a) = run(&mut shared);
        let (table_b, resolution_b) = run(&mut copied);
        let names = |table: &SymbolTable<'_>| {
            table
                .names()
                .iter()
                .map(|name| name.bytes().to_vec())
                .collect::<Vec<_>>()
        };
        assert_eq!(names(&table_a), names(&table_b));
        assert_eq!(live(&resolution_a), live(&resolution_b));
        for file in 0..shared.len() {
            let file = FileId::new(file);
            assert_eq!(resolution_a.symbol_ids(file), resolution_b.symbol_ids(file));
        }
    }

    #[test]
    fn extracts_transitively_and_skips_unneeded_members() {
        let mut files = [
            object(0, &["U:foo", "D:main"]),
            member(1, 0, &["D:foo", "U:bar"]),
            member(1, 1, &["D:bar"]),
            member(1, 2, &["D:unused"]),
        ];
        let (table, resolution) = run(&mut files);
        assert_eq!(live(&resolution), [0, 1, 2]);
        assert_eq!(
            resolution.extracted(),
            [vec![FileId::new(1)], vec![FileId::new(2)]]
        );
        assert!(resolution.undefined().is_empty());
        assert!(resolution.duplicates().is_empty());
        assert_eq!(
            table.definition_file(id(&table, "bar")),
            Some(FileId::new(2))
        );
        // The unextracted member's symbol stays lazy.
        assert_eq!(
            table.definition_kind(id(&table, "unused")),
            DefinitionKind::Lazy
        );
        assert!(
            table
                .flags(id(&table, "foo"))
                .contains(SymbolFlags::REFERENCED)
        );
        assert_eq!(resolution.symbol_ids(FileId::new(3)), []);
        assert_eq!(
            resolution.symbol_ids(FileId::new(1)),
            [id(&table, "foo"), id(&table, "bar")]
        );
    }

    #[test]
    fn archive_before_its_user_is_still_searched() {
        let mut files = [member(0, 0, &["D:foo"]), object(1, &["U:foo"])];
        let (_, resolution) = run(&mut files);
        assert_eq!(live(&resolution), [0, 1]);
        assert!(resolution.undefined().is_empty());
    }

    #[test]
    fn earliest_member_wins() {
        let mut files = [
            member(5, 0, &["D:foo"]),
            object(0, &["U:foo"]),
            member(2, 3, &["D:foo"]),
            member(2, 4, &["D:foo"]),
        ];
        let (table, resolution) = run(&mut files);
        assert_eq!(live(&resolution), [1, 2]);
        assert_eq!(
            table.definition_file(id(&table, "foo")),
            Some(FileId::new(2))
        );
    }

    #[test]
    fn weak_references_and_existing_definitions_do_not_extract() {
        let mut files = [
            object(
                0,
                &["u:weakref", "U:has_weak", "U:has_shared", "W:has_weak"],
            ),
            object(1, &["S:has_shared"]),
            member(2, 0, &["D:weakref"]),
            member(2, 1, &["D:has_weak"]),
            member(2, 2, &["D:has_shared"]),
        ];
        let (table, resolution) = run(&mut files);
        assert_eq!(live(&resolution), [0, 1]);
        assert!(resolution.undefined().is_empty());
        let weakref = id(&table, "weakref");
        assert!(table.flags(weakref).contains(SymbolFlags::WEAK_REFERENCED));
        assert!(!table.flags(weakref).contains(SymbolFlags::REFERENCED));
        assert_eq!(table.definition_kind(weakref), DefinitionKind::Lazy);
    }

    #[test]
    fn reports_undefined_and_duplicates_in_order() {
        let mut files = [
            object(3, &["U:zeta", "D:dup", "U:alpha"]),
            object(1, &["U:alpha", "D:dup", "C4:common", "u:weak_only"]),
            object(2, &["D:dup", "C8:common", "U:alpha"]),
        ];
        let (table, resolution) = run(&mut files);

        let undefined: Vec<(String, Vec<usize>)> = resolution
            .undefined()
            .iter()
            .map(|u| {
                (
                    u.name.display().to_string(),
                    u.references.iter().map(|r| r.file.index()).collect(),
                )
            })
            .collect();
        assert_eq!(
            undefined,
            [
                ("alpha".to_string(), vec![1, 2, 0]),
                ("zeta".to_string(), vec![0])
            ]
        );

        let [duplicate] = resolution.duplicates() else {
            panic!("expected one duplicate: {:?}", resolution.duplicates());
        };
        assert_eq!(duplicate.symbol, id(&table, "dup"));
        assert_eq!(duplicate.winner.file, FileId::new(1));
        let others: Vec<usize> = duplicate.others.iter().map(|d| d.file.index()).collect();
        assert_eq!(others, [2, 0]);

        let common = table.definition(id(&table, "common"));
        assert_eq!((common.file.index(), common.aux), (2, 8));
    }

    #[test]
    fn stale_lazy_index_terminates_and_reports() {
        let mut lying = member(1, 0, &["D:real"]);
        lying.lazy.push(SymbolName::new(b"ghost"));
        let mut files = [object(0, &["U:ghost", "U:real"]), lying];
        let (_, resolution) = run(&mut files);
        assert_eq!(live(&resolution), [0, 1]);
        let names: Vec<String> = resolution
            .undefined()
            .iter()
            .map(|u| u.name.display().to_string())
            .collect();
        assert_eq!(names, ["ghost"]);
    }

    #[test]
    fn load_error_of_earliest_input_is_returned() {
        let mut files = [object(4, &["U:foo"]), object(2, &["U:bar"]), object(3, &[])];
        files[0].fail = true;
        files[2].fail = true;
        let mut table = SymbolTable::new();
        let error = resolve_symbols(&mut table, &ElfReferenceRules, &mut files).unwrap_err();
        assert!(error.to_string().starts_with("input3"), "{error}");
    }

    #[test]
    fn hook_sees_each_rounds_new_files_in_position_order() {
        let mut files = [
            member(3, 1, &["D:c"]),
            object(2, &["U:a", "U:b"]),
            member(3, 0, &["D:a", "U:c"]),
            object(0, &["D:main"]),
            member(1, 0, &["D:b"]),
            member(1, 1, &["D:unused"]),
        ];
        let mut recorder = Recorder::default();
        let mut table = SymbolTable::new();
        let resolution =
            resolve_symbols_with(&mut table, &ElfReferenceRules, &mut files, &mut recorder)
                .unwrap();
        let p = InputPosition::new;
        assert_eq!(
            recorder.rounds,
            [
                vec![(p(0, 0), 3), (p(2, 0), 1)],
                vec![(p(1, 0), 4), (p(3, 0), 2)],
                vec![(p(3, 1), 0)],
            ]
        );
        assert_eq!(resolution.extracted().len(), 2);
        assert!(!files[5].hooked);
    }

    /// Keeps names and marks files hooked as they load, so the driver
    /// looks their names up while loading.
    #[derive(Default)]
    struct EarlyHook {
        loaded: std::sync::atomic::AtomicUsize,
        rounds: usize,
    }

    impl LoadHook<Mock> for EarlyHook {
        fn on_load(&self, round: usize, _: FileId, _: u32, file: &mut Mock) -> Result<()> {
            assert_eq!(round, self.rounds);
            assert_eq!(file.loads, 1, "hook before load");
            file.hooked = true;
            self.loaded
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        }
    }

    impl RoundHook<Mock> for EarlyHook {
        fn keeps_names(&self) -> bool {
            true
        }

        fn load_hook(&self) -> Option<&dyn LoadHook<Mock>> {
            Some(self)
        }

        fn after_load(&mut self, round: usize, files: &mut [RoundFile<'_, Mock>]) -> Result<()> {
            assert_eq!(round, self.rounds);
            assert!(files.iter().all(|entry| entry.file.hooked));
            self.rounds += 1;
            Ok(())
        }
    }

    #[test]
    fn names_looked_up_while_loading_get_the_same_ids() {
        // Later rounds bring both known names and new ones (`late*`, only
        // referenced or defined by extracted members), in several files.
        let build = || {
            vec![
                object(0, &["U:a", "D:main", "U:b"]),
                member(1, 0, &["D:a", "U:late1", "U:c", "D:late2"]),
                member(1, 1, &["D:b", "U:late2", "U:late3", "U:late1"]),
                member(1, 2, &["D:c", "U:d", "u:late4"]),
                member(2, 0, &["D:d", "U:late5", "D:late3"]),
                member(2, 1, &["D:unused", "U:never"]),
            ]
        };
        let mut plain = build();
        let (table_a, resolution_a) = run(&mut plain);
        for threads in [1, 3] {
            let mut early = build();
            let mut table_b = SymbolTable::new();
            let mut hook = EarlyHook::default();
            let resolution_b = rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .unwrap()
                .install(|| {
                    resolve_symbols_with(&mut table_b, &ElfReferenceRules, &mut early, &mut hook)
                })
                .unwrap();
            assert_eq!(table_a.names(), table_b.names());
            assert_eq!(live(&resolution_a), live(&resolution_b));
            assert_eq!(resolution_a.extracted(), resolution_b.extracted());
            for file in 0..plain.len() {
                let file = FileId::new(file);
                assert_eq!(resolution_a.symbol_ids(file), resolution_b.symbol_ids(file));
            }
            assert_eq!(hook.rounds, resolution_b.extracted().len() + 1);
            assert_eq!(
                hook.loaded.load(std::sync::atomic::Ordering::Relaxed),
                live(&resolution_b).len()
            );
        }
    }

    #[test]
    fn members_loaded_early_resolve_the_same() {
        // `x` would define `s` under the first round's table, so it is
        // loaded early once `a` references `s`; but `c`, extracted in the
        // same round as `a`, defines `s` (weakly), so `x` never becomes
        // live. `bad`, which fails to load, is needed only by `x`. `y1` and
        // `y2` take two more rounds.
        let build = |early: bool| {
            let mut files = vec![
                object(0, &["U:a1", "U:c1", "D:main"]),
                member(1, 0, &["D:s", "U:bad1"]),
                member(1, 1, &["D:a1", "U:s", "U:y1"]),
                member(1, 2, &["D:c1", "W:s"]),
                member(2, 0, &["D:y1", "U:y2", "U:late"]),
                member(2, 1, &["D:y2", "D:late"]),
                member(3, 0, &["D:bad1"]),
            ];
            files[6].fail = true;
            for file in &mut files {
                file.early = early && !file.live_at_start;
            }
            files
        };
        let mut plain = build(false);
        let (table_a, resolution_a) = run(&mut plain);
        assert_eq!(live(&resolution_a), [0, 2, 3, 4, 5]);
        for threads in [1, 3] {
            let mut early = build(true);
            let mut table_b = SymbolTable::new();
            let mut hook = EarlyHook::default();
            let resolution_b = rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .unwrap()
                .install(|| {
                    resolve_symbols_with(&mut table_b, &ElfReferenceRules, &mut early, &mut hook)
                })
                .unwrap();
            assert_eq!(table_a.names(), table_b.names());
            assert_eq!(live(&resolution_a), live(&resolution_b));
            assert_eq!(resolution_a.extracted(), resolution_b.extracted());
            for file in 0..plain.len() {
                let id = FileId::new(file);
                assert_eq!(resolution_a.symbol_ids(id), resolution_b.symbol_ids(id));
            }
            for id in table_a.ids() {
                assert_eq!(table_a.definition(id), table_b.definition(id));
            }
            // `x` and `bad` were loaded early (`bad` failing silently) and
            // unloaded; the others were loaded once and kept.
            for (index, file) in early.iter().enumerate() {
                let dropped = usize::from(index == 1 || index == 6);
                assert_eq!((file.loads, file.unloads), (1, dropped), "file {index}");
            }
        }

        // A member that fails to load is an error once it is extracted,
        // whether it was loaded early or not.
        for early in [false, true] {
            let mut files = build(early);
            files[0].names.push(SymbolName::new(b"bad1"));
            files[0].uses.push(SymbolUse::Reference { weak: false });
            let mut table = SymbolTable::new();
            let error = resolve_symbols_with(
                &mut table,
                &ElfReferenceRules,
                &mut files,
                &mut EarlyHook::default(),
            )
            .unwrap_err();
            assert!(error.to_string().starts_with("input3"), "{error}");
        }
    }

    #[test]
    fn hook_error_stops_resolution() {
        let mut files = [object(0, &["U:a"]), member(1, 0, &["D:a"])];
        let mut recorder = Recorder {
            fail_in_round: Some(1),
            ..Recorder::default()
        };
        let mut table = SymbolTable::new();
        let error = resolve_symbols_with(&mut table, &ElfReferenceRules, &mut files, &mut recorder)
            .unwrap_err();
        assert!(matches!(error, Error::Internal(_)), "{error}");
        assert_eq!(recorder.rounds.len(), 2);
    }

    #[test]
    fn symbol_table_overflow_is_a_limit_error() {
        // Round 0 interns main, a, b; extracting the member adds c and d.
        let mut files = [
            object(0, &["D:main", "U:a"]),
            member(1, 0, &["D:a", "D:b", "U:c", "U:d"]),
        ];
        for (limit, ok) in [(5, true), (4, false), (2, false)] {
            for file in &mut files {
                file.loads = 0;
                file.hooked = false;
            }
            let mut table = SymbolTable::new();
            table.set_limit(limit);
            let result = resolve_symbols_with(
                &mut table,
                &ElfReferenceRules,
                &mut files,
                &mut Recorder::default(),
            );
            match result {
                Ok(_) => assert!(ok, "limit {limit}"),
                Err(Error::Limit(message)) => {
                    assert!(!ok, "limit {limit}: {message}");
                    assert!(table.len() <= limit);
                }
                Err(other) => panic!("limit {limit}: {other}"),
            }
        }
    }
}
