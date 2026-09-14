//! Property tests for the format-neutral passes (GC, ICF, merge sections):
//! results are compared with naive sequential reference implementations on
//! random inputs, checked for independence from the thread count and from
//! input permutation, and malformed merge inputs must never panic.
//!
//! The `#[ignore]`d `bench_*` tests are micro-benchmarks on about a million
//! sections; run them with
//! `cargo test --release --test passes -- --ignored --nocapture`.

use std::collections::{BTreeMap, VecDeque};
use std::time::Instant;

use qld::passes::{
    BitSet, Csr, CsrBuilder, GraphBuilder, IcfInput, IcfMode, IcfReloc, IcfSection, IcfTarget,
    MergeError, MergeGroup, MergeInput, MergeKind, MergeSection, MergedSections, PieceRef,
    ReferenceTree, SectionGraph, SplitSection, collect_garbage, fold_identical, merge_sections,
    merge_split_sections, split_section, why_live,
};
use qld::{SectionId, SymbolId};
use rayon::prelude::*;

/// splitmix64: small, seedable, good enough for test data.
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
}

fn in_pool<R: Send>(threads: usize, f: impl FnOnce() -> R + Send) -> R {
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()
        .expect("thread pool")
        .install(f)
}

const POOLS: [usize; 3] = [1, 2, 8];

fn id(index: usize) -> SectionId {
    SectionId::new(index)
}

fn permutation(rng: &mut Rng, n: usize) -> Vec<usize> {
    let mut perm: Vec<usize> = (0..n).collect();
    for i in (1..n).rev() {
        perm.swap(i, rng.below(i + 1));
    }
    perm
}

// ---------------------------------------------------------------------------
// Garbage collection
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct TestGraph {
    n: usize,
    edges: Vec<(usize, usize)>,
    roots: Vec<usize>,
}

impl TestGraph {
    fn build(&self) -> SectionGraph {
        let mut builder = GraphBuilder::new(self.n);
        for &(from, to) in &self.edges {
            builder.add_edge(id(from), id(to));
        }
        for &root in &self.roots {
            builder.add_root(id(root));
        }
        builder.build().expect("valid graph")
    }

    fn adjacency(&self) -> Vec<Vec<usize>> {
        let mut adjacency = vec![Vec::new(); self.n];
        for &(from, to) in &self.edges {
            adjacency[from].push(to);
        }
        adjacency
    }

    /// Sequential BFS distances from the root set.
    fn distances(&self) -> Vec<Option<usize>> {
        let adjacency = self.adjacency();
        let mut distance = vec![None; self.n];
        let mut queue = VecDeque::new();
        for &root in &self.roots {
            if distance[root].is_none() {
                distance[root] = Some(0);
                queue.push_back(root);
            }
        }
        while let Some(section) = queue.pop_front() {
            let next_distance = distance[section].map(|d| d + 1);
            for &next in &adjacency[section] {
                if distance[next].is_none() {
                    distance[next] = next_distance;
                    queue.push_back(next);
                }
            }
        }
        distance
    }
}

/// Random graphs mixing chains, cycles, diamonds, dense areas and
/// unreachable islands.
fn random_graph(rng: &mut Rng, n: usize) -> TestGraph {
    let mut edges = Vec::new();
    let mut roots = Vec::new();
    let mut start = 0;
    while start < n {
        let len = (1 + rng.below(40)).min(n - start);
        let block: Vec<usize> = (start..start + len).collect();
        match rng.below(6) {
            0 => edges.extend(block.windows(2).map(|w| (w[0], w[1]))),
            1 => {
                edges.extend(block.windows(2).map(|w| (w[0], w[1])));
                edges.push((block[len - 1], block[0]));
            }
            2 if len >= 4 => {
                // Diamonds stacked: i -> i+1, i -> i+2, i+1 -> i+3, i+2 -> i+3.
                for i in (0..len - 3).step_by(3) {
                    edges.push((block[i], block[i + 1]));
                    edges.push((block[i], block[i + 2]));
                    edges.push((block[i + 1], block[i + 3]));
                    edges.push((block[i + 2], block[i + 3]));
                }
            }
            3 => {
                for _ in 0..len * 2 {
                    edges.push((block[rng.below(len)], block[rng.below(len)]));
                }
            }
            4 => {
                // Cross-block edges anywhere.
                for &section in &block {
                    edges.push((section, rng.below(n)));
                }
            }
            _ => {} // island of isolated sections
        }
        if rng.chance(30) {
            roots.push(block[rng.below(len)]);
        }
        start += len;
    }
    if n > 0 && rng.chance(50) {
        roots.push(rng.below(n));
    }
    TestGraph { n, edges, roots }
}

