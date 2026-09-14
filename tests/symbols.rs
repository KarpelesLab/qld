//! Integration tests for the symbol table (workstream W4).
//!
//! Property-style tests generate random inputs from a fixed seed and check
//! that interning and resolution give identical results for every rayon
//! thread count and every order in which inputs are handed over, and that
//! resolution agrees with a naive sequential model of the fixpoint.
//!
//! The `#[ignore]`d stress tests print timings:
//!
//! ```sh
//! cargo test --release --test symbols -- --ignored --nocapture
//! ```

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::Instant;

use qld::symbols::elf_reference::ElfReferenceRules;
use qld::symbols::{
    DefinitionKind, InputPosition, InternJob, Resolution, ResolveFile, SymbolFlags, SymbolName,
    SymbolTable, SymbolUse, resolve_symbols,
};
use qld::{FileId, Result, SymbolId};
use rayon::prelude::*;

// ----- helpers ---------------------------------------------------------------

/// SplitMix64: small, fast, and good enough for test data.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound as u64) as usize
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }

    fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            items.swap(i, self.below(i + 1));
        }
    }
}

fn pool(threads: usize) -> rayon::ThreadPool {
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()
        .expect("thread pool")
}

const THREAD_COUNTS: [usize; 3] = [1, 2, 8];

/// Name storage: bytes plus an optional version, so tests cover both.
struct NamePool {
    entries: Vec<(Vec<u8>, Option<Vec<u8>>)>,
}

impl NamePool {
    fn new(count: usize) -> Self {
        let entries = (0..count)
            .map(|i| {
                // Every seventh name shares its bytes with the previous one
                // but carries a version, so versioned and unversioned
                // spellings of the same bytes coexist.
                if i % 7 == 6 {
                    (
                        format!("sym_{:x}", i - 1).into_bytes(),
                        Some(format!("VERS_{}", i % 3).into_bytes()),
                    )
                } else {
                    (format!("sym_{i:x}").into_bytes(), None)
                }
            })
            .collect();
        Self { entries }
    }

    fn name(&self, index: usize) -> SymbolName<'_> {
        let (bytes, version) = &self.entries[index];
        SymbolName::with_version(bytes, version.as_deref())
    }

    fn display(&self, index: usize) -> String {
        self.name(index).display().to_string()
    }
}

// ----- interning --------------------------------------------------------------

struct JobSpec {
    position: InputPosition,
    names: Vec<usize>,
}

fn random_jobs(rng: &mut Rng, pool_size: usize, jobs: usize, max_len: usize) -> Vec<JobSpec> {
    let mut inputs: Vec<u32> = (0..jobs as u32).collect();
    rng.shuffle(&mut inputs);
    inputs
        .into_iter()
        .map(|input| JobSpec {
            position: InputPosition::new(input, rng.below(3) as u32),
            names: (0..rng.below(max_len + 1))
                .map(|_| {
                    // Skew towards low indices so popular names repeat a lot.
                    let bound = if rng.chance(50) {
                        pool_size / 16
                    } else {
                        pool_size
                    };
                    rng.below(bound.max(1))
                })
                .collect(),
        })
        .collect()
}

/// The single-threaded definition of the expected IDs: walk every batch in
/// position order and number names by first sight.
fn model_ids(names: &NamePool, batches: &[Vec<JobSpec>]) -> Vec<Vec<Vec<u32>>> {
    let mut ids: HashMap<usize, u32> = HashMap::new();
    let mut by_bytes: HashMap<SymbolName<'_>, u32> = HashMap::new();
    batches
        .iter()
        .map(|batch| {
            let mut order: Vec<usize> = (0..batch.len()).collect();
            order.sort_by_key(|&j| batch[j].position);
            for &j in &order {
                for &name in &batch[j].names {
                    let next = by_bytes.len() as u32;
                    let id = *by_bytes.entry(names.name(name)).or_insert(next);
                    ids.insert(name, id);
                }
            }
            batch
                .iter()
                .map(|job| job.names.iter().map(|name| ids[name]).collect())
                .collect()
        })
        .collect()
}

