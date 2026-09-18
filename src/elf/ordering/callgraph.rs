//! `--call-graph-profile-sort`: section order from a call graph, as lld
//! builds it.
//!
//! The graph's edges are (caller section, callee section, weight). They
//! come from `--call-graph-ordering-file` (lines of `caller callee count`,
//! symbols looked up by name as lld does: the last object file that has a
//! symbol of that name wins) or else from the `SHT_LLVM_CALL_GRAPH_PROFILE`
//! sections of the input objects, whose entries are weights and whose
//! `R_*_NONE` relocations name the two symbols of each edge. Weights of the
//! same pair add up, and pairs keep the order they were first seen in.
//! Edges between sections of different output sections are ignored.
//!
//! Two algorithms order the sections:
//!
//! - `hfsort` is the C3 heuristic of Ottoni and Maher ("Optimizing Function
//!   Placement for Large-Scale Data-Center Applications", CGO 2017), as in
//!   lld's `CallGraphSort`: in decreasing density order, a section joins
//!   the cluster of its most frequent caller unless the edge is unlikely
//!   (under 10% of its weight), the cluster would pass 1 MiB, or the merged
//!   density would drop below an eighth; clusters are then laid out by
//!   decreasing density. `--print-symbol-order` lists the symbols in the
//!   resulting order.
//! - `cdsort` is LLVM's cache-directed sort ([`super::cdsort`]).

use std::path::Path;

use rayon::prelude::*;

use crate::args::options::CallGraphSort;
use crate::diag::{Diagnostic, DiagnosticSink};
use crate::elf::read::Relocations;
use crate::elf::read::consts::{SHF_EXCLUDE, STT_SECTION};
use crate::error::{Error, Result};
use crate::ids::SectionId;

use super::super::place::Placement;
use super::super::refs::{Def, Refs};
use super::super::sections::NONE;
use super::cdsort;

/// `SHT_LLVM_CALL_GRAPH_PROFILE`.
pub const SHT_LLVM_CALL_GRAPH_PROFILE: u32 = 0x6fff_4c09;

/// Call graph edges between sections, in first-seen order.
#[derive(Debug, Default)]
pub struct Profile {
    edges: Vec<((SectionId, SectionId), u64)>,
    index: hashbrown::HashMap<(SectionId, SectionId), usize, foldhash::fast::FixedState>,
}

impl Profile {
    fn add(&mut self, from: SectionId, to: SectionId, weight: u64) {
        match self.index.get(&(from, to)) {
            Some(&at) => {
                if let Some(edge) = self.edges.get_mut(at) {
                    edge.1 = edge.1.wrapping_add(weight);
                }
            }
            None => {
                self.index.insert((from, to), self.edges.len());
                self.edges.push(((from, to), weight));
            }
        }
    }

    /// Whether the graph has no edge.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.edges.is_empty()
    }
}

/// The section a symbol of `file` orders, as lld's `Defined::section`: the
/// section it is defined in (the one ICF folded it into), live or not.
fn defined_section<F: crate::elf::read::ElfFormat>(
    refs: &Refs<'_, '_, F>,
    file: usize,
    symbol: usize,
) -> Option<SectionId> {
    let target = refs.target(file, symbol)?;
    let Def::Section { file, section, .. } = target.def else {
        return None;
    };
    let id = refs.sections.id(file, section)?;
    Some(refs.sections.resolve(id).unwrap_or(id))
}