fn check_gc(graph: &TestGraph) {
    let built = graph.build();
    let distance = graph.distances();
    let expected_removed: Vec<SectionId> = (0..graph.n)
        .filter(|&s| distance[s].is_none())
        .map(id)
        .collect();
    let results: Vec<_> = POOLS
        .iter()
        .map(|&threads| in_pool(threads, || collect_garbage(&built)))
        .collect();
    for result in &results {
        assert_eq!(result, &results[0], "GC result depends on thread count");
    }
    let live = &results[0];
    assert_eq!(live.removed(), expected_removed);
    assert_eq!(live.num_live(), graph.n - expected_removed.len());

    let tree = ReferenceTree::new(&built);
    let adjacency = graph.adjacency();
    for (section, &distance) in distance.iter().enumerate() {
        let chain = tree.chain(id(section));
        assert_eq!(chain, why_live(&built, id(section)));
        match (chain, distance) {
            (None, None) => {}
            (Some(chain), Some(d)) => {
                assert_eq!(chain.len(), d + 1, "chain is not a shortest path");
                assert!(graph.roots.contains(&chain[0].index()));
                assert_eq!(chain.last(), Some(&id(section)));
                for pair in chain.windows(2) {
                    assert!(adjacency[pair[0].index()].contains(&pair[1].index()));
                }
            }
            (chain, d) => panic!("section {section}: chain {chain:?}, distance {d:?}"),
        }
    }
}

#[test]
fn gc_matches_reference_on_random_graphs() {
    let mut rng = Rng(1);
    for round in 0..60 {
        let n = match round {
            0 => 0,
            1 => 1,
            _ => 1 + rng.below(3000),
        };
        check_gc(&random_graph(&mut rng, n));
    }
}

#[test]
fn gc_deep_chain_and_wide_star() {
    let n = 200_000;
    let chain = TestGraph {
        n,
        edges: (0..n - 1).map(|i| (i, i + 1)).collect(),
        roots: vec![0],
    };
    let built = chain.build();
    for threads in POOLS {
        assert_eq!(in_pool(threads, || collect_garbage(&built)).num_live(), n);
    }
    let star = TestGraph {
        n,
        edges: (1..n)
            .map(|i| (0, i))
            .chain((1..n).map(|i| (i, n - i)))
            .collect(),
        roots: vec![0],
    };
    let built = star.build();
    for threads in POOLS {
        assert_eq!(in_pool(threads, || collect_garbage(&built)).num_live(), n);
    }
}

#[test]
fn gc_is_permutation_invariant() {
    let mut rng = Rng(2);
    for _ in 0..20 {
        let n = 1 + rng.below(2000);
        let graph = random_graph(&mut rng, n);
        let perm = permutation(&mut rng, graph.n);
        let permuted = TestGraph {
            n: graph.n,
            edges: graph
                .edges
                .iter()
                .map(|&(a, b)| (perm[a], perm[b]))
                .collect(),
            roots: graph.roots.iter().map(|&r| perm[r]).collect(),
        };
        let original = in_pool(8, || collect_garbage(&graph.build()));
        let shuffled = in_pool(8, || collect_garbage(&permuted.build()));
        for (section, &moved) in perm.iter().enumerate() {
            assert_eq!(original.is_live(id(section)), shuffled.is_live(id(moved)));
        }
    }
}

#[test]
fn gc_parallel_graph_construction() {
    let mut rng = Rng(3);
    let graph = random_graph(&mut rng, 50_000);
    let adjacency = graph.adjacency();
    let built = SectionGraph::build_parallel(
        graph.n,
        // Over-count to exercise compaction.
        |section| adjacency[section.index()].len() + 1,
        |section, slot| {
            let targets = &adjacency[section.index()];
            for (dest, &target) in slot.iter_mut().zip(targets) {
                *dest = id(target);
            }
            targets.len()
        },
        graph.roots.iter().map(|&r| id(r)).collect(),
    )
    .expect("valid graph");
    assert_eq!(
        collect_garbage(&built).removed(),
        collect_garbage(&graph.build()).removed()
    );
    assert_eq!(built.num_edges(), graph.edges.len());
}

// ---------------------------------------------------------------------------
// Identical code folding
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct TestSection {
    contents: Vec<u8>,
    key: u64,
    foldable: bool,
    address_significant: bool,
    relocs: Vec<IcfReloc>,
}

fn icf_input(sections: &[TestSection]) -> IcfInput<'_> {
    let mut relocs = CsrBuilder::new(sections.len());
    for (index, section) in sections.iter().enumerate() {
        for &reloc in &section.relocs {
            relocs.push(index, reloc);
        }
    }
    let described = sections
        .iter()
        .map(|section| IcfSection {
            contents: &section.contents,
            key: section.key,
            foldable: section.foldable,
            address_significant: section.address_significant,
        })
        .collect();
    IcfInput::new(described, relocs.build().expect("rows")).expect("valid ICF input")
}

