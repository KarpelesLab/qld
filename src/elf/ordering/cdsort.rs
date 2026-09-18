//! Cache-directed sort (CDSort), lld's default call graph sort: LLVM's
//! `codelayout::computeCacheDirectedLayout` with its default configuration.
//!
//! Each function starts as a chain of its own. Chains adjacent in the call
//! graph are merged greedily, best gain first, while merging improves a
//! score that rewards calls over short distances (`count * distance^-0.25`)
//! and dense chains that fit in few cache pages; chains are finally sorted
//! by density. The arithmetic, tie-breaks and iteration orders follow
//! LLVM's implementation step for step (it works on doubles, so the order
//! of additions matters), so that the result is the order lld produces.

use std::collections::BTreeMap;

/// `CDSortConfig` defaults.
const CACHE_ENTRIES: f64 = 16.0;
const CACHE_SIZE: f64 = 2048.0;
const MAX_CHAIN_SIZE: usize = 128;
const DISTANCE_POWER: f64 = 0.25;
const FREQUENCY_SCALE: f64 = 0.25;
/// Epsilon for comparing doubles.
const EPS: f64 = 1e-8;

/// A call between two functions.
#[derive(Clone, Copy, Debug)]
pub struct Call {
    /// Caller index.
    pub from: usize,
    /// Callee index.
    pub to: usize,
    /// Execution count.
    pub count: u64,
    /// Offset of the call in the caller.
    pub offset: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MergeType {
    XY,
    YX,
}

#[derive(Clone, Copy)]
struct Gain {
    score: f64,
    merge: MergeType,
}

impl Default for Gain {
    fn default() -> Self {
        Self {
            score: -1.0,
            merge: MergeType::XY,
        }
    }
}

struct Node {
    size: u64,
    count: u64,
    chain: usize,
    estimated: u64,
    out_jumps: Vec<usize>,
    in_jumps: Vec<usize>,
}

struct Jump {
    source: usize,
    target: usize,
    count: u64,
    offset: u64,
}

struct Chain {
    id: u64,
    count: f64,
    size: u64,
    nodes: Vec<usize>,
    /// Adjacent chains and the edge to each, in insertion order.
    edges: Vec<(usize, usize)>,
}

impl Chain {
    fn density(&self) -> f64 {
        self.count / self.size as f64
    }

    fn edge_to(&self, other: usize) -> Option<usize> {
        self.edges
            .iter()
            .find(|&&(chain, _)| chain == other)
            .map(|&(_, edge)| edge)
    }

    fn remove_edge(&mut self, other: usize) {
        if let Some(at) = self.edges.iter().position(|&(chain, _)| chain == other) {
            self.edges.remove(at);
        }
    }
}

struct Edge {
    src: usize,
    dst: usize,
    jumps: Vec<usize>,
    gain: Gain,
}

impl Edge {
    fn change_endpoint(&mut self, from: usize, to: usize) {
        if self.src == from {
            self.src = to;
        }
        if self.dst == from {
            self.dst = to;
        }
    }
}

/// Queue key: (-gain, source chain ID, destination chain ID), as LLVM's
/// `std::set` orders edges.
#[derive(Clone, Copy, PartialEq)]
struct Key(f64, u64, u64);

impl Eq for Key {}

impl PartialOrd for Key {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Key {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.0
            .total_cmp(&other.0)
            .then(self.1.cmp(&other.1))
            .then(self.2.cmp(&other.2))
    }
}

struct Sorter {
    nodes: Vec<Node>,
    jumps: Vec<Jump>,
    chains: Vec<Chain>,
    edges: Vec<Edge>,
    total_samples: u64,
    total_size: u64,
}

/// Orders the functions of sizes `sizes` and execution counts `counts`
/// (both indexed by function) given the `calls` between them. Returns
/// every function index once.
#[must_use]
pub fn sort(sizes: &[u64], counts: &[u64], calls: &[Call]) -> Vec<usize> {
    let mut sorter = Sorter::new(sizes, counts, calls);
    sorter.merge_chain_pairs();
    sorter.concat_chains()
}

impl Sorter {
    fn new(sizes: &[u64], counts: &[u64], calls: &[Call]) -> Self {
        let mut nodes = Vec::with_capacity(sizes.len());
        let mut total_samples = 0u64;
        let mut total_size = 0u64;
        for (index, &size) in sizes.iter().enumerate() {
            let size = size.max(1);
            let count = counts.get(index).copied().unwrap_or(0);
            total_samples = total_samples.wrapping_add(count);
            if count > 0 {
                total_size = total_size.wrapping_add(size);
            }
            nodes.push(Node {
                size,
                count,
                chain: index,
                estimated: 0,
                out_jumps: Vec::new(),
                in_jumps: Vec::new(),
            });
        }
        let mut jumps = Vec::with_capacity(calls.len());
        for call in calls {
            if call.from == call.to || call.count == 0 {
                continue;
            }
            if call.from >= nodes.len() || call.to >= nodes.len() {
                continue;
            }
            let jump = jumps.len();
            jumps.push(Jump {
                source: call.from,
                target: call.to,
                count: call.count,
                offset: call.offset,
            });
            if let Some(node) = nodes.get_mut(call.to) {
                node.in_jumps.push(jump);
                node.count = node.count.max(call.count);
            }
            if let Some(node) = nodes.get_mut(call.from) {
                node.out_jumps.push(jump);
                node.count = node.count.max(call.count);
            }
        }
        let mut chains = Vec::with_capacity(nodes.len());
        for (index, node) in nodes.iter_mut().enumerate() {
            let sum = |list: &[usize]| {
                list.iter()
                    .filter_map(|&j| jumps.get(j))
                    .map(|j| j.count)
                    .fold(0u64, u64::wrapping_add)
            };
            node.count = node
                .count
                .max(sum(&node.in_jumps))
                .max(sum(&node.out_jumps));
            chains.push(Chain {
                id: index as u64,
                count: node.count as f64,
                size: node.size,
                nodes: vec![index],
                edges: Vec::new(),
            });
        }
        let mut edges: Vec<Edge> = Vec::with_capacity(jumps.len());
        for pred in 0..nodes.len() {
            for &jump in &nodes[pred].out_jumps {
                let succ = jumps[jump].target;
                let (pred_chain, succ_chain) = (nodes[pred].chain, nodes[succ].chain);
                if let Some(edge) = chains[pred_chain].edge_to(succ_chain) {
                    edges[edge].jumps.push(jump);
                    continue;
                }
                let edge = edges.len();
                edges.push(Edge {
                    src: pred_chain,
                    dst: succ_chain,
                    jumps: vec![jump],
                    gain: Gain::default(),
                });
                chains[pred_chain].edges.push((succ_chain, edge));
                chains[succ_chain].edges.push((pred_chain, edge));
            }
        }
        Self {
            nodes,
            jumps,
            chains,
            edges,
            total_samples,
            total_size,
        }
    }