/// Interns every batch, handing jobs over in `order`, and returns the IDs
/// per job (in spec order) plus the table's names.
fn intern_all(
    names: &NamePool,
    batches: &[Vec<JobSpec>],
    rng: &mut Rng,
) -> (Vec<Vec<Vec<u32>>>, Vec<String>) {
    let mut table = SymbolTable::new();
    let mut all = Vec::new();
    for batch in batches {
        let resolved: Vec<Vec<SymbolName<'_>>> = batch
            .iter()
            .map(|job| job.names.iter().map(|&n| names.name(n)).collect())
            .collect();
        let mut outputs: Vec<Vec<SymbolId>> = batch
            .iter()
            .map(|job| vec![SymbolId::new(0); job.names.len()])
            .collect();
        let mut order: Vec<usize> = (0..batch.len()).collect();
        rng.shuffle(&mut order);
        {
            let mut slots: Vec<Option<&mut Vec<SymbolId>>> = outputs.iter_mut().map(Some).collect();
            let mut jobs: Vec<InternJob<'_, '_>> = order
                .iter()
                .map(|&j| InternJob {
                    position: batch[j].position,
                    names: &resolved[j],
                    ids: slots[j].take().unwrap(),
                })
                .collect();
            table.intern_batch(&mut jobs);
        }
        all.push(
            outputs
                .into_iter()
                .map(|ids| ids.into_iter().map(SymbolId::as_u32).collect())
                .collect(),
        );
    }
    let table_names = table
        .names()
        .iter()
        .map(|n| n.display().to_string())
        .collect();
    (all, table_names)
}

#[test]
fn intern_ids_are_independent_of_threads_and_job_order() {
    let names = NamePool::new(3000);
    for seed in 0..4u64 {
        let mut rng = Rng(seed);
        let batches: Vec<Vec<JobSpec>> = (0..3)
            .map(|_| random_jobs(&mut rng, names.entries.len(), 40, 300))
            .collect();
        let expected = model_ids(&names, &batches);

        let mut reference_names = None;
        for threads in THREAD_COUNTS {
            for permutation in 0..3u64 {
                let mut order_rng = Rng(seed * 1000 + permutation);
                let (ids, table_names) =
                    pool(threads).install(|| intern_all(&names, &batches, &mut order_rng));
                assert_eq!(
                    ids, expected,
                    "seed {seed}, {threads} threads, permutation {permutation}"
                );
                match &reference_names {
                    None => reference_names = Some(table_names),
                    Some(reference) => assert_eq!(&table_names, reference),
                }
            }
        }
    }
}

#[test]
fn concurrent_flags_and_definitions_converge() {
    let names = NamePool::new(500);
    let mut table = SymbolTable::new();
    let ids: Vec<SymbolId> = (0..names.entries.len())
        .map(|i| table.intern(names.name(i)))
        .collect();
    let rules = ElfReferenceRules;

    // Many candidates per symbol, offered in a different order each time.
    let mut rng = Rng(42);
    let candidates: Vec<(usize, qld::symbols::Definition)> = (0..20_000)
        .map(|i| {
            let kind = match rng.below(5) {
                0 => DefinitionKind::Lazy,
                1 => DefinitionKind::Shared,
                2 => DefinitionKind::Weak,
                3 => DefinitionKind::Common,
                _ => DefinitionKind::Regular,
            };
            (
                rng.below(ids.len()),
                qld::symbols::Definition {
                    kind,
                    file: FileId::new(i),
                    index: rng.below(4) as u32,
                    position: InputPosition::new(rng.below(50) as u32, rng.below(4) as u32),
                    aux: rng.below(3) as u64,
                },
            )
        })
        .collect();

    let mut reference = None;
    for threads in [1, 2, 8, 16] {
        let mut shuffled = candidates.clone();
        Rng(threads as u64).shuffle(&mut shuffled);
        let fresh = {
            let mut t = SymbolTable::new();
            for i in 0..names.entries.len() {
                t.intern(names.name(i));
            }
            t
        };
        pool(threads).install(|| {
            shuffled.par_iter().for_each(|(symbol, candidate)| {
                let id = ids[*symbol];
                fresh.insert_definition(&rules, id, candidate);
                fresh.set_flags(
                    id,
                    SymbolFlags::from_bits(1 << (candidate.file.index() % 12)),
                );
            });
        });
        let state: Vec<_> = ids
            .iter()
            .map(|&id| (fresh.definition(id), fresh.flags(id)))
            .collect();
        match &reference {
            None => reference = Some(state),
            Some(reference) => assert_eq!(&state, reference, "{threads} threads"),
        }
    }
}

// ----- resolution -------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Use {
    Def(DefinitionKind, u64),
    Ref(bool),
}

#[derive(Clone)]
struct FileSpec {
    /// Which object or archive this is; positions are assigned per layout.
    group: usize,
    member: u32,
    live_at_start: bool,
    symbols: Vec<(usize, Use)>,
}

struct Scenario {
    groups: usize,
    files: Vec<FileSpec>,
}