/// Naive reference: signature refinement to a fixed point, O(n²) per round.
fn naive_icf(sections: &[TestSection], mode: IcfMode) -> Vec<SectionId> {
    let n = sections.len();
    let eligible = |i: usize| {
        sections[i].foldable && (mode == IcfMode::All || !sections[i].address_significant)
    };
    let shape = |r: &IcfReloc| {
        let target = match r.target {
            IcfTarget::Section { offset, .. } => (0, offset),
            IcfTarget::Symbol(symbol) => (1, u64::from(symbol.as_u32())),
            IcfTarget::Value(value) => (2, value),
        };
        (r.offset, r.kind, r.addend, target)
    };
    let constant_equal = |a: &TestSection, b: &TestSection| {
        a.key == b.key
            && a.contents == b.contents
            && a.relocs.len() == b.relocs.len()
            && a.relocs
                .iter()
                .zip(&b.relocs)
                .all(|(x, y)| shape(x) == shape(y))
    };
    let mut class: Vec<usize> = (0..n)
        .map(|i| {
            if eligible(i) {
                (0..n)
                    .find(|&j| eligible(j) && constant_equal(&sections[i], &sections[j]))
                    .unwrap_or(i)
            } else {
                i
            }
        })
        .collect();
    loop {
        let targets = |i: usize, class: &[usize]| -> Vec<usize> {
            sections[i]
                .relocs
                .iter()
                .filter_map(|r| match r.target {
                    IcfTarget::Section { section, .. } => Some(class[section.index()]),
                    _ => None,
                })
                .collect()
        };
        let next: Vec<usize> = (0..n)
            .map(|i| {
                if !eligible(i) {
                    return i;
                }
                let mine = targets(i, &class);
                (0..n)
                    .find(|&j| eligible(j) && class[j] == class[i] && targets(j, &class) == mine)
                    .unwrap_or(i)
            })
            .collect();
        if next == class {
            return class.into_iter().map(id).collect();
        }
        class = next;
    }
}

const SNIPPETS: [&[u8]; 3] = [b"\x55\x48\x89\xe5", b"\xc3", b"\x55\x48\x89\xe5"];

fn random_reloc(rng: &mut Rng, position: usize, n: usize, section_bias: u64) -> IcfReloc {
    let target = if rng.chance(section_bias) {
        IcfTarget::Section {
            section: id(rng.below(n)),
            offset: 0,
        }
    } else if rng.chance(50) {
        IcfTarget::Symbol(SymbolId::new(rng.below(2)))
    } else {
        IcfTarget::Value(rng.below(2) as u64)
    };
    IcfReloc {
        offset: (position * 4) as u64,
        kind: 2,
        addend: -4,
        target,
    }
}

/// Random sections with few distinct contents, so many are foldable, plus
/// replicated templates of mutually calling functions (cycles).
fn random_icf_sections(rng: &mut Rng, n: usize) -> Vec<TestSection> {
    let mut sections: Vec<TestSection> = Vec::with_capacity(n);
    while sections.len() < n {
        let remaining = n - sections.len();
        if remaining >= 8 && rng.chance(20) {
            // A template of m functions calling each other, replicated.
            let m = 1 + rng.below(4);
            let copies = (2 + rng.below(3)).min(remaining / m);
            let template: Vec<(usize, Vec<usize>)> = (0..m)
                .map(|_| {
                    let calls = (0..1 + rng.below(2)).map(|_| rng.below(m)).collect();
                    (rng.below(2), calls)
                })
                .collect();
            for _ in 0..copies {
                let base = sections.len();
                for (snippet, calls) in &template {
                    sections.push(TestSection {
                        contents: SNIPPETS[*snippet].to_vec(),
                        key: 7,
                        foldable: !rng.chance(5),
                        address_significant: rng.chance(10),
                        relocs: calls
                            .iter()
                            .enumerate()
                            .map(|(position, &callee)| IcfReloc {
                                offset: (position * 4) as u64,
                                kind: 2,
                                addend: -4,
                                target: IcfTarget::Section {
                                    section: id(base + callee),
                                    offset: 0,
                                },
                            })
                            .collect(),
                    });
                }
            }
        } else {
            let count = rng.below(3);
            let relocs = (0..count)
                .map(|position| random_reloc(rng, position, n, 70))
                .collect();
            sections.push(TestSection {
                contents: SNIPPETS[rng.below(SNIPPETS.len())].to_vec(),
                key: rng.below(2) as u64,
                foldable: rng.chance(90),
                address_significant: rng.chance(20),
                relocs,
            });
        }
    }
    sections
}

#[test]
fn icf_matches_reference_on_random_sections() {
    let mut rng = Rng(4);
    let (mut folded, mut max_rounds, mut safe_differs) = (0, 0, false);
    for round in 0..80 {
        let n = if round == 0 { 0 } else { 1 + rng.below(250) };
        let sections = random_icf_sections(&mut rng, n);
        let input = icf_input(&sections);
        for mode in [IcfMode::All, IcfMode::Safe] {
            let expected = naive_icf(&sections, mode);
            for threads in POOLS {
                let result = in_pool(threads, || fold_identical(&input, mode));
                assert_eq!(
                    result.fold_into(),
                    expected.as_slice(),
                    "round {round}, {mode:?}, {threads} threads"
                );
                folded += result.num_folded();
                max_rounds = max_rounds.max(result.rounds());
            }
        }
        safe_differs |= naive_icf(&sections, IcfMode::All) != naive_icf(&sections, IcfMode::Safe);
    }
    // The generator must exercise folding, refinement and safe mode.
    assert!(folded > 1000, "only {folded} folds");
    assert!(max_rounds >= 3, "at most {max_rounds} rounds");
    assert!(safe_differs);
}