/// Reads the call graph of the input objects' `SHT_LLVM_CALL_GRAPH_PROFILE`
/// sections.
///
/// # Errors
///
/// Returns [`Error::Malformed`] for unreadable sections, and when the
/// relocations do not match the weights.
pub fn from_objects<F: crate::elf::read::ElfFormat>(
    refs: &Refs<'_, '_, F>,
    diagnostics: &dyn DiagnosticSink,
) -> Result<Profile> {
    type Edges = Vec<(SectionId, SectionId, u64)>;
    let per_file: Vec<Result<(Edges, bool)>> = refs
        .files
        .par_iter()
        .enumerate()
        .map(|(file, input)| {
            let mut edges = Vec::new();
            let Some(object) = &input.object else {
                return Ok((edges, false));
            };
            if !refs.resolution.is_live(crate::ids::FileId::new(file)) {
                return Ok((edges, false));
            }
            // The last profile section of the object, as lld keeps it.
            let Some(section) = object.sections.iter().rev().find(|s| {
                s.header.sh_type == SHT_LLVM_CALL_GRAPH_PROFILE
                    && s.header.sh_flags & SHF_EXCLUDE != 0
            }) else {
                return Ok((edges, false));
            };
            let data = object.section_data(section)?;
            let weights: Vec<u64> = data
                .as_chunks::<8>()
                .0
                .iter()
                .map(|w| u64::from_le_bytes(*w))
                .collect();
            let mut symbols: Vec<u32> = Vec::new();
            if section.relocs != 0 {
                let header = object.elf.section_header(section.relocs)?;
                if let Some(rel) = object.elf.relocation_section(section.relocs, &header)? {
                    match rel.relocations {
                        Relocations::Rela(list) => symbols.extend(list.iter().map(|r| r.symbol)),
                        Relocations::Rel(list) => symbols.extend(list.iter().map(|r| r.symbol)),
                    }
                }
            }
            if symbols.is_empty() {
                return Ok((edges, true));
            }
            if symbols.len() != weights.len().saturating_mul(2) {
                return Err(object.malformed(
                    section.header.sh_offset,
                    "call graph profile (number of relocations doesn't match weights)",
                ));
            }
            for (&[from, to], &weight) in symbols.as_chunks::<2>().0.iter().zip(&weights) {
                let from = defined_section(refs, file, from as usize);
                let to = defined_section(refs, file, to as usize);
                if let (Some(from), Some(to)) = (from, to) {
                    edges.push((from, to, weight));
                }
            }
            Ok((edges, false))
        })
        .collect();
    let mut profile = Profile::default();
    for result in per_file {
        let (edges, missing_relocations) = result?;
        if missing_relocations {
            diagnostics.emit(Diagnostic::warning(
                "SHT_LLVM_CALL_GRAPH_PROFILE exists, but relocation section doesn't",
            ));
        }
        for (from, to, weight) in edges {
            profile.add(from, to, weight);
        }
    }
    Ok(profile)
}

/// Reads `--call-graph-ordering-file`.
///
/// # Errors
///
/// Returns I/O errors, and [`Error::Option`] for a line that is not
/// `caller callee count`.
pub fn from_file<F: crate::elf::read::ElfFormat>(
    refs: &Refs<'_, '_, F>,
    path: &Path,
    warn: bool,
    ignore_undefined: bool,
    diagnostics: &dyn DiagnosticSink,
) -> Result<Profile> {
    let text = std::fs::read(path).map_err(|e| Error::io(path, e))?;
    // Every symbol of every object by name; later files win (lld's map).
    let mut names: hashbrown::HashMap<&[u8], (usize, usize), foldhash::fast::FixedState> =
        hashbrown::HashMap::with_hasher(foldhash::fast::FixedState::default());
    for (file, input) in refs.files.iter().enumerate() {
        let Some(object) = &input.object else {
            continue;
        };
        if !refs.resolution.is_live(crate::ids::FileId::new(file)) {
            continue;
        }
        let symbols = object.elf.symbols();
        for index in 1..symbols.len() {
            let Some(raw) = symbols.get_raw(index) else {
                continue;
            };
            if let Ok(name) = symbols.name(index, &raw) {
                names.insert(name, (file, index));
            }
        }
    }
    let mut profile = Profile::default();
    let find = |name: &[u8]| -> Option<SectionId> {
        let Some(&(file, index)) = names.get(name) else {
            if warn {
                diagnostics.emit(Diagnostic::warning(format!(
                    "{}: no such symbol: {}",
                    path.display(),
                    String::from_utf8_lossy(name)
                )));
            }
            return None;
        };
        if warn
            && let Some((display, why)) = super::unorderable(refs, file, index, ignore_undefined)
        {
            diagnostics.emit(Diagnostic::warning(format!(
                "{display}: unable to order {why} symbol: {}",
                String::from_utf8_lossy(name)
            )));
        }
        defined_section(refs, file, index)
    };
    for line in text.split(|&b| b == b'\n') {
        let line = line.trim_ascii();
        if line.is_empty() || line.first() == Some(&b'#') {
            continue;
        }
        let fields: Vec<&[u8]> = line.split(|&b| b == b' ').collect();
        let count = fields
            .get(2)
            .and_then(|f| std::str::from_utf8(f).ok())
            .and_then(|f| f.parse::<u64>().ok());
        let (3, Some(count)) = (fields.len(), count) else {
            return Err(Error::Option(format!("{}: parse error", path.display())));
        };
        let (Some(from), Some(to)) = (fields.first(), fields.get(1)) else {
            continue;
        };
        if let Some(from) = find(from)
            && let Some(to) = find(to)
        {
            profile.add(from, to, count);
        }
    }
    Ok(profile)
}

/// The size lld's `getSize` gives an input section.
fn section_size<F: crate::elf::read::ElfFormat>(refs: &Refs<'_, '_, F>, id: SectionId) -> u64 {
    refs.sections
        .locate(id)
        .and_then(|(file, index)| {
            refs.files
                .get(file)?
                .object
                .as_ref()?
                .section(index)
                .map(|s| s.header.sh_size)
        })
        .unwrap_or(0)
}

