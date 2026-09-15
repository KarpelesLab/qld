//! Garbage collection of unreferenced sections (`--gc-sections`,
//! `-dead_strip`).
//!
//! [`collect_garbage`] marks every section reachable from the roots of a
//! [`SectionGraph`] and returns the [`LiveSet`]. The mark runs on the current
//! rayon pool: each task pops sections from its own local stack, claims
//! targets through an atomic mark bit (exactly one task wins each section),
//! and hands half of its stack to a new task when the stack grows. The set of
//! marked sections is the reachable set, so the result does not depend on
//! scheduling.
//!
//! For diagnostics, [`LiveSet::removed`] lists the dead sections in input
//! order (`--print-gc-sections`), and [`why_live`] / [`ReferenceTree`] give a
//! shortest reference chain from a root (`--why-live`). Those run
//! sequentially and only when asked for.

use rayon::Scope;

use super::bitset::{AtomicBitSet, BitSet};
use super::graph::SectionGraph;
use crate::ids::SectionId;

/// A task hands off half its stack once the stack is longer than this.
const SPLIT_THRESHOLD: usize = 256;

/// The same for [`mark_reachable`], whose steps read relocations and so
/// cost far more than a graph's: splitting early keeps long chains of
/// references from running on one thread.
const LAZY_SPLIT_THRESHOLD: usize = 32;

/// Marks live sections. Usable for more than one round of marking, for
/// formats where marking discovers new roots (for example, lld's
/// `-z start-stop-gc` handling, where a live reference to `__start_foo` makes
/// the `foo` sections roots).
#[derive(Debug)]
pub struct GcMarker<'g> {
    graph: &'g SectionGraph,
    bits: AtomicBitSet,
}

impl<'g> GcMarker<'g> {
    /// Creates a marker with nothing marked.
    #[must_use]
    pub fn new(graph: &'g SectionGraph) -> Self {
        Self {
            graph,
            bits: AtomicBitSet::new(graph.num_sections()),
        }
    }

    /// Marks the graph's roots and everything reachable from them.
    pub fn mark_roots(&self) {
        self.mark_from(self.graph.roots());
    }

    /// Marks `roots` and everything reachable from them. Sections already
    /// marked are not visited again. Out-of-range roots are ignored.
    pub fn mark_from(&self, roots: &[SectionId]) {
        let graph = self.graph;
        let bits = &self.bits;
        let parallel = rayon::current_num_threads() > 1;
        if !parallel {
            let stack: Vec<SectionId> = roots
                .iter()
                .copied()
                .filter(|root| bits.set(root.index()))
                .collect();
            mark_sequential(graph, bits, stack);
            return;
        }
        rayon::scope(|scope| {
            for chunk in roots.chunks(SPLIT_THRESHOLD) {
                let stack: Vec<SectionId> = chunk
                    .iter()
                    .copied()
                    .filter(|root| bits.set(root.index()))
                    .collect();
                if !stack.is_empty() {
                    scope.spawn(move |scope| mark_task(scope, graph, bits, stack));
                }
            }
        });
    }

    /// Returns whether `section` is marked so far.
    #[must_use]
    pub fn is_marked(&self, section: SectionId) -> bool {
        self.bits.get(section.index())
    }

    /// Finishes marking and returns the live set.
    #[must_use]
    pub fn finish(self) -> LiveSet {
        LiveSet {
            bits: self.bits.into_bitset(),
        }
    }
}

fn mark_sequential(graph: &SectionGraph, bits: &AtomicBitSet, mut stack: Vec<SectionId>) {
    while let Some(section) = stack.pop() {
        for &target in graph.edges(section) {
            if bits.set(target.index()) {
                stack.push(target);
            }
        }
    }
}

fn mark_task<'s>(
    scope: &Scope<'s>,
    graph: &'s SectionGraph,
    bits: &'s AtomicBitSet,
    mut stack: Vec<SectionId>,
) {
    while let Some(section) = stack.pop() {
        for &target in graph.edges(section) {
            if bits.set(target.index()) {
                stack.push(target);
            }
        }
        if stack.len() > SPLIT_THRESHOLD {
            let half = stack.split_off(stack.len() / 2);
            scope.spawn(move |scope| mark_task(scope, graph, bits, half));
        }
    }
}

/// Marks `roots` and every section reachable from them, with the edges
/// given by `edges(section, targets)` (which appends the section's targets
/// to `targets`), and returns the live set of `sections` sections.
///
/// The same mark as [`collect_garbage`], without building a
/// [`SectionGraph`]: only the sections that turn out reachable have their
/// edges enumerated, once. Out-of-range roots and targets are ignored. The
/// result is the reachable set, whatever the scheduling.
pub fn mark_reachable<E>(sections: usize, roots: &[SectionId], edges: &E) -> LiveSet
where
    E: Fn(SectionId, &mut Vec<SectionId>) + Sync,
{
    let marks = AtomicBitSet::new(sections);
    let bits = &marks;
    if rayon::current_num_threads() <= 1 {
        let mut stack: Vec<SectionId> = roots
            .iter()
            .copied()
            .filter(|root| bits.set(root.index()))
            .collect();
        let mut targets = Vec::new();
        while let Some(section) = stack.pop() {
            edges(section, &mut targets);
            stack.extend(targets.drain(..).filter(|target| bits.set(target.index())));
        }
    } else {
        rayon::scope(|scope| {
            for chunk in roots.chunks(LAZY_SPLIT_THRESHOLD) {
                let stack: Vec<SectionId> = chunk
                    .iter()
                    .copied()
                    .filter(|root| bits.set(root.index()))
                    .collect();
                if !stack.is_empty() {
                    scope.spawn(move |scope| mark_lazy_task(scope, edges, bits, stack));
                }
            }
        });
    }
    LiveSet {
        bits: marks.into_bitset(),
    }
}