    fn key(&self, edge: usize) -> Key {
        let e = &self.edges[edge];
        Key(-e.gain.score, self.chains[e.src].id, self.chains[e.dst].id)
    }

    fn merge_chain_pairs(&mut self) {
        let mut queue: BTreeMap<Key, usize> = BTreeMap::new();
        for node in 0..self.nodes.len() {
            if self.nodes[node].count == 0 {
                continue;
            }
            let chain = self.nodes[node].chain;
            let adjacent: Vec<usize> = self.chains[chain].edges.iter().map(|&(_, e)| e).collect();
            for edge in adjacent {
                if self.edges[edge].src == self.edges[edge].dst {
                    continue;
                }
                // Already processed.
                if self.edges[edge].gain.score != -1.0 {
                    continue;
                }
                let gain = self.best_merge_gain(edge);
                self.edges[edge].gain = gain;
                if gain.score > EPS {
                    queue.entry(self.key(edge)).or_insert(edge);
                }
            }
        }
        while let Some((_, best)) = queue.pop_first() {
            let (src, dst) = (self.edges[best].src, self.edges[best].dst);
            for chain in [src, dst] {
                let adjacent: Vec<usize> =
                    self.chains[chain].edges.iter().map(|&(_, e)| e).collect();
                for edge in adjacent {
                    let key = self.key(edge);
                    queue.remove(&key);
                }
            }
            let gain = self.edges[best].gain;
            self.merge_chains(src, dst, gain.merge);
            let adjacent: Vec<usize> = self.chains[src].edges.iter().map(|&(_, e)| e).collect();
            for edge in adjacent {
                let (a, b) = (self.edges[edge].src, self.edges[edge].dst);
                if a == b {
                    continue;
                }
                if self.chains[a]
                    .nodes
                    .len()
                    .saturating_add(self.chains[b].nodes.len())
                    > MAX_CHAIN_SIZE
                {
                    continue;
                }
                let gain = self.best_merge_gain(edge);
                self.edges[edge].gain = gain;
                if gain.score > EPS {
                    queue.entry(self.key(edge)).or_insert(edge);
                }
            }
        }
    }

    fn best_merge_gain(&mut self, edge: usize) -> Gain {
        let (src, dst) = (self.edges[edge].src, self.edges[edge].dst);
        let mut gain = Gain::default();
        for merge in [MergeType::XY, MergeType::YX] {
            let new = self.merge_gain(src, dst, edge, merge);
            if (gain.score - new.score).abs() < EPS {
                let (src_id, dst_id) = (self.chains[src].id, self.chains[dst].id);
                if (merge == MergeType::XY && src_id < dst_id)
                    || (merge == MergeType::YX && src_id > dst_id)
                {
                    gain = new;
                }
            } else if new.score > gain.score + EPS {
                gain = new;
            }
        }
        gain
    }