fn random_scenario(rng: &mut Rng, pool_size: usize) -> Scenario {
    let objects = 4 + rng.below(12);
    let dsos = rng.below(3);
    let archives = 1 + rng.below(5);
    let mut files = Vec::new();
    let unique_names = |rng: &mut Rng, max: usize| {
        let count = 1 + rng.below(max);
        let mut seen = BTreeSet::new();
        (0..count)
            .map(|_| rng.below(pool_size))
            .filter(|n| seen.insert(*n))
            .collect::<Vec<_>>()
    };
    for group in 0..objects {
        let symbols = unique_names(rng, 40)
            .into_iter()
            .map(|n| {
                let u = match rng.below(100) {
                    0..=59 => Use::Ref(rng.chance(10)),
                    60..=84 => Use::Def(DefinitionKind::Regular, 0),
                    85..=92 => Use::Def(DefinitionKind::Weak, 0),
                    _ => Use::Def(DefinitionKind::Common, 1 + rng.below(4) as u64),
                };
                (n, u)
            })
            .collect();
        files.push(FileSpec {
            group,
            member: 0,
            live_at_start: true,
            symbols,
        });
    }
    for group in objects..objects + dsos {
        let symbols = unique_names(rng, 60)
            .into_iter()
            .map(|n| (n, Use::Def(DefinitionKind::Shared, 0)))
            .collect();
        files.push(FileSpec {
            group,
            member: 0,
            live_at_start: true,
            symbols,
        });
    }
    for group in objects + dsos..objects + dsos + archives {
        let members = 1 + rng.below(30);
        let whole_archive = rng.chance(10);
        for member in 0..members as u32 {
            let symbols = unique_names(rng, 12)
                .into_iter()
                .map(|n| {
                    let u = match rng.below(100) {
                        0..=44 => Use::Ref(rng.chance(10)),
                        45..=89 => Use::Def(DefinitionKind::Regular, 0),
                        90..=95 => Use::Def(DefinitionKind::Weak, 0),
                        _ => Use::Def(DefinitionKind::Common, 1 + rng.below(4) as u64),
                    };
                    (n, u)
                })
                .collect();
            files.push(FileSpec {
                group,
                member,
                live_at_start: whole_archive,
                symbols,
            });
        }
    }
    Scenario {
        groups: objects + dsos + archives,
        files,
    }
}

struct TestFile<'a> {
    position: InputPosition,
    live_at_start: bool,
    lazy: Vec<SymbolName<'a>>,
    names: Vec<SymbolName<'a>>,
    uses: Vec<SymbolUse>,
    loaded: bool,
}

impl<'a> ResolveFile<'a> for TestFile<'a> {
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
        assert!(!self.loaded, "file loaded twice");
        self.loaded = true;
        Ok(())
    }

    fn symbol_names(&self) -> &[SymbolName<'a>] {
        assert!(self.loaded, "symbols read before load");
        &self.names
    }

    fn symbol_use(&self, index: usize) -> SymbolUse {
        self.uses[index]
    }
}

/// Positions for one layout: `group_order[g]` is group `g`'s command-line
/// index, `member_order` permutes member indices within each group.
struct Layout {
    group_input: Vec<u32>,
    member_rank: HashMap<(usize, u32), u32>,
}

impl Layout {
    fn identity(scenario: &Scenario) -> Self {
        Self {
            group_input: (0..scenario.groups as u32).collect(),
            member_rank: scenario
                .files
                .iter()
                .map(|f| ((f.group, f.member), f.member))
                .collect(),
        }
    }

    fn shuffled(scenario: &Scenario, rng: &mut Rng) -> Self {
        let mut group_input: Vec<u32> = (0..scenario.groups as u32).collect();
        rng.shuffle(&mut group_input);
        let mut member_rank = HashMap::new();
        for group in 0..scenario.groups {
            let mut members: Vec<u32> = scenario
                .files
                .iter()
                .filter(|f| f.group == group)
                .map(|f| f.member)
                .collect();
            let mut ranks = members.clone();
            rng.shuffle(&mut ranks);
            for (m, r) in members.drain(..).zip(ranks) {
                member_rank.insert((group, m), r);
            }
        }
        Self {
            group_input,
            member_rank,
        }
    }

    fn position(&self, spec: &FileSpec) -> InputPosition {
        InputPosition::new(
            self.group_input[spec.group],
            self.member_rank[&(spec.group, spec.member)],
        )
    }
}

fn build_files<'a>(
    names: &'a NamePool,
    scenario: &Scenario,
    layout: &Layout,
    order: &[usize],
) -> Vec<TestFile<'a>> {
    order
        .iter()
        .map(|&i| {
            let spec = &scenario.files[i];
            let mut lazy = Vec::new();
            let mut file_names = Vec::new();
            let mut uses = Vec::new();
            for &(name, u) in &spec.symbols {
                file_names.push(names.name(name));
                uses.push(match u {
                    Use::Def(kind, aux) => {
                        lazy.push(names.name(name));
                        SymbolUse::Definition { kind, aux }
                    }
                    Use::Ref(weak) => SymbolUse::Reference { weak },
                });
            }
            TestFile {
                position: layout.position(spec),
                live_at_start: spec.live_at_start,
                lazy,
                names: file_names,
                uses,
                loaded: false,
            }
        })
        .collect()
}

