//! The format-neutral resolution driver: symbol insertion and archive
//! extraction, iterated to a fixpoint.
//!
//! Each round:
//!
//! 1. **Load** the files that became live (in parallel): initially the
//!    objects and shared libraries on the command line, later the archive
//!    members chosen in the previous round.
//! 2. **Intern** their global symbol names, and in the first round the lazy
//!    names of every archive member, as one [`intern_batch`] (so IDs stay
//!    deterministic).
//! 3. **Insert and mark**, in parallel: offer each definition, lazy ones
//!    included, to the table, which keeps the best candidate per symbol under
//!    the [`Resolver`]; and set reference flags. A symbol whose
//!    [`REFERENCED`](SymbolFlags::REFERENCED) bit this sets for the first
//!    time is a candidate for extraction.
//! 4. **Choose members**: for each candidate whose best definition
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

use rayon::prelude::*;

use super::definition::{Definition, DefinitionKind, Resolver};
use super::flags::SymbolFlags;
use super::name::{InputPosition, SymbolName};
use super::report::{DuplicateSymbol, SymbolReference, UndefinedSymbol};
use super::table::{InternJob, SymbolTable};
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

    /// The file's global symbols, after [`load`](Self::load).
    fn symbol_names(&self) -> &[SymbolName<'a>];

    /// What entry `index` of [`symbol_names`](Self::symbol_names) does.
    /// Called from many threads, only with `index < symbol_names().len()`.
    fn symbol_use(&self, index: usize) -> SymbolUse;
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

    loop {
        load_files(&mut select_mut(files, &load), &load)?;
        for &index in &load {
            live[index] = true;
        }

        let files_ref = &*files;
        intern_round(
            table,
            files_ref,
            &load,
            &lazy,
            &mut symbol_ids,
            &mut lazy_ids,
        )?;
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
        if members.is_empty() {
            break;
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
    Ok(Resolution {
        live,
        symbol_ids,
        extracted,
        undefined,
        duplicates,
    })
}

/// Loads `files`, whose indices are `indices`, in parallel.
fn load_files<'a, F: ResolveFile<'a>>(files: &mut [&mut F], indices: &[usize]) -> Result<()> {
    let first_error = files
        .par_iter_mut()
        .zip(indices.par_iter())
        .filter_map(|(file, &index)| {
            let position = file.position();
            file.load().err().map(|error| (position, index, error))
        })
        .min_by_key(|(position, index, _)| (*position, *index));
    match first_error {
        Some((_, _, error)) => Err(error),
        None => Ok(()),
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
    let mut outputs: Vec<(usize, bool, &mut Vec<SymbolId>)> = load
        .iter()
        .zip(select_mut(symbol_ids, load))
        .map(|(&index, ids)| (index, false, ids))
        .chain(
            lazy.iter()
                .zip(select_mut(lazy_ids, lazy))
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
    table.try_intern_batch(&mut jobs)
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
            assert_eq!(self.loads, 1, "symbols read before load");
            &self.names
        }

        fn symbol_use(&self, index: usize) -> SymbolUse {
            assert_eq!(self.loads, 1, "symbols read before load");
            self.uses[index]
        }
    }

    fn run(files: &mut [Mock]) -> (SymbolTable<'static>, Resolution<'static>) {
        let mut table = SymbolTable::new();
        let resolution = resolve_symbols(&mut table, &ElfReferenceRules, files).unwrap();
        (table, resolution)
    }

    fn id(table: &SymbolTable<'_>, name: &str) -> SymbolId {
        table.lookup(&SymbolName::new(name.as_bytes())).unwrap()
    }

    fn live(resolution: &Resolution<'_>) -> Vec<usize> {
        resolution.live_files().map(FileId::index).collect()
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
    fn symbol_table_overflow_is_a_limit_error() {
        // Round 0 interns main, a, b; extracting the member adds c and d.
        for (limit, ok) in [(5, true), (4, false), (2, false)] {
            let mut files = [
                object(0, &["D:main", "U:a"]),
                member(1, 0, &["D:a", "D:b", "U:c", "U:d"]),
            ];
            let mut table = SymbolTable::new();
            table.set_limit(limit);
            match resolve_symbols(&mut table, &ElfReferenceRules, &mut files) {
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