#[test]
fn icf_folds_replicated_cycles() {
    // Three copies of a 3-cycle a -> b -> c -> a, where b and c have the same
    // contents as each other but a differs.
    let mut sections = Vec::new();
    for copy in 0..3 {
        let base = copy * 3;
        for (index, contents) in [&b"A"[..], b"B", b"B"].iter().enumerate() {
            sections.push(TestSection {
                contents: contents.to_vec(),
                key: 0,
                foldable: true,
                address_significant: false,
                relocs: vec![IcfReloc {
                    offset: 0,
                    kind: 1,
                    addend: 0,
                    target: IcfTarget::Section {
                        section: id(base + (index + 1) % 3),
                        offset: 0,
                    },
                }],
            });
        }
    }
    let input = icf_input(&sections);
    let result = fold_identical(&input, IcfMode::All);
    let expected: Vec<SectionId> = (0..9).map(|i| id(i % 3)).collect();
    assert_eq!(result.fold_into(), expected.as_slice());
    assert_eq!(
        result.fold_into(),
        naive_icf(&sections, IcfMode::All).as_slice()
    );
    let report = result.report();
    let groups: Vec<_> = report
        .groups()
        .map(|g| (g.kept, g.folded.to_vec()))
        .collect();
    assert_eq!(
        groups,
        vec![
            (id(0), vec![id(3), id(6)]),
            (id(1), vec![id(4), id(7)]),
            (id(2), vec![id(5), id(8)]),
        ]
    );
}

#[test]
fn icf_is_permutation_invariant() {
    let mut rng = Rng(5);
    for _ in 0..20 {
        let n = 1 + rng.below(3000);
        let sections = random_icf_sections(&mut rng, n);
        let perm = permutation(&mut rng, n);
        let mut permuted = vec![sections[0].clone(); n];
        for (old, section) in sections.iter().enumerate() {
            let mut moved = section.clone();
            for reloc in &mut moved.relocs {
                if let IcfTarget::Section { section, offset } = reloc.target {
                    reloc.target = IcfTarget::Section {
                        section: id(perm[section.index()]),
                        offset,
                    };
                }
            }
            permuted[perm[old]] = moved;
        }
        let original = in_pool(8, || fold_identical(&icf_input(&sections), IcfMode::Safe));
        let shuffled = in_pool(2, || fold_identical(&icf_input(&permuted), IcfMode::Safe));
        // Same partition: a and b share a class before iff they do after.
        let before = original.fold_into();
        let after = shuffled.fold_into();
        let mut class_map = BTreeMap::new();
        for old in 0..n {
            let pair = (before[old], after[perm[old]]);
            let mapped = *class_map.entry(pair.0).or_insert(pair.1);
            assert_eq!(mapped, pair.1, "partition differs under permutation");
        }
        let distinct_before: std::collections::BTreeSet<_> = before.iter().collect();
        let distinct_after: std::collections::BTreeSet<_> = after.iter().collect();
        assert_eq!(distinct_before.len(), distinct_after.len());
        // Representatives are the lowest member in each numbering.
        for (index, rep) in after.iter().enumerate() {
            assert!(rep.index() <= index);
        }
    }
}

#[test]
fn icf_rejects_invalid_input() {
    let contents = [0u8; 4];
    let section = IcfSection {
        contents: &contents,
        key: 0,
        foldable: true,
        address_significant: false,
    };
    let bad_target = Csr::from_parts(
        vec![0, 1],
        vec![IcfReloc {
            offset: 0,
            kind: 0,
            addend: 0,
            target: IcfTarget::Section {
                section: id(1),
                offset: 0,
            },
        }],
    )
    .expect("offsets");
    assert!(IcfInput::new(vec![section], bad_target).is_err());
    assert!(IcfInput::new(vec![section, section], Csr::empty(1)).is_err());
}

// ---------------------------------------------------------------------------
// Merge sections
// ---------------------------------------------------------------------------

fn words(rng: &mut Rng, count: usize) -> Vec<Vec<u8>> {
    // Short words over a tiny alphabet, so suffix relations are frequent.
    (0..count)
        .map(|_| {
            (0..rng.below(6))
                .map(|_| b'a' + rng.below(3) as u8)
                .collect()
        })
        .collect()
}

fn string_section(rng: &mut Rng, vocabulary: &[Vec<u8>], char_size: usize) -> Vec<u8> {
    let mut data = Vec::new();
    for _ in 0..rng.below(8) {
        for &byte in &vocabulary[rng.below(vocabulary.len())] {
            data.push(byte);
            data.extend(std::iter::repeat_n(0x7f, char_size - 1));
        }
        data.extend(std::iter::repeat_n(0, char_size));
    }
    data
}

/// Naive reference for splitting: the `(start, end)` of every piece.
fn naive_split(kind: MergeKind, data: &[u8]) -> Vec<(u64, u64)> {
    let mut pieces = Vec::new();
    match kind {
        MergeKind::Strings { char_size } => {
            let unit = usize::from(char_size);
            let mut start = 0usize;
            for position in (0..data.len()).step_by(unit) {
                if data[position..position + unit].iter().all(|&b| b == 0) {
                    pieces.push((start as u64, (position + unit) as u64));
                    start = position + unit;
                }
            }
        }
        MergeKind::Fixed { entry_size } => {
            for position in (0..data.len() as u64).step_by(entry_size as usize) {
                pieces.push((position, position + entry_size));
            }
        }
    }
    pieces
}