fn mark_lazy_task<'s, E>(
    scope: &Scope<'s>,
    edges: &'s E,
    bits: &'s AtomicBitSet,
    mut stack: Vec<SectionId>,
) where
    E: Fn(SectionId, &mut Vec<SectionId>) + Sync,
{
    let mut targets = Vec::new();
    while let Some(section) = stack.pop() {
        edges(section, &mut targets);
        stack.extend(targets.drain(..).filter(|target| bits.set(target.index())));
        if stack.len() > LAZY_SPLIT_THRESHOLD {
            let half = stack.split_off(stack.len() / 2);
            scope.spawn(move |scope| mark_lazy_task(scope, edges, bits, half));
        }
    }
}

/// Runs a full mark from the graph's roots and returns the live sections.
#[must_use]
pub fn collect_garbage(graph: &SectionGraph) -> LiveSet {
    let marker = GcMarker::new(graph);
    marker.mark_roots();
    marker.finish()
}

/// The outcome of garbage collection: which sections are live.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveSet {
    bits: BitSet,
}

impl LiveSet {
    /// Returns whether `section` is live. Out-of-range sections are dead.
    #[inline]
    #[must_use]
    pub fn is_live(&self, section: SectionId) -> bool {
        self.bits.get(section.index())
    }

    /// Number of sections in the graph.
    #[must_use]
    pub fn num_sections(&self) -> usize {
        self.bits.len()
    }

    /// Number of live sections.
    #[must_use]
    pub fn num_live(&self) -> usize {
        self.bits.count_ones()
    }

    /// Live sections, in input order.
    pub fn live(&self) -> impl Iterator<Item = SectionId> + '_ {
        self.bits.ones().map(SectionId::new)
    }

    /// Removed sections, in input order: the list `--print-gc-sections`
    /// prints (the backend filters out sections it never reports, such as
    /// empty or synthetic ones).
    #[must_use]
    pub fn removed(&self) -> Vec<SectionId> {
        self.bits.zeros().map(SectionId::new).collect()
    }

    /// The live bits, indexed by section.
    #[must_use]
    pub fn bits(&self) -> &BitSet {
        &self.bits
    }
}

/// Sentinel parent for sections the search has not reached.
const UNREACHED: u32 = u32::MAX;

/// Shortest reference chains from the roots to every reachable section.
///
/// Built by a sequential breadth-first search that visits roots in the order
/// the graph lists them and edges in the order each section lists them, so
/// the chain reported for a section is deterministic. Use it when many
/// `--why-live` queries are answered at once; [`why_live`] answers one.
#[derive(Clone, Debug)]
pub struct ReferenceTree {
    /// `parent[s]` is the section that first reached `s`, `s` itself for a
    /// root, or [`UNREACHED`].
    parent: Vec<u32>,
}

impl ReferenceTree {
    /// Searches the whole graph.
    #[must_use]
    pub fn new(graph: &SectionGraph) -> Self {
        Self::search(graph, None)
    }

    /// Breadth-first search from the roots, stopping early once `target` is
    /// reached.
    fn search(graph: &SectionGraph, target: Option<SectionId>) -> Self {
        let mut parent = vec![UNREACHED; graph.num_sections()];
        let mut queue: Vec<SectionId> = Vec::new();
        for &root in graph.roots() {
            if let Some(slot) = parent.get_mut(root.index())
                && *slot == UNREACHED
            {
                *slot = root.as_u32();
                queue.push(root);
            }
        }
        let mut head = 0;
        while let Some(&section) = queue.get(head) {
            if target.is_some_and(|target| parent.get(target.index()) != Some(&UNREACHED)) {
                break;
            }
            head += 1;
            for &next in graph.edges(section) {
                if let Some(slot) = parent.get_mut(next.index())
                    && *slot == UNREACHED
                {
                    *slot = section.as_u32();
                    queue.push(next);
                }
            }
        }
        Self { parent }
    }

    /// Returns the chain `[root, ..., section]` along which `section` is
    /// reachable, or `None` if it is not reachable (it is dead).
    #[must_use]
    pub fn chain(&self, section: SectionId) -> Option<Vec<SectionId>> {
        let mut current = section.as_u32();
        if self
            .parent
            .get(section.index())
            .is_none_or(|&p| p == UNREACHED)
        {
            return None;
        }
        let mut chain = vec![section];
        // A parent chain in a BFS tree is acyclic and at most n long; the
        // bound only guards against a corrupted table.
        for _ in 0..self.parent.len() {
            let parent = *self.parent.get(current as usize)?;
            if parent == current {
                chain.reverse();
                return Some(chain);
            }
            chain.push(SectionId::from_u32(parent));
            current = parent;
        }
        None
    }
}

