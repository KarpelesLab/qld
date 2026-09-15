//! Building the export trie (`LC_DYLD_EXPORTS_TRIE`).
//!
//! The format is described in [`crate::macho::read::trie`]. Construction
//! follows the usual approach: a radix tree over the sorted names, nodes
//! numbered in depth-first order, then node offsets recomputed until the
//! ULEB128 sizes of the child offsets stop changing.

#![deny(clippy::arithmetic_side_effects)]

use super::buf::{push_uleb, to_u64, uleb_size};
use crate::macho::read::consts::EXPORT_SYMBOL_FLAGS_REEXPORT;

/// One exported symbol.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExportEntry {
    /// Symbol name.
    pub name: Vec<u8>,
    /// `EXPORT_SYMBOL_FLAGS_*`.
    pub flags: u64,
    /// Address relative to the image base (for re-exports: the ordinal).
    pub address: u64,
}

#[derive(Default)]
struct Node {
    terminal: Option<Vec<u8>>,
    edges: Vec<(Vec<u8>, usize)>,
    offset: usize,
}

/// Builds the trie for `entries`. Duplicate names keep their first entry in
/// name order; the result does not depend on the order of `entries`.
#[must_use]
pub fn build(entries: &[ExportEntry]) -> Vec<u8> {
    if entries.is_empty() {
        return Vec::new();
    }
    let mut sorted: Vec<&ExportEntry> = entries.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    sorted.dedup_by(|a, b| a.name == b.name);

    let mut nodes: Vec<Node> = vec![Node::default()];
    build_node(&sorted, 0, 0, &mut nodes);

    loop {
        let mut offset = 0usize;
        let mut changed = false;
        for index in 0..nodes.len() {
            let size = node_size(&nodes, index);
            if let Some(node) = nodes.get_mut(index)
                && node.offset != offset
            {
                node.offset = offset;
                changed = true;
            }
            offset = offset.saturating_add(size);
        }
        if !changed {
            break;
        }
    }

    let mut out = Vec::new();
    for index in 0..nodes.len() {
        write_node(&nodes, index, &mut out);
    }
    out
}

fn terminal_bytes(entry: &ExportEntry) -> Vec<u8> {
    let mut info = Vec::new();
    push_uleb(&mut info, entry.flags);
    push_uleb(&mut info, entry.address);
    if entry.flags & EXPORT_SYMBOL_FLAGS_REEXPORT != 0 {
        info.push(0);
    }
    info
}

/// Fills node `index` from `group`, whose names share their first `prefix`
/// bytes. Children are pushed in depth-first order.
fn build_node(group: &[&ExportEntry], prefix: usize, index: usize, nodes: &mut Vec<Node>) {
    let mut rest = group;
    if let Some(first) = rest.first()
        && first.name.len() == prefix
    {
        if let Some(node) = nodes.get_mut(index) {
            node.terminal = Some(terminal_bytes(first));
        }
        rest = rest.get(1..).unwrap_or(&[]);
    }
    let mut start = 0usize;
    while let Some(head) = rest.get(start) {
        let byte = head.name.get(prefix).copied();
        let mut end = start.saturating_add(1);
        while rest
            .get(end)
            .is_some_and(|entry| entry.name.get(prefix).copied() == byte)
        {
            end = end.saturating_add(1);
        }
        let run = rest.get(start..end).unwrap_or(&[]);
        // Sorted names: the first and last of the run bound the prefix all
        // of them share.
        let last = run.last().map_or(&head.name, |e| &e.name);
        let mut common = prefix.saturating_add(1);
        while common < head.name.len()
            && common < last.len()
            && head.name.get(common) == last.get(common)
        {
            common = common.saturating_add(1);
        }
        let label = head.name.get(prefix..common).unwrap_or(&[]).to_vec();
        let child = nodes.len();
        nodes.push(Node::default());
        if let Some(node) = nodes.get_mut(index) {
            node.edges.push((label, child));
        }
        build_node(run, common, child, nodes);
        start = end;
    }
}

fn node_size(nodes: &[Node], index: usize) -> usize {
    let Some(node) = nodes.get(index) else {
        return 0;
    };
    let mut size = match &node.terminal {
        Some(info) => uleb_size(to_u64(info.len())).saturating_add(info.len()),
        None => 1,
    };
    size = size.saturating_add(1);
    for (label, child) in &node.edges {
        let offset = nodes.get(*child).map_or(0, |n| n.offset);
        size = size
            .saturating_add(label.len())
            .saturating_add(1)
            .saturating_add(uleb_size(to_u64(offset)));
    }
    size
}

fn write_node(nodes: &[Node], index: usize, out: &mut Vec<u8>) {
    let Some(node) = nodes.get(index) else {
        return;
    };
    match &node.terminal {
        Some(info) => {
            push_uleb(out, to_u64(info.len()));
            out.extend_from_slice(info);
        }
        None => out.push(0),
    }
    out.push(u8::try_from(node.edges.len()).unwrap_or(u8::MAX));
    for (label, child) in &node.edges {
        out.extend_from_slice(label);
        out.push(0);
        push_uleb(out, to_u64(nodes.get(*child).map_or(0, |n| n.offset)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::macho::read::bytes::Source;
    use crate::macho::read::trie::{ExportTarget, ExportTrieIter};
    use std::path::Path;

    #[test]
    fn round_trips_through_the_reader() {
        let names = [
            "_main",
            "__mh_execute_header",
            "_foo",
            "_foobar",
            "_fo",
            "_bar",
            "_a",
            "_ab",
            "_abc",
        ];
        let mut entries: Vec<ExportEntry> = names
            .iter()
            .enumerate()
            .map(|(i, n)| ExportEntry {
                name: n.as_bytes().to_vec(),
                flags: (i % 2) as u64 * 4,
                address: 0x1000 * (i as u64 + 1) + 0x7_0000,
            })
            .collect();
        for i in 0..300u64 {
            entries.push(ExportEntry {
                name: format!("_sym{i:04}").into_bytes(),
                flags: 0,
                address: i * 0x40_0000,
            });
        }
        let trie = build(&entries);
        let source = Source::new(Path::new("t"));
        let mut decoded: Vec<(Vec<u8>, u64, u64)> = ExportTrieIter::new(&trie, 0, source)
            .map(|e| {
                let e = e.unwrap();
                let ExportTarget::Address(address) = e.target else {
                    panic!()
                };
                (e.name, e.flags, address)
            })
            .collect();
        decoded.sort();
        let mut expected: Vec<(Vec<u8>, u64, u64)> = entries
            .iter()
            .map(|e| (e.name.clone(), e.flags, e.address))
            .collect();
        expected.sort();
        assert_eq!(decoded, expected);
        let mut shuffled = entries.clone();
        shuffled.reverse();
        assert_eq!(build(&shuffled), trie);
    }
}