/// Everything observable about a resolution, keyed by position and name
/// rather than by `FileId`, so it can be compared across slice orders.
#[derive(PartialEq, Eq, Debug)]
struct Summary {
    names_by_id: Vec<String>,
    live: Vec<InputPosition>,
    extracted: Vec<Vec<InputPosition>>,
    /// Per symbol ID: kind, position, index and aux of the winner, and flags.
    state: Vec<(DefinitionKind, InputPosition, u32, u64, u32)>,
    /// Per live file (by position): its symbol IDs.
    file_ids: Vec<(InputPosition, Vec<u32>)>,
    undefined: Vec<(String, Vec<(InputPosition, u32)>)>,
    duplicates: Vec<(String, InputPosition, Vec<InputPosition>)>,
}

fn summarize(
    table: &SymbolTable<'_>,
    files: &[TestFile<'_>],
    resolution: &Resolution<'_>,
) -> Summary {
    let position = |file: FileId| files[file.index()].position;
    let mut file_ids: Vec<(InputPosition, Vec<u32>)> = resolution
        .live_files()
        .map(|f| {
            (
                position(f),
                resolution
                    .symbol_ids(f)
                    .iter()
                    .map(|id| id.as_u32())
                    .collect(),
            )
        })
        .collect();
    file_ids.sort();
    let mut live: Vec<InputPosition> = resolution.live_files().map(position).collect();
    live.sort();
    Summary {
        names_by_id: table
            .names()
            .iter()
            .map(|n| n.display().to_string())
            .collect(),
        live,
        extracted: resolution
            .extracted()
            .iter()
            .map(|round| round.iter().map(|&f| position(f)).collect())
            .collect(),
        state: table
            .ids()
            .map(|id| {
                let d = table.definition(id);
                (d.kind, d.position, d.index, d.aux, table.flags(id).bits())
            })
            .collect(),
        file_ids,
        undefined: resolution
            .undefined()
            .iter()
            .map(|u| {
                (
                    u.name.display().to_string(),
                    u.references.iter().map(|r| (r.position, r.index)).collect(),
                )
            })
            .collect(),
        duplicates: resolution
            .duplicates()
            .iter()
            .map(|d| {
                (
                    d.name.display().to_string(),
                    d.winner.position,
                    d.others.iter().map(|o| o.position).collect(),
                )
            })
            .collect(),
    }
}

fn resolve_in_pool(threads: usize, files: &mut [TestFile<'_>]) -> Summary {
    pool(threads).install(|| {
        let mut table = SymbolTable::new();
        let resolution = resolve_symbols(&mut table, &ElfReferenceRules, files).expect("resolve");
        summarize(&table, files, &resolution)
    })
}

#[test]
fn resolution_is_independent_of_threads_and_file_order() {
    let names = NamePool::new(400);
    let (mut max_rounds, mut undefined, mut duplicates) = (0, 0, 0);
    for seed in 0..6u64 {
        let mut rng = Rng(0x5eed + seed);
        let scenario = random_scenario(&mut rng, names.entries.len());
        let layout = Layout::shuffled(&scenario, &mut rng);

        let mut reference: Option<Summary> = None;
        for threads in THREAD_COUNTS {
            for permutation in 0..3 {
                let mut order: Vec<usize> = (0..scenario.files.len()).collect();
                if permutation > 0 {
                    rng.shuffle(&mut order);
                }
                let mut files = build_files(&names, &scenario, &layout, &order);
                let summary = resolve_in_pool(threads, &mut files);
                match &reference {
                    None => reference = Some(summary),
                    Some(reference) => assert_eq!(
                        &summary, reference,
                        "seed {seed}, {threads} threads, permutation {permutation}"
                    ),
                }
            }
        }
        let reference = reference.unwrap();
        max_rounds = max_rounds.max(reference.extracted.len());
        undefined += reference.undefined.len();
        duplicates += reference.duplicates.len();
    }
    // Make sure the generated scenarios exercise what they are meant to.
    assert!(max_rounds >= 2, "no multi-round extraction ({max_rounds})");
    assert!(
        undefined > 0 && duplicates > 0,
        "{undefined} / {duplicates}"
    );
}

/// What the naive model predicts, keyed by name.
#[derive(PartialEq, Eq, Debug)]
struct ModelOutcome {
    live: BTreeSet<InputPosition>,
    /// Winner per referenced-or-defined name: kind, position, aux.
    winners: BTreeMap<String, (DefinitionKind, InputPosition, u64)>,
    undefined: BTreeSet<String>,
    duplicates: BTreeSet<String>,
}

/// A deliberately naive, sequential model of the fixpoint: recompute every
/// winner from scratch each round, and extract the lazy winner of every
/// referenced symbol.
fn model_resolution(names: &NamePool, scenario: &Scenario, layout: &Layout) -> ModelOutcome {
    let files = &scenario.files;
    let positions: Vec<InputPosition> = files.iter().map(|f| layout.position(f)).collect();
    let mut live: Vec<bool> = files.iter().map(|f| f.live_at_start).collect();

    // Best candidate key: higher rank, then larger aux for commons, then
    // lower position.
    type Key = (u8, u64, std::cmp::Reverse<InputPosition>);
    let key = |kind: DefinitionKind, aux: u64, position: InputPosition| -> Key {
        let aux = if kind == DefinitionKind::Common {
            aux
        } else {
            0
        };
        (
            ElfReferenceRules::rank(kind),
            aux,
            std::cmp::Reverse(position),
        )
    };

    let winners = |live: &[bool]| {
        let mut best: HashMap<usize, (Key, DefinitionKind, usize, u64)> = HashMap::new();
        for (i, file) in files.iter().enumerate() {
            for &(name, u) in &file.symbols {
                let Use::Def(kind, aux) = u else { continue };
                let kind = if live[i] { kind } else { DefinitionKind::Lazy };
                let aux = if live[i] { aux } else { 0 };
                let k = key(kind, aux, positions[i]);
                let entry = best.entry(name).or_insert((k, kind, i, aux));
                if k > entry.0 {
                    *entry = (k, kind, i, aux);
                }
            }
        }
        best
    };

    loop {
        let best = winners(&live);
        let mut extract = BTreeSet::new();
        for (i, file) in files.iter().enumerate() {
            if !live[i] {
                continue;
            }
            for &(name, u) in &file.symbols {
                if u == Use::Ref(false)
                    && let Some(&(_, DefinitionKind::Lazy, owner, _)) = best.get(&name)
                {
                    extract.insert(owner);
                }
            }
        }
        if extract.is_empty() {
            break;
        }
        for owner in extract {
            live[owner] = true;
        }
    }

    let best = winners(&live);
    let mut undefined = BTreeSet::new();
    let mut duplicates = BTreeSet::new();
    for (i, file) in files.iter().enumerate() {
        if !live[i] {
            continue;
        }
        for &(name, u) in &file.symbols {
            match (u, best.get(&name)) {
                (Use::Ref(false), None | Some((_, DefinitionKind::Lazy, _, _))) => {
                    undefined.insert(names.display(name));
                }
                (
                    Use::Def(DefinitionKind::Regular, _),
                    Some(&(_, DefinitionKind::Regular, owner, _)),
                ) if owner != i => {
                    duplicates.insert(names.display(name));
                }
                _ => {}
            }
        }
    }
    ModelOutcome {
        live: (0..files.len())
            .filter(|&i| live[i])
            .map(|i| positions[i])
            .collect(),
        winners: best
            .into_iter()
            .map(|(name, (_, kind, owner, aux))| {
                (names.display(name), (kind, positions[owner], aux))
            })
            .collect(),
        undefined,
        duplicates,
    }
}

fn outcome(names: &NamePool, scenario: &Scenario, layout: &Layout, threads: usize) -> ModelOutcome {
    let order: Vec<usize> = (0..scenario.files.len()).collect();
    let mut files = build_files(names, scenario, layout, &order);
    pool(threads).install(|| {
        let mut table = SymbolTable::new();
        let resolution =
            resolve_symbols(&mut table, &ElfReferenceRules, &mut files).expect("resolve");
        let live = resolution
            .live_files()
            .map(|f| files[f.index()].position)
            .collect();
        let winners = table
            .ids()
            .filter(|&id| table.definition(id).is_defined())
            .map(|id| {
                let d = table.definition(id);
                (
                    table.name(id).display().to_string(),
                    (d.kind, d.position, d.aux),
                )
            })
            .collect();
        ModelOutcome {
            live,
            winners,
            undefined: resolution
                .undefined()
                .iter()
                .map(|u| u.name.display().to_string())
                .collect(),
            duplicates: resolution
                .duplicates()
                .iter()
                .map(|d| d.name.display().to_string())
                .collect(),
        }
    })
}

#[test]
fn resolution_matches_sequential_model_for_any_command_line_order() {
    let names = NamePool::new(300);
    for seed in 0..8u64 {
        let mut rng = Rng(0xa11ce + seed);
        let scenario = random_scenario(&mut rng, names.entries.len());

        // Which names some file defines at all, and which ones initially live
        // files reference: those must resolve in every layout.
        let defined: BTreeSet<String> = scenario
            .files
            .iter()
            .flat_map(|f| f.symbols.iter())
            .filter(|(_, u)| matches!(u, Use::Def(..)))
            .map(|&(n, _)| names.display(n))
            .collect();

        let mut undefined_sets = BTreeSet::new();
        for layout_index in 0..4 {
            let layout = if layout_index == 0 {
                Layout::identity(&scenario)
            } else {
                Layout::shuffled(&scenario, &mut rng)
            };
            let expected = model_resolution(&names, &scenario, &layout);
            for threads in [1, 8] {
                let actual = outcome(&names, &scenario, &layout, threads);
                assert_eq!(
                    actual, expected,
                    "seed {seed}, layout {layout_index}, {threads} threads"
                );
            }
            // Whether a symbol resolves never depends on order: nothing that
            // some input defines is ever reported undefined.
            for name in &expected.undefined {
                assert!(
                    !defined.contains(name),
                    "{name} is defined somewhere but unresolved"
                );
            }
            undefined_sets.insert(
                expected
                    .undefined
                    .iter()
                    .filter(|name| !defined.contains(*name))
                    .cloned()
                    .collect::<Vec<_>>(),
            );
        }
        // Symbols referenced from initially live files that nothing defines
        // are undefined in every layout.
        let always_live_refs: BTreeSet<String> = scenario
            .files
            .iter()
            .filter(|f| f.live_at_start)
            .flat_map(|f| f.symbols.iter())
            .filter(|(_, u)| *u == Use::Ref(false))
            .map(|&(n, _)| names.display(n))
            .filter(|name| !defined.contains(name))
            .collect();
        for set in &undefined_sets {
            let set: BTreeSet<String> = set.iter().cloned().collect();
            assert!(always_live_refs.is_subset(&set), "seed {seed}");
        }
    }
}

// ----- stress -----------------------------------------------------------------

/// Synthetic names: one byte buffer, each name's span in it, and per job the
/// list of name indices it references.
type SyntheticNames = (Vec<u8>, Vec<(usize, usize)>, Vec<Vec<usize>>);

/// Builds `unique` synthetic names in one buffer, and `jobs` jobs holding
/// `refs` name references in total (each unique name appears at least once).
fn synthetic_names(unique: usize, jobs: usize, refs: usize) -> SyntheticNames {
    let mut buffer = Vec::with_capacity(unique * 24);
    let mut spans = Vec::with_capacity(unique);
    for i in 0..unique {
        let start = buffer.len();
        // Mangled-looking names of varying length.
        let text = format!(
            "_ZN4core3ptr{}drop_in_place{:x}E",
            i % 97,
            i.wrapping_mul(2_654_435_761)
        );
        buffer.extend_from_slice(text.as_bytes());
        spans.push((start, buffer.len()));
    }
    let mut rng = Rng(7);
    let mut lists: Vec<Vec<usize>> = (0..jobs)
        .map(|_| Vec::with_capacity(refs / jobs + 1))
        .collect();
    for i in 0..unique {
        lists[rng.below(jobs)].push(i);
    }
    for _ in unique..refs {
        let name = if rng.chance(30) {
            rng.below(unique / 100 + 1)
        } else {
            rng.below(unique)
        };
        lists[rng.below(jobs)].push(name);
    }
    for list in &mut lists {
        rng.shuffle(list);
    }
    (buffer, spans, lists)
}

#[test]
#[ignore = "stress test; run with --release --ignored --nocapture"]
fn stress_intern_millions_of_symbols() {
    let unique = 4_000_000;
    let refs = 12_000_000;
    let jobs = 4000;
    let started = Instant::now();
    let (buffer, spans, lists) = synthetic_names(unique, jobs, refs);
    println!(
        "generated {unique} unique names, {refs} references in {jobs} jobs: {:?}",
        started.elapsed()
    );

    let max_threads = std::thread::available_parallelism().map_or(8, |n| n.get());
    let mut thread_counts = vec![1];
    while thread_counts.last().unwrap() * 2 <= max_threads {
        thread_counts.push(thread_counts.last().unwrap() * 2);
    }
    if *thread_counts.last().unwrap() != max_threads {
        thread_counts.push(max_threads);
    }

    let mut reference: Option<Vec<Vec<SymbolId>>> = None;
    let mut single_threaded = None;
    for threads in thread_counts {
        let pool = pool(threads);
        pool.install(|| {
            let t = Instant::now();
            let names: Vec<Vec<SymbolName<'_>>> = lists
                .par_iter()
                .map(|list| {
                    list.iter()
                        .map(|&i| SymbolName::new(&buffer[spans[i].0..spans[i].1]))
                        .collect()
                })
                .collect();
            let hash_time = t.elapsed();

            let mut outputs: Vec<Vec<SymbolId>> = lists
                .iter()
                .map(|l| vec![SymbolId::new(0); l.len()])
                .collect();
            let t = Instant::now();
            let mut table = SymbolTable::new();
            {
                let mut batch: Vec<InternJob<'_, '_>> = names
                    .iter()
                    .zip(outputs.iter_mut())
                    .enumerate()
                    .map(|(j, (names, ids))| InternJob {
                        position: InputPosition::new(j as u32, 0),
                        names,
                        ids,
                    })
                    .collect();
                table.intern_batch(&mut batch);
            }
            let intern_time = t.elapsed();
            assert_eq!(table.len(), unique);

            let t = Instant::now();
            let rules = ElfReferenceRules;
            outputs.par_iter().enumerate().for_each(|(j, ids)| {
                for (k, &id) in ids.iter().enumerate() {
                    let candidate = qld::symbols::Definition {
                        kind: if k % 3 == 0 { DefinitionKind::Regular } else { DefinitionKind::Weak },
                        file: FileId::new(j),
                        index: k as u32,
                        position: InputPosition::new(j as u32, 0),
                        aux: 0,
                    };
                    table.insert_definition(&rules, id, &candidate);
                    table.set_flags(id, SymbolFlags::REFERENCED);
                }
            });
            let resolve_time = t.elapsed();

            let speedup = match single_threaded {
                None => {
                    single_threaded = Some(intern_time);
                    1.0
                }
                Some(base) => base.as_secs_f64() / intern_time.as_secs_f64(),
            };
            println!(
                "{threads:>3} threads: hash {hash_time:>10.2?}  intern {intern_time:>10.2?} ({speedup:>5.2}x)  \
                 define+flag {resolve_time:>10.2?}"
            );
            match &reference {
                None => reference = Some(outputs),
                Some(reference) => assert!(reference == &outputs, "IDs differ at {threads} threads"),
            }
        });
    }
}

/// A link shaped like a static glibc "hello world": four objects, three
/// archives with 1,700 members in total, of which 100 are extracted over
/// five rounds, and about 20,000 symbol entries.
fn small_static_link() -> (NamePool, Scenario) {
    const MEMBERS: usize = 1700;
    const LEVELS: usize = 5;
    const PER_LEVEL: usize = 20;
    const DEFS: usize = 8;
    const OBJECT_DEFS: usize = 40;
    // Member `m` defines names `m * DEFS .. (m + 1) * DEFS`; objects define
    // the names after all members'.
    let object_base = MEMBERS * DEFS;
    let names = NamePool::new(object_base + 4 * OBJECT_DEFS + 64);
    let mut rng = Rng(0x611bc);
    let archive_of = |m: usize| match m {
        0..1500 => 0,
        1500..1650 => 1,
        _ => 2,
    };
    // Chain members: level `k` member `j` is member `(k * 331 + j * 17) %
    // MEMBERS`, spread across the archives.
    let chain = |level: usize, j: usize| (level * 331 + j * 17 + 5) % MEMBERS;

    let mut files = Vec::new();
    for object in 0..4 {
        let mut symbols: Vec<(usize, Use)> = (0..OBJECT_DEFS)
            .map(|d| {
                (
                    object_base + object * OBJECT_DEFS + d,
                    Use::Def(DefinitionKind::Regular, 0),
                )
            })
            .collect();
        // Each object references five first-level chain members.
        for j in 0..PER_LEVEL / 4 {
            symbols.push((chain(0, object * 5 + j) * DEFS, Use::Ref(false)));
        }
        files.push(FileSpec {
            group: object,
            member: 0,
            live_at_start: true,
            symbols,
        });
    }
    let mut in_chain = vec![None; MEMBERS];
    for level in 0..LEVELS {
        for j in 0..PER_LEVEL {
            in_chain[chain(level, j)] = Some((level, j));
        }
    }
    let mut member_index = [0u32; 3];
    for (m, chain_slot) in in_chain.iter().enumerate() {
        let mut symbols: Vec<(usize, Use)> = (0..DEFS)
            .map(|d| (m * DEFS + d, Use::Def(DefinitionKind::Regular, 0)))
            .collect();
        match *chain_slot {
            Some((level, j)) if level + 1 < LEVELS => {
                symbols.push((chain(level + 1, j) * DEFS + 1, Use::Ref(false)));
            }
            _ => {}
        }
        // References to object symbols and, from members that stay lazy, to
        // other lazy members' symbols.
        while symbols.len() < DEFS + 4 {
            let name = if chain_slot.is_some() || rng.chance(50) {
                object_base + rng.below(4 * OBJECT_DEFS)
            } else {
                let other = rng.below(MEMBERS);
                if in_chain[other].is_some() || other == m {
                    continue;
                }
                other * DEFS + rng.below(DEFS)
            };
            if symbols.iter().all(|&(n, _)| n != name) {
                symbols.push((name, Use::Ref(rng.chance(5))));
            }
        }
        let archive = archive_of(m);
        files.push(FileSpec {
            group: 4 + archive,
            member: member_index[archive],
            live_at_start: false,
            symbols,
        });
        member_index[archive] += 1;
    }
    (names, Scenario { groups: 7, files })
}

#[test]
#[ignore = "timing test; run with --release --ignored --nocapture"]
fn timing_resolve_small_static_link() {
    let (names, scenario) = small_static_link();
    let layout = Layout::identity(&scenario);
    let order: Vec<usize> = (0..scenario.files.len()).collect();
    let entries: usize = scenario.files.iter().map(|f| f.symbols.len()).sum();
    let iterations = 200;
    if let Ok(load) = std::fs::read_to_string("/proc/loadavg") {
        println!("load average: {}", load.trim());
    }
    let max_threads = std::thread::available_parallelism().map_or(8, |n| n.get());
    let mut reference = None;
    for threads in [1, 8, max_threads.max(64)] {
        let pool = pool(threads);
        let mut times = Vec::with_capacity(iterations);
        let mut summary = None;
        for _ in 0..iterations {
            let mut files = build_files(&names, &scenario, &layout, &order);
            pool.install(|| {
                let started = Instant::now();
                let mut table = SymbolTable::new();
                let resolution = resolve_symbols(&mut table, &ElfReferenceRules, &mut files)
                    .expect("resolve");
                times.push(started.elapsed());
                if summary.is_none() {
                    assert_eq!(resolution.extracted().len(), 5, "rounds");
                    let extracted: usize = resolution.extracted().iter().map(Vec::len).sum();
                    assert_eq!(extracted, 100, "extracted members");
                    println!(
                        "{} files, {entries} symbol entries, {} symbols, {extracted} extracted in {} rounds",
                        files.len(),
                        table.len(),
                        resolution.extracted().len(),
                    );
                    summary = Some(summarize(&table, &files, &resolution));
                }
            });
        }
        times.sort();
        println!(
            "{threads:>3} threads: min {:>9.2?}  median {:>9.2?}  p90 {:>9.2?}",
            times[0],
            times[iterations / 2],
            times[iterations * 9 / 10],
        );
        match &reference {
            None => reference = summary,
            Some(reference) => assert!(
                Some(reference) == summary.as_ref(),
                "results differ at {threads} threads"
            ),
        }
    }
}

#[test]
#[ignore = "stress test; run with --release --ignored --nocapture"]
fn stress_resolve_large_link() {
    let names = NamePool::new(2_000_000);
    let mut rng = Rng(99);
    // 3000 objects and 20 archives of 2000 members each.
    let mut specs = Vec::new();
    for group in 0..3000 {
        let symbols = (0..400)
            .map(|_| {
                let n = rng.below(names.entries.len());
                (
                    n,
                    if rng.chance(70) {
                        Use::Ref(false)
                    } else {
                        Use::Def(DefinitionKind::Weak, 0)
                    },
                )
            })
            .collect::<BTreeMap<_, _>>()
            .into_iter()
            .collect();
        specs.push(FileSpec {
            group,
            member: 0,
            live_at_start: true,
            symbols,
        });
    }
    for group in 3000..3020 {
        for member in 0..2000 {
            let symbols = (0..60)
                .map(|_| {
                    let n = rng.below(names.entries.len());
                    (
                        n,
                        if rng.chance(40) {
                            Use::Ref(false)
                        } else {
                            Use::Def(DefinitionKind::Regular, 0)
                        },
                    )
                })
                .collect::<BTreeMap<_, _>>()
                .into_iter()
                .collect();
            specs.push(FileSpec {
                group,
                member,
                live_at_start: false,
                symbols,
            });
        }
    }
    let scenario = Scenario {
        groups: 3020,
        files: specs,
    };
    let layout = Layout::identity(&scenario);
    let order: Vec<usize> = (0..scenario.files.len()).collect();

    let max_threads = std::thread::available_parallelism().map_or(8, |n| n.get());
    let mut reference = None;
    for threads in [1, 4, max_threads] {
        let mut files = build_files(&names, &scenario, &layout, &order);
        let t = Instant::now();
        let summary = pool(threads).install(|| {
            let mut table = SymbolTable::new();
            let resolution = resolve_symbols(&mut table, &ElfReferenceRules, &mut files).expect("resolve");
            let elapsed = t.elapsed();
            println!(
                "{threads:>3} threads: resolve {elapsed:>10.2?}, {} symbols, {} live files, {} rounds, {} undefined",
                table.len(),
                resolution.live_files().count(),
                resolution.extracted().len(),
                resolution.undefined().len(),
            );
            summarize(&table, &files, &resolution)
        });
        match &reference {
            None => reference = Some(summary),
            Some(reference) => {
                assert!(reference == &summary, "results differ at {threads} threads")
            }
        }
    }
}