/// Returns a shortest reference chain `[root, ..., section]` explaining why
/// `section` is live, or `None` if it is dead. For `--why-live`.
///
/// Runs a sequential breadth-first search that stops once `section` is
/// reached. The chain is deterministic: among equally short chains, the one
/// through earlier roots and earlier edges wins.
#[must_use]
pub fn why_live(graph: &SectionGraph, section: SectionId) -> Option<Vec<SectionId>> {
    ReferenceTree::search(graph, Some(section)).chain(section)
}

#[cfg(test)]
mod tests {
    use super::super::graph::GraphBuilder;
    use super::*;

    fn id(index: usize) -> SectionId {
        SectionId::new(index)
    }

    #[test]
    fn marks_reachable_including_cycles() {
        // 0 -> 1 -> 2 -> 1 (cycle), 3 -> 4 unreachable island, 5 root alone.
        let mut builder = GraphBuilder::new(6);
        builder.add_edge(id(0), id(1));
        builder.add_edge(id(1), id(2));
        builder.add_edge(id(2), id(1));
        builder.add_edge(id(3), id(4));
        builder.add_edge(id(4), id(3));
        builder.add_root(id(0));
        builder.add_root(id(5));
        builder.add_root(id(0));
        let graph = builder.build().unwrap();
        let live = collect_garbage(&graph);
        assert_eq!(
            live.live().collect::<Vec<_>>(),
            vec![id(0), id(1), id(2), id(5)]
        );
        assert_eq!(live.removed(), vec![id(3), id(4)]);
        assert_eq!(live.num_live(), 4);
        assert_eq!(why_live(&graph, id(2)), Some(vec![id(0), id(1), id(2)]));
        assert_eq!(why_live(&graph, id(5)), Some(vec![id(5)]));
        assert_eq!(why_live(&graph, id(3)), None);
        assert_eq!(why_live(&graph, id(99)), None);
    }

    #[test]
    fn lazy_mark_matches_graph_mark() {
        // A pseudo-random graph with long chains, cycles and unreachable
        // parts, marked on pools of several sizes.
        let sections = 20_000usize;
        let mut builder = GraphBuilder::new(sections);
        let mut adjacency: Vec<Vec<SectionId>> = vec![Vec::new(); sections];
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for (from, targets) in adjacency.iter_mut().enumerate() {
            if from + 1 < sections && next() % 4 != 0 {
                targets.push(id(from + 1));
            }
            for _ in 0..next() % 3 {
                let to = (next() % sections as u64) as usize;
                if next() % 2 == 0 {
                    targets.push(id(to));
                }
            }
        }
        for (from, targets) in adjacency.iter().enumerate() {
            for &to in targets {
                builder.add_edge(id(from), to);
            }
        }
        let roots: Vec<SectionId> = (0..sections).step_by(997).map(id).collect();
        for &root in &roots {
            builder.add_root(root);
        }
        let graph = builder.build().unwrap();
        let expected = collect_garbage(&graph);
        for threads in [1, 3, 8] {
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .unwrap();
            let lazy = pool.install(|| {
                mark_reachable(sections, &roots, &|section, targets| {
                    targets.extend_from_slice(&adjacency[section.index()]);
                })
            });
            assert_eq!(lazy, expected, "{threads} threads");
        }
    }

    #[test]
    fn marker_supports_extra_rounds() {
        let mut builder = GraphBuilder::new(3);
        builder.add_edge(id(1), id(2));
        builder.add_root(id(0));
        let graph = builder.build().unwrap();
        let marker = GcMarker::new(&graph);
        marker.mark_roots();
        assert!(!marker.is_marked(id(1)));
        marker.mark_from(&[id(1), id(42)]);
        assert_eq!(marker.finish().removed(), Vec::<SectionId>::new());
    }

    #[test]
    fn why_live_prefers_shortest_then_earliest() {
        // Diamond: 0 -> 1 -> 3, 0 -> 2 -> 3, plus long path 0 -> 4 -> 5 -> 3.
        let mut builder = GraphBuilder::new(6);
        builder.add_edge(id(0), id(4));
        builder.add_edge(id(4), id(5));
        builder.add_edge(id(5), id(3));
        builder.add_edge(id(0), id(2));
        builder.add_edge(id(0), id(1));
        builder.add_edge(id(1), id(3));
        builder.add_edge(id(2), id(3));
        builder.add_root(id(0));
        let graph = builder.build().unwrap();
        assert_eq!(why_live(&graph, id(3)), Some(vec![id(0), id(2), id(3)]));
        let tree = ReferenceTree::new(&graph);
        assert_eq!(tree.chain(id(5)), Some(vec![id(0), id(4), id(5)]));
    }
}