/// Output of [`naive_merge`]: merged size, `(input offset, output offset)` of
/// each piece of each section (`None` if dead), and the distinct live pieces.
type NaiveMerge = (u64, Vec<Vec<(u64, Option<u64>)>>, BTreeMap<Vec<u8>, u64>);

/// Naive reference for merging without tail merging: pieces, first live
/// occurrence order, aligned offsets. `live(section, piece)` says whether a
/// piece of `datas[section]` is live.
fn naive_merge(
    group: &MergeGroup,
    datas: &[&[u8]],
    live: &dyn Fn(usize, usize) -> bool,
) -> NaiveMerge {
    let mut offsets: BTreeMap<Vec<u8>, u64> = BTreeMap::new();
    let mut size = 0u64;
    let mut per_section = Vec::new();
    for (section, data) in datas.iter().enumerate() {
        let mut pieces = Vec::new();
        for (piece, (start, end)) in naive_split(group.kind, data).into_iter().enumerate() {
            if !live(section, piece) {
                pieces.push((start, None));
                continue;
            }
            let bytes = data[start as usize..end as usize].to_vec();
            let offset = *offsets.entry(bytes).or_insert_with(|| {
                let aligned = size.div_ceil(group.alignment) * group.alignment;
                size = aligned + (end - start);
                aligned
            });
            pieces.push((start, Some(offset)));
        }
        per_section.push(pieces);
    }
    (size, per_section, offsets)
}

/// The merged contents of every group.
fn merged_images(merged: &MergedSections<'_, '_>, groups: usize) -> Vec<Vec<u8>> {
    (0..groups)
        .map(|g| {
            let mut out = vec![0xee; merged.group(g).unwrap().size() as usize];
            merged.write_group(g, &mut out).unwrap();
            out
        })
        .collect()
}