/// Output section of `id` for the same-output-section test (`NONE` when
/// it has none: lld then compares null pointers, which are equal).
fn output_of(placement: &Placement<'_>, id: SectionId) -> u32 {
    placement.output_of(id).unwrap_or(NONE)
}

/// The sections of the graph in the order `algorithm` gives, and the
/// priority of the first one.
#[must_use]
pub fn order<F: crate::elf::read::ElfFormat>(
    refs: &Refs<'_, '_, F>,
    placement: &Placement<'_>,
    profile: &Profile,
    algorithm: CallGraphSort,
) -> (Vec<SectionId>, i64) {
    match algorithm {
        CallGraphSort::None => (Vec::new(), 0),
        CallGraphSort::Hfsort => hfsort(refs, placement, profile),
        CallGraphSort::Cdsort => cache_directed(refs, placement, profile),
    }
}

/// Maps sections to dense node numbers in first-seen order.
#[derive(Default)]
struct Nodes {
    sections: Vec<SectionId>,
    index: hashbrown::HashMap<SectionId, usize, foldhash::fast::FixedState>,
}

impl Nodes {
    /// The node of `id`, and whether it is new.
    fn get_or_create(&mut self, id: SectionId) -> (usize, bool) {
        if let Some(&node) = self.index.get(&id) {
            return (node, false);
        }
        let node = self.sections.len();
        self.index.insert(id, node);
        self.sections.push(id);
        (node, true)
    }
}

/// lld's `MAX_DENSITY_DEGRADATION`.
const MAX_DENSITY_DEGRADATION: f64 = 8.0;
/// lld's `MAX_CLUSTER_SIZE`.
const MAX_CLUSTER_SIZE: u64 = 1024 * 1024;

#[derive(Clone, Copy)]
struct Cluster {
    next: usize,
    prev: usize,
    size: u64,
    weight: u64,
    initial_weight: u64,
    best_pred: Option<(usize, u64)>,
}

impl Cluster {
    fn density(&self) -> f64 {
        if self.size == 0 {
            return 0.0;
        }
        self.weight as f64 / self.size as f64
    }
}

fn hfsort<F: crate::elf::read::ElfFormat>(
    refs: &Refs<'_, '_, F>,
    placement: &Placement<'_>,
    profile: &Profile,
) -> (Vec<SectionId>, i64) {
    let mut nodes = Nodes::default();
    let mut clusters: Vec<Cluster> = Vec::new();
    let mut node = |id: SectionId, clusters: &mut Vec<Cluster>| {
        let (node, new) = nodes.get_or_create(id);
        if new {
            clusters.push(Cluster {
                next: node,
                prev: node,
                size: section_size(refs, id),
                weight: 0,
                initial_weight: 0,
                best_pred: None,
            });
        }
        node
    };
    for &((from, to), weight) in &profile.edges {
        if output_of(placement, from) != output_of(placement, to) {
            continue;
        }
        let from = node(from, &mut clusters);
        let to = node(to, &mut clusters);
        let Some(cluster) = clusters.get_mut(to) else {
            continue;
        };
        cluster.weight = cluster.weight.wrapping_add(weight);
        if from == to {
            continue;
        }
        if cluster.best_pred.is_none_or(|(_, best)| best < weight) {
            cluster.best_pred = Some((from, weight));
        }
    }
    let sections = nodes.sections;
    for cluster in &mut clusters {
        cluster.initial_weight = cluster.weight;
    }

    let count = clusters.len();
    let mut leaders: Vec<usize> = (0..count).collect();
    let mut sorted: Vec<usize> = (0..count).collect();
    let density = |clusters: &[Cluster], c: usize| clusters.get(c).map_or(0.0, Cluster::density);
    sorted.sort_by(|&a, &b| density(&clusters, b).total_cmp(&density(&clusters, a)));
    for &l in &sorted {
        let Some(&c) = clusters.get(l) else {
            continue;
        };
        let Some((pred, weight)) = c.best_pred else {
            continue;
        };
        if weight.saturating_mul(10) <= c.initial_weight {
            continue;
        }
        let pred_leader = leader(&mut leaders, pred);
        if pred_leader == l {
            continue;
        }
        let Some(&p) = clusters.get(pred_leader) else {
            continue;
        };
        if c.size.saturating_add(p.size) > MAX_CLUSTER_SIZE {
            continue;
        }
        // lld's `isNewDensityBad`.
        let merged = p.weight.wrapping_add(c.weight) as f64 / p.size.wrapping_add(c.size) as f64;
        if merged < p.density() / MAX_DENSITY_DEGRADATION {
            continue;
        }
        if let Some(slot) = leaders.get_mut(l) {
            *slot = pred_leader;
        }
        merge_clusters(&mut clusters, pred_leader, l);
    }

    let mut sorted: Vec<usize> = (0..count)
        .filter(|&c| clusters.get(c).is_some_and(|c| c.size > 0))
        .collect();
    sorted.sort_by(|&a, &b| density(&clusters, b).total_cmp(&density(&clusters, a)));
    let mut order = Vec::with_capacity(count);
    for &leader in &sorted {
        let mut i = leader;
        loop {
            if let Some(&id) = sections.get(i) {
                order.push(id);
            }
            i = clusters.get(i).map_or(leader, |c| c.next);
            if i == leader || order.len() > count {
                break;
            }
        }
    }
    let first = i64::try_from(count).map_or(i64::MIN, |c| c.saturating_neg());
    (order, first)
}