    fn merge_gain(&mut self, pred: usize, succ: usize, edge: usize, merge: MergeType) -> Gain {
        let freq_gain = self.freq_locality_gain(pred, succ);
        let order: [usize; 2] = match merge {
            MergeType::XY => [pred, succ],
            MergeType::YX => [succ, pred],
        };
        let mut address = 0u64;
        for chain in order {
            for index in 0..self.chains[chain].nodes.len() {
                let node = self.chains[chain].nodes[index];
                self.nodes[node].estimated = address;
                address = address.wrapping_add(self.nodes[node].size);
            }
        }
        let mut current = 0.0f64;
        let mut new = 0.0f64;
        for &jump in &self.edges[edge].jumps {
            let j = &self.jumps[jump];
            let src = self.nodes[j.source].estimated.wrapping_add(j.offset);
            let dst = self.nodes[j.target].estimated;
            new += dist_score(src, dst, j.count);
            current += dist_score(0, self.total_size, j.count);
        }
        let dist_gain = new - current;
        let mut score = dist_gain + FREQUENCY_SCALE * freq_gain;
        if score >= 0.0 {
            score /= self.chains[pred].size.min(self.chains[succ].size) as f64;
        }
        Gain { score, merge }
    }

    fn freq_locality_gain(&self, pred: usize, succ: usize) -> f64 {
        let total = self.total_samples as f64;
        let miss = |density: f64| {
            let page = density * CACHE_SIZE;
            if page >= total {
                return 0.0;
            }
            let p = page / total;
            (1.0 - p).powf(CACHE_ENTRIES)
        };
        let (p, s) = (&self.chains[pred], &self.chains[succ]);
        let current = p.count * miss(p.density()) + s.count * miss(s.density());
        let merged_count = p.count + s.count;
        let merged_size = (p.size.wrapping_add(s.size)) as f64;
        let merged_density = merged_count / merged_size;
        let new = merged_count * miss(merged_density);
        current - new
    }

    fn merge_chains(&mut self, into: usize, from: usize, merge: MergeType) {
        let (x, y) = (&self.chains[into].nodes, &self.chains[from].nodes);
        let merged: Vec<usize> = match merge {
            MergeType::XY => x.iter().chain(y).copied().collect(),
            MergeType::YX => y.iter().chain(x).copied().collect(),
        };
        let from_count = self.chains[from].count;
        let from_size = self.chains[from].size;
        {
            let chain = &mut self.chains[into];
            chain.count += from_count;
            chain.size = chain.size.wrapping_add(from_size);
            chain.id = merged.first().copied().unwrap_or(0) as u64;
            chain.nodes = merged;
        }
        let nodes = self.chains[into].nodes.clone();
        for node in nodes {
            self.nodes[node].chain = into;
        }
        // Merge the edges (LLVM's `ChainT::mergeEdges`).
        let other_edges = self.chains[from].edges.clone();
        for (dst_chain, dst_edge) in other_edges {
            let target = if dst_chain == from { into } else { dst_chain };
            match self.chains[into].edge_to(target) {
                None => {
                    self.edges[dst_edge].change_endpoint(from, into);
                    self.chains[into].edges.push((target, dst_edge));
                    if dst_chain != into && dst_chain != from {
                        self.chains[dst_chain].edges.push((into, dst_edge));
                    }
                }
                Some(current) => {
                    let moved = core::mem::take(&mut self.edges[dst_edge].jumps);
                    self.edges[current].jumps.extend(moved);
                }
            }
            if dst_chain != from {
                self.chains[dst_chain].remove_edge(from);
            }
        }
        let chain = &mut self.chains[from];
        chain.nodes = Vec::new();
        chain.edges = Vec::new();
    }

    fn concat_chains(&self) -> Vec<usize> {
        let mut sorted: Vec<(f64, u64, usize)> = Vec::new();
        for (index, chain) in self.chains.iter().enumerate() {
            if chain.nodes.is_empty() {
                continue;
            }
            let mut size = 0.0f64;
            let mut count = 0.0f64;
            for &node in &chain.nodes {
                size += self.nodes[node].size as f64;
                count += self.nodes[node].count as f64;
            }
            sorted.push((count / size, chain.id, index));
        }
        sorted.sort_by(|a, b| (-a.0).total_cmp(&-b.0).then(a.1.cmp(&b.1)));
        sorted
            .iter()
            .flat_map(|&(_, _, chain)| self.chains[chain].nodes.iter().copied())
            .collect()
    }
}

fn dist_score(src: u64, dst: u64, count: u64) -> f64 {
    let dist = src.abs_diff(dst);
    let d = if dist == 0 { 0.1 } else { dist as f64 };
    count as f64 * d.powf(-DISTANCE_POWER)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hot_pairs_end_up_adjacent() {
        // 0 calls 2 a lot, 1 calls 3 a little.
        let sizes = [16, 16, 16, 16];
        let counts = [0, 0, 1000, 10];
        let calls = [
            Call {
                from: 0,
                to: 2,
                count: 1000,
                offset: 8,
            },
            Call {
                from: 1,
                to: 3,
                count: 10,
                offset: 8,
            },
        ];
        let order = sort(&sizes, &counts, &calls);
        assert_eq!(order.len(), 4);
        let at = |n: usize| order.iter().position(|&x| x == n).unwrap();
        assert_eq!(at(0).abs_diff(at(2)), 1);
        assert_eq!(at(1).abs_diff(at(3)), 1);
        assert!(at(0) < at(1));
    }
}