/// Asserts that two merge results agree on layout and on every offset.
fn assert_same_merge(
    a: &MergedSections<'_, '_>,
    b: &MergedSections<'_, '_>,
    sections: &[MergeSection<'_>],
    groups: usize,
) {
    assert_eq!(a.groups(), b.groups());
    assert_eq!(merged_images(a, groups), merged_images(b, groups));
    assert_eq!(a.num_pieces(), b.num_pieces());
    for (s, section) in sections.iter().enumerate() {
        for offset in 0..=section.data.len() as u64 {
            assert_eq!(a.output_offset(s, offset), b.output_offset(s, offset));
            assert_eq!(a.piece_at(s, offset), b.piece_at(s, offset));
        }
    }
}

/// Checks a merge result against [`naive_merge`] for every group.
/// `live(section, piece)` uses indices into `sections`.
fn check_against_reference(
    merged: &MergedSections<'_, '_>,
    groups: &[MergeGroup],
    sections: &[MergeSection<'_>],
    live: &dyn Fn(usize, usize) -> bool,
) {
    let images = merged_images(merged, groups.len());
    for (g, group) in groups.iter().enumerate() {
        let members: Vec<usize> = (0..sections.len())
            .filter(|&s| sections[s].group as usize == g)
            .collect();
        let datas: Vec<&[u8]> = members.iter().map(|&s| sections[s].data).collect();
        let member_live = |position: usize, piece: usize| live(members[position], piece);
        let (naive_size, naive_pieces, uniques) = naive_merge(group, &datas, &member_live);
        let image = &images[g];
        let size = merged.group(g).unwrap().size();
        let tail = group.tail_merge && matches!(group.kind, MergeKind::Strings { .. });
        if !tail {
            assert_eq!(size, naive_size);
        } else {
            assert!(size <= naive_size);
            if group.alignment == 1 {
                // Storage is exactly the strings that are not a suffix of
                // another distinct string.
                let owned: u64 = uniques
                    .keys()
                    .filter(|s| !uniques.keys().any(|o| o.len() > s.len() && o.ends_with(s)))
                    .map(|s| s.len() as u64)
                    .sum();
                assert_eq!(size, owned);
            }
        }
        for (member, pieces) in members.iter().zip(&naive_pieces) {
            let data = sections[*member].data;
            for (index, &(start, naive_out)) in pieces.iter().enumerate() {
                let end = pieces.get(index + 1).map_or(data.len() as u64, |p| p.0);
                let Some(naive_out) = naive_out else {
                    for inner in start..end {
                        assert_eq!(merged.output_offset(*member, inner), None);
                    }
                    assert_eq!(merged.piece_output_offset(*member, index as u32), None);
                    continue;
                };
                let out = merged.output_offset(*member, start).expect("mapped");
                if !tail {
                    assert_eq!(out, naive_out);
                }
                assert_eq!(out % group.alignment, 0);
                let bytes = &data[start as usize..end as usize];
                assert_eq!(&image[out as usize..out as usize + bytes.len()], bytes);
                assert_eq!(merged.piece_output_offset(*member, index as u32), Some(out));
                // Offsets inside the piece keep their position.
                for inner in start..end {
                    assert_eq!(
                        merged.output_offset(*member, inner),
                        Some(out + inner - start)
                    );
                    let found = merged.piece_at(*member, inner).unwrap();
                    assert_eq!(found.addend, inner - start);
                    assert_eq!(found.piece as usize, index);
                }
            }
        }
    }
}

fn random_merge_case(rng: &mut Rng) -> (Vec<MergeGroup>, Vec<(u32, Vec<u8>)>) {
    let count = 1 + rng.below(40);
    let vocabulary = words(rng, count);
    let groups: Vec<MergeGroup> = (0..1 + rng.below(3))
        .map(|_| {
            let kind = if rng.chance(70) {
                MergeKind::Strings {
                    char_size: [1, 2, 4][rng.below(3)],
                }
            } else {
                MergeKind::Fixed {
                    entry_size: [1, 2, 4, 8][rng.below(4)],
                }
            };
            MergeGroup {
                kind,
                alignment: 1 << rng.below(4),
                tail_merge: rng.chance(50),
            }
        })
        .collect();
    let sections = (0..rng.below(60))
        .map(|_| {
            let group = rng.below(groups.len());
            let data = match groups[group].kind {
                MergeKind::Strings { char_size } => {
                    string_section(rng, &vocabulary, usize::from(char_size))
                }
                MergeKind::Fixed { entry_size } => (0..rng.below(10) * entry_size as usize)
                    .map(|_| rng.below(2) as u8)
                    .collect(),
            };
            (group as u32, data)
        })
        .collect();
    (groups, sections)
}

fn to_sections(raw: &[(u32, Vec<u8>)]) -> Vec<MergeSection<'_>> {
    raw.iter()
        .map(|(group, data)| MergeSection {
            group: *group,
            data,
        })
        .collect()
}

/// Splits every section with its group's kind and alignment, independently,
/// on `threads` OS threads that take sections round-robin (each in reverse
/// order), as a backend does while parsing files in parallel.
fn split_on_threads<'a>(
    groups: &[MergeGroup],
    sections: &[MergeSection<'a>],
    threads: usize,
) -> Vec<SplitSection<'a>> {
    let mut indexed: Vec<(usize, SplitSection<'a>)> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|thread| {
                scope.spawn(move || {
                    (0..sections.len())
                        .rev()
                        .filter(|index| index % threads == thread)
                        .map(|index| {
                            let section = &sections[index];
                            let group = groups[section.group as usize];
                            let split = split_section(section.data, group.kind, group.alignment)
                                .expect("well-formed");
                            (index, split)
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|handle| handle.join().expect("split thread"))
            .collect()
    });
    indexed.sort_by_key(|&(index, _)| index);
    indexed.into_iter().map(|(_, split)| split).collect()
}

#[test]
fn merge_matches_reference_on_random_sections() {
    let mut rng = Rng(6);
    for _ in 0..150 {
        let (groups, raw) = random_merge_case(&mut rng);
        let sections = to_sections(&raw);
        let results: Vec<_> = POOLS
            .iter()
            .map(|&threads| {
                in_pool(threads, || {
                    merge_sections(&groups, &sections).expect("well-formed")
                })
            })
            .collect();
        for other in &results[1..] {
            assert_same_merge(&results[0], other, &sections, groups.len());
        }
        check_against_reference(&results[0], &groups, &sections, &|_, _| true);
    }
}

#[test]
fn merge_two_phases_match_wrapper_and_reference() {
    let mut rng = Rng(11);
    for case in 0..150 {
        let (groups, raw) = random_merge_case(&mut rng);
        let sections = to_sections(&raw);
        let splits = split_on_threads(&groups, &sections, 1 + case % 4);

        // Phase 1 alone maps offsets to pieces, at piece boundaries and in
        // the middle of pieces.
        for (split, section) in splits.iter().zip(&sections) {
            let naive = naive_split(groups[section.group as usize].kind, section.data);
            assert_eq!(split.num_pieces(), naive.len());
            assert_eq!(split.hashes().len(), naive.len());
            for (piece, &(start, end)) in naive.iter().enumerate() {
                let expect = |offset: u64| {
                    Some(PieceRef {
                        piece: piece as u32,
                        addend: offset - start,
                    })
                };
                assert_eq!(split.piece_start(piece), Some(start));
                assert_eq!(
                    split.piece_bytes(piece),
                    Some(&section.data[start as usize..end as usize])
                );
                let mid = start + (end - start) / 2;
                for offset in [start, mid, end - 1] {
                    assert_eq!(split.piece_at(offset), expect(offset));
                }
            }
            assert_eq!(split.piece_at(section.data.len() as u64), None);
            assert_eq!(split.piece_start(naive.len()), None);
            assert_eq!(split.piece_bytes(naive.len()), None);
        }

        let inputs: Vec<MergeInput<'_, '_>> = splits
            .iter()
            .zip(&sections)
            .map(|(split, section)| MergeInput {
                group: section.group,
                split,
            })
            .collect();
        let bases: Vec<usize> = splits
            .iter()
            .scan(0, |next, split| {
                let base = *next;
                *next += split.num_pieces();
                Some(base)
            })
            .collect();
        let total: usize = splits.iter().map(SplitSection::num_pieces).sum();
        let mut live = BitSet::new(total);
        for bit in 0..total {
            if rng.chance(70) {
                live.insert(bit);
            }
        }
        let is_live = |section: usize, piece: usize| live.get(bases[section] + piece);

        let wrapped = merge_sections(&groups, &sections).expect("well-formed");
        let results: Vec<_> = POOLS
            .iter()
            .map(|&threads| {
                in_pool(threads, || {
                    let all = merge_split_sections(&groups, &inputs, None).expect("consistent");
                    let partial =
                        merge_split_sections(&groups, &inputs, Some(&live)).expect("consistent");
                    (all, partial)
                })
            })
            .collect();
        for (all, partial) in &results {
            assert_same_merge(all, &wrapped, &sections, groups.len());
            assert_same_merge(partial, &results[0].1, &sections, groups.len());
            check_against_reference(all, &groups, &sections, &|_, _| true);
            check_against_reference(partial, &groups, &sections, &is_live);
            for (s, base) in bases.iter().enumerate() {
                assert_eq!(partial.first_piece(s), Some(*base));
            }
            assert_eq!(partial.first_piece(sections.len()), Some(total));
        }
    }
}

#[test]
fn merge_never_panics_on_garbage() {
    let mut rng = Rng(7);
    for _ in 0..500 {
        let groups: Vec<MergeGroup> = (0..1 + rng.below(2))
            .map(|_| MergeGroup {
                kind: if rng.chance(50) {
                    MergeKind::Strings {
                        char_size: rng.below(6) as u8,
                    }
                } else {
                    MergeKind::Fixed {
                        entry_size: rng.below(5) as u64,
                    }
                },
                alignment: rng.below(9) as u64,
                tail_merge: rng.chance(50),
            })
            .collect();
        let raw: Vec<(u32, Vec<u8>)> = (0..rng.below(10))
            .map(|_| {
                let data = (0..rng.below(20))
                    .map(|_| if rng.chance(60) { 0 } else { rng.next() as u8 })
                    .collect();
                (rng.below(groups.len() + 1) as u32, data)
            })
            .collect();
        let sections = to_sections(&raw);
        match merge_sections(&groups, &sections) {
            Ok(merged) => {
                for g in 0..groups.len() {
                    let mut out = vec![0; merged.group(g).unwrap().size() as usize];
                    merged.write_group(g, &mut out).unwrap();
                    assert!(merged.write_group(g, &mut []).is_err() || out.is_empty());
                }
                for (s, section) in sections.iter().enumerate() {
                    for offset in 0..=section.data.len() as u64 + 1 {
                        let _ = merged.output_offset(s, offset);
                    }
                }
            }
            Err(MergeError::Malformed { section, malformed }) => {
                assert!(section < sections.len());
                assert!(malformed.offset <= sections[section].data.len() as u64);
                // The same error is reported under any thread count.
                let again = in_pool(8, || merge_sections(&groups, &sections).err());
                assert_eq!(again, Some(MergeError::Malformed { section, malformed }));
            }
            Err(MergeError::Input(_)) => {}
        }

        // Phase 1 with arbitrary kinds and alignments, then phase 2 with
        // arbitrary (possibly mismatched) groups and liveness bitmaps.
        let splits: Vec<SplitSection<'_>> = sections
            .iter()
            .filter_map(|section| {
                let group = groups[rng.below(groups.len())];
                let alignment = rng.below(9) as u64;
                let result = split_section(section.data, group.kind, alignment);
                if let Ok(split) = &result {
                    for offset in 0..=section.data.len() as u64 {
                        if let Some(found) = split.piece_at(offset) {
                            let start = split.piece_start(found.piece as usize).unwrap();
                            assert_eq!(start + found.addend, offset);
                        }
                    }
                }
                result.ok()
            })
            .collect();
        let inputs: Vec<MergeInput<'_, '_>> = splits
            .iter()
            .map(|split| MergeInput {
                group: rng.below(groups.len() + 1) as u32,
                split,
            })
            .collect();
        let total: usize = splits.iter().map(SplitSection::num_pieces).sum();
        let mut live = BitSet::new(total + usize::from(rng.chance(20)));
        for bit in 0..live.len() {
            if rng.chance(50) {
                live.insert(bit);
            }
        }
        let bitmap = rng.chance(50).then_some(&live);
        if let Ok(merged) = merge_split_sections(&groups, &inputs, bitmap) {
            for g in 0..groups.len() {
                let mut out = vec![0; merged.group(g).unwrap().size() as usize];
                merged.write_group(g, &mut out).unwrap();
            }
            for (s, split) in splits.iter().enumerate() {
                for offset in 0..=split.data().len() as u64 + 1 {
                    let _ = merged.output_offset(s, offset);
                }
                let _ = merged.piece_output_offset(s, split.num_pieces() as u32);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Micro-benchmarks
// ---------------------------------------------------------------------------

fn time<R>(label: &str, f: impl FnOnce() -> R) -> R {
    let start = Instant::now();
    let result = f();
    println!("{label}: {:.1} ms", start.elapsed().as_secs_f64() * 1e3);
    result
}

#[test]
#[ignore = "micro-benchmark"]
fn bench_gc_million_sections() {
    let n = 1_000_000;
    let mut rng = Rng(8);
    let targets: Vec<[u32; 4]> = (0..n)
        .map(|_| std::array::from_fn(|_| rng.below(n) as u32))
        .collect();
    let roots: Vec<SectionId> = (0..100).map(|_| id(rng.below(n))).collect();
    let graph = time("gc: build graph (parallel)", || {
        SectionGraph::build_parallel(
            n,
            |section| 1 + usize::from(section.index().is_multiple_of(3)) * 3,
            |section, slot| {
                let wanted = slot.len();
                for (dest, &target) in slot.iter_mut().zip(&targets[section.index()]) {
                    *dest = SectionId::from_u32(target);
                }
                wanted
            },
            roots,
        )
        .expect("graph")
    });
    println!("gc: {} sections, {} edges", n, graph.num_edges());
    for threads in [1, 2, 8, rayon::current_num_threads()] {
        let live = in_pool(threads, || {
            time(&format!("gc: mark, {threads} threads"), || {
                collect_garbage(&graph)
            })
        });
        println!("gc: {} live", live.num_live());
    }
    let tree = time("gc: reference tree", || ReferenceTree::new(&graph));
    let _ = tree.chain(id(n - 1));
}

#[test]
#[ignore = "micro-benchmark"]
fn bench_icf_million_sections() {
    let n = 1_000_000;
    let mut rng = Rng(9);
    let bodies: Vec<Vec<u8>> = (0..5000)
        .map(|_| (0..64).map(|_| rng.below(4) as u8).collect())
        .collect();
    let choice: Vec<usize> = (0..n).map(|_| rng.below(bodies.len())).collect();
    let callees: Vec<[usize; 2]> = (0..n).map(|i| [rng.below(n), (i + 1) % n]).collect();
    let input = time("icf: build input (parallel)", || {
        let sections = choice
            .iter()
            .map(|&c| IcfSection {
                contents: &bodies[c],
                key: 0,
                foldable: true,
                address_significant: false,
            })
            .collect();
        let relocs = Csr::build_parallel(
            n,
            IcfReloc {
                offset: 0,
                kind: 0,
                addend: 0,
                target: IcfTarget::Value(0),
            },
            |section| usize::from(choice[section].is_multiple_of(2)) * 2,
            |section, slot| {
                for (position, dest) in slot.iter_mut().enumerate() {
                    *dest = IcfReloc {
                        offset: 8 * position as u64,
                        kind: 2,
                        addend: -4,
                        target: IcfTarget::Section {
                            section: id(callees[section][position] % 1000),
                            offset: 0,
                        },
                    };
                }
                slot.len()
            },
        )
        .expect("relocs");
        IcfInput::new(sections, relocs).expect("input")
    });
    for threads in [1, 8, rayon::current_num_threads()] {
        let result = in_pool(threads, || {
            time(&format!("icf: fold, {threads} threads"), || {
                fold_identical(&input, IcfMode::All)
            })
        });
        println!(
            "icf: {} folded in {} rounds",
            result.num_folded(),
            result.rounds()
        );
    }
}

#[test]
#[ignore = "micro-benchmark"]
fn bench_merge_million_sections() {
    let n = 1_000_000;
    let mut rng = Rng(10);
    let vocabulary: Vec<Vec<u8>> = (0..200_000)
        .map(|i| format!("symbol_name_{i}_{}", rng.below(1000)).into_bytes())
        .collect();
    let datas: Vec<Vec<u8>> = (0..n)
        .map(|_| {
            let mut data = Vec::new();
            for _ in 0..5 {
                data.extend_from_slice(&vocabulary[rng.below(vocabulary.len())]);
                data.push(0);
            }
            data
        })
        .collect();
    let sections: Vec<MergeSection<'_>> = datas
        .iter()
        .map(|data| MergeSection { group: 0, data })
        .collect();
    for tail_merge in [false, true] {
        let groups = [MergeGroup {
            kind: MergeKind::Strings { char_size: 1 },
            alignment: 1,
            tail_merge,
        }];
        for threads in [1, 8, rayon::current_num_threads()] {
            let merged = in_pool(threads, || {
                time(
                    &format!("merge: one-shot, tail_merge={tail_merge}, {threads} threads"),
                    || merge_sections(&groups, &sections).expect("merge"),
                )
            });
            println!(
                "merge: {} pieces -> {} bytes",
                merged.num_pieces(),
                merged.group(0).unwrap().size()
            );
            drop(merged);
            in_pool(threads, || {
                // Phase 1 as the backend runs it: one call per section from
                // parallel per-file parsing.
                let splits: Vec<SplitSection<'_>> = time(
                    &format!("merge:   phase 1 split, {threads} threads"),
                    || {
                        sections
                            .par_iter()
                            .map(|section| {
                                split_section(section.data, groups[0].kind, 1).expect("split")
                            })
                            .collect()
                    },
                );
                let inputs: Vec<MergeInput<'_, '_>> = splits
                    .iter()
                    .map(|split| MergeInput { group: 0, split })
                    .collect();
                let merged = time(
                    &format!("merge:   phase 2 dedup+layout, {threads} threads"),
                    || merge_split_sections(&groups, &inputs, None).expect("merge"),
                );
                assert_eq!(merged.num_pieces(), n * 5);
            });
        }
    }
}