/// Union-find with path halving.
fn leader(leaders: &mut [usize], mut v: usize) -> usize {
    loop {
        let Some(&parent) = leaders.get(v) else {
            return v;
        };
        if parent == v {
            return v;
        }
        let grand = leaders.get(parent).copied().unwrap_or(parent);
        if let Some(slot) = leaders.get_mut(v) {
            *slot = grand;
        }
        v = grand;
    }
}

/// lld's `mergeClusters`: appends the circular list of `from` to `into`.
fn merge_clusters(clusters: &mut [Cluster], into: usize, from: usize) {
    let (Some(&a), Some(&b)) = (clusters.get(into), clusters.get(from)) else {
        return;
    };
    let (tail1, tail2) = (a.prev, b.prev);
    if let Some(c) = clusters.get_mut(into) {
        c.prev = tail2;
        c.size = c.size.wrapping_add(b.size);
        c.weight = c.weight.wrapping_add(b.weight);
    }
    if let Some(c) = clusters.get_mut(tail2) {
        c.next = into;
    }
    if let Some(c) = clusters.get_mut(from) {
        c.prev = tail1;
        c.size = 0;
        c.weight = 0;
    }
    if let Some(c) = clusters.get_mut(tail1) {
        c.next = from;
    }
}

fn cache_directed<F: crate::elf::read::ElfFormat>(
    refs: &Refs<'_, '_, F>,
    placement: &Placement<'_>,
    profile: &Profile,
) -> (Vec<SectionId>, i64) {
    let mut nodes = Nodes::default();
    let mut sizes: Vec<u64> = Vec::new();
    let mut counts: Vec<u64> = Vec::new();
    let mut calls: Vec<cdsort::Call> = Vec::new();
    for &((from, to), weight) in &profile.edges {
        if output_of(placement, from) != output_of(placement, to) || weight == 0 {
            continue;
        }
        let mut node = |id: SectionId| {
            let (node, new) = nodes.get_or_create(id);
            if new {
                sizes.push(section_size(refs, id));
                counts.push(0);
            }
            node
        };
        let from = node(from);
        let to = node(to);
        if from == to {
            continue;
        }
        let offset = sizes.get(from).map_or(0, |s| s.saturating_add(1) / 2);
        calls.push(cdsort::Call {
            from,
            to,
            count: weight,
            offset,
        });
        if let Some(count) = counts.get_mut(to) {
            *count = count.wrapping_add(weight);
        }
    }
    let sorted = cdsort::sort(&sizes, &counts, &calls);
    let first = i64::try_from(sorted.len()).map_or(i64::MIN, |c| c.saturating_neg());
    let order = sorted
        .into_iter()
        .filter_map(|n| nodes.sections.get(n).copied())
        .collect();
    (order, first)
}

/// Writes `--print-symbol-order`: for each section in `order`, the names of
/// the symbols of its file defined in it (section symbols aside), as lld
/// does for `hfsort`.
///
/// # Errors
///
/// Returns I/O errors.
pub fn print_symbol_order<F: crate::elf::read::ElfFormat>(
    refs: &Refs<'_, '_, F>,
    order: &[SectionId],
    path: &Path,
) -> Result<()> {
    let mut out = Vec::new();
    for &id in order {
        let Some((file, _)) = refs.sections.locate(id) else {
            continue;
        };
        let Some(object) = refs.files.get(file).and_then(|f| f.object.as_ref()) else {
            continue;
        };
        let symbols = object.elf.symbols();
        for index in 1..symbols.len() {
            let Some(raw) = symbols.get_raw(index) else {
                continue;
            };
            if raw.kind() == STT_SECTION {
                continue;
            }
            if defined_section(refs, file, index) != Some(id) {
                continue;
            }
            // A global defined elsewhere does not count: lld compares the
            // symbol's own section.
            if index >= object.first_global
                && refs
                    .global_id(file, index)
                    .map(|g| refs.symbols.definition(g).file.index())
                    != Some(file)
            {
                continue;
            }
            if let Ok(name) = symbols.name(index, &raw) {
                out.extend_from_slice(name);
                out.push(b'\n');
            }
        }
    }
    std::fs::write(path, out).map_err(|e| Error::io(path, e))
}
