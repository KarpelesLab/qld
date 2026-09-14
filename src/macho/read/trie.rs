//! The export trie (`LC_DYLD_EXPORTS_TRIE`, or the export range of
//! `LC_DYLD_INFO_ONLY`).
//!
//! A node is a ULEB128 terminal size, the terminal information when that size
//! is non-zero, a child count byte, and for each child a NUL-terminated edge
//! label and a ULEB128 node offset. A symbol's name is the concatenation of
//! the labels on the path from the root to its terminal node.
//!
//! The decoder visits each node at most once, so a trie with cycles or
//! shared nodes (neither of which a valid trie has) cannot make it loop or
//! blow up.

use core::ops::Range;

use super::bytes::{Source, cstr, read_uleb, to_u64};
use super::consts::{
    EXPORT_SYMBOL_FLAGS_KIND_ABSOLUTE, EXPORT_SYMBOL_FLAGS_KIND_MASK,
    EXPORT_SYMBOL_FLAGS_KIND_THREAD_LOCAL, EXPORT_SYMBOL_FLAGS_REEXPORT,
    EXPORT_SYMBOL_FLAGS_STUB_AND_RESOLVER, EXPORT_SYMBOL_FLAGS_WEAK_DEFINITION,
};
use crate::error::Result;

/// What an export resolves to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExportTarget<'a> {
    /// Defined in this image at `address` (relative to the image base).
    Address(u64),
    /// Re-exported from another dylib.
    Reexport {
        /// Dependency ordinal (1-based, in load command order).
        ordinal: u64,
        /// The name in the other dylib; empty means the same name.
        name: &'a [u8],
    },
    /// A stub whose target is computed by a resolver function.
    StubAndResolver {
        /// Address of the stub.
        stub: u64,
        /// Address of the resolver function.
        resolver: u64,
    },
}

/// An exported symbol.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Export<'a> {
    /// The symbol name.
    pub name: Vec<u8>,
    /// Raw `EXPORT_SYMBOL_FLAGS_*`.
    pub flags: u64,
    /// Where the symbol is.
    pub target: ExportTarget<'a>,
}

impl Export<'_> {
    /// Whether the export is a weak definition.
    #[must_use]
    pub fn is_weak(&self) -> bool {
        self.flags & EXPORT_SYMBOL_FLAGS_WEAK_DEFINITION != 0
    }

    /// Whether the export is thread-local.
    #[must_use]
    pub fn is_thread_local(&self) -> bool {
        self.flags & EXPORT_SYMBOL_FLAGS_KIND_MASK == EXPORT_SYMBOL_FLAGS_KIND_THREAD_LOCAL
    }

    /// Whether the export is absolute.
    #[must_use]
    pub fn is_absolute(&self) -> bool {
        self.flags & EXPORT_SYMBOL_FLAGS_KIND_MASK == EXPORT_SYMBOL_FLAGS_KIND_ABSOLUTE
    }

    /// Whether the export is a re-export.
    #[must_use]
    pub fn is_reexport(&self) -> bool {
        self.flags & EXPORT_SYMBOL_FLAGS_REEXPORT != 0
    }
}

/// A node waiting to be visited.
#[derive(Clone, Debug)]
struct Pending {
    node: usize,
    /// Length of the parent's name.
    prefix: usize,
    /// The edge label, as a range of the trie.
    label: Range<usize>,
}

/// Iterator over the exports of a trie, in depth-first edge order.
///
/// The first malformed node ends iteration with an error.
#[derive(Clone, Debug)]
pub struct ExportTrieIter<'a> {
    data: &'a [u8],
    file_offset: u64,
    source: Source<'a>,
    stack: Vec<Pending>,
    name: Vec<u8>,
    visited: Vec<u8>,
}

type VisitResult<'a> = core::result::Result<Option<Export<'a>>, (usize, &'static str)>;

impl<'a> ExportTrieIter<'a> {
    /// Iterates over the trie in `data`, found at `file_offset`. An empty
    /// trie has no exports.
    #[must_use]
    pub fn new(data: &'a [u8], file_offset: u64, source: Source<'a>) -> Self {
        let stack = if data.is_empty() {
            Vec::new()
        } else {
            vec![Pending {
                node: 0,
                prefix: 0,
                label: 0..0,
            }]
        };
        Self {
            data,
            file_offset,
            source,
            stack,
            name: Vec::new(),
            visited: vec![0; data.len().div_ceil(8)],
        }
    }

    /// Visits one node: pushes its children and returns its export, if it is
    /// terminal.
    fn visit(&mut self, pending: Pending) -> VisitResult<'a> {
        let data = self.data;
        let node = pending.node;
        let slot = self
            .visited
            .get_mut(node >> 3)
            .ok_or((node, "node offset out of range"))?;
        let mask = 1u8 << (node & 7);
        if *slot & mask != 0 {
            return Err((node, "node reached twice"));
        }
        *slot |= mask;
        self.name.truncate(pending.prefix);
        self.name
            .extend_from_slice(data.get(pending.label).unwrap_or(&[]));

        let mut pos = node;
        let terminal_size = read_uleb(data, &mut pos).ok_or((node, "truncated node"))?;
        let children_at = usize::try_from(terminal_size)
            .ok()
            .and_then(|size| pos.checked_add(size))
            .filter(|&at| at < data.len())
            .ok_or((node, "terminal size"))?;
        let mut export = None;
        if terminal_size != 0 {
            let info = data.get(pos..children_at).unwrap_or(&[]);
            let mut p = 0usize;
            let flags = read_uleb(info, &mut p).ok_or((pos, "truncated flags"))?;
            let target = if flags & EXPORT_SYMBOL_FLAGS_REEXPORT != 0 {
                let ordinal = read_uleb(info, &mut p).ok_or((pos, "truncated ordinal"))?;
                let imported = info
                    .get(p..)
                    .and_then(cstr)
                    .ok_or((pos, "unterminated re-export name"))?;
                ExportTarget::Reexport {
                    ordinal,
                    name: imported,
                }
            } else {
                let address = read_uleb(info, &mut p).ok_or((pos, "truncated address"))?;
                if flags & EXPORT_SYMBOL_FLAGS_STUB_AND_RESOLVER != 0 {
                    let resolver = read_uleb(info, &mut p).ok_or((pos, "truncated resolver"))?;
                    ExportTarget::StubAndResolver {
                        stub: address,
                        resolver,
                    }
                } else {
                    ExportTarget::Address(address)
                }
            };
            export = Some(Export {
                name: self.name.clone(),
                flags,
                target,
            });
        }

        let mut pos = children_at;
        let count = *data.get(pos).ok_or((pos, "truncated child count"))?;
        pos = pos.saturating_add(1);
        let first_child = self.stack.len();
        let prefix = self.name.len();
        for _ in 0..count {
            let label = data
                .get(pos..)
                .and_then(cstr)
                .ok_or((pos, "unterminated edge label"))?;
            let label_range = pos..pos.saturating_add(label.len());
            pos = label_range.end.saturating_add(1);
            let child = read_uleb(data, &mut pos).ok_or((pos, "truncated child offset"))?;
            let child = usize::try_from(child)
                .ok()
                .filter(|&c| c < data.len())
                .ok_or((pos, "child offset out of range"))?;
            // No valid name is longer than the trie that spells it.
            if prefix.saturating_add(label.len()) > data.len() {
                return Err((pos, "name longer than the trie"));
            }
            self.stack.push(Pending {
                node: child,
                prefix,
                label: label_range,
            });
        }
        // Children are popped from the end: reverse them to visit in edge
        // order.
        if let Some(children) = self.stack.get_mut(first_child..) {
            children.reverse();
        }
        Ok(export)
    }
}

impl<'a> Iterator for ExportTrieIter<'a> {
    type Item = Result<Export<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        while let Some(pending) = self.stack.pop() {
            match self.visit(pending) {
                Ok(Some(export)) => return Some(Ok(export)),
                Ok(None) => {}
                Err((offset, what)) => {
                    self.stack.clear();
                    return Some(Err(self.source.malformed(
                        self.file_offset.saturating_add(to_u64(offset)),
                        format!("export trie ({what})"),
                    )));
                }
            }
        }
        None
    }
}

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)]
pub(crate) mod tests {
    use super::*;
    use std::path::Path;

    fn uleb(mut v: u64, out: &mut Vec<u8>) {
        loop {
            let byte = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(byte);
                return;
            }
            out.push(byte | 0x80);
        }
    }

    /// A simple trie builder for tests: every export hangs off the root with
    /// its full name as the edge label, except names sharing a first byte
    /// with the previous one go through a shared node.
    pub(crate) fn build(exports: &[(&[u8], u64, &[u8])]) -> Vec<u8> {
        // Layout: root, then one terminal node per export.
        // Terminal info: flags, then address or (ordinal, name).
        let mut nodes: Vec<Vec<u8>> = Vec::new();
        for &(_, flags, payload) in exports {
            let mut info = Vec::new();
            uleb(flags, &mut info);
            info.extend_from_slice(payload);
            let mut node = Vec::new();
            uleb(info.len() as u64, &mut node);
            node.extend(info);
            node.push(0);
            nodes.push(node);
        }
        // Root size with 2-byte child offsets.
        let mut root_size = 2;
        for &(name, _, _) in exports {
            root_size += name.len() + 1 + 2;
        }
        let mut root = vec![0, exports.len() as u8];
        let mut offset = root_size;
        for (i, &(name, _, _)) in exports.iter().enumerate() {
            root.extend_from_slice(name);
            root.push(0);
            root.push((offset as u8 & 0x7f) | 0x80);
            root.push((offset >> 7) as u8);
            offset += nodes[i].len();
        }
        assert_eq!(root.len(), root_size);
        let mut trie = root;
        for node in nodes {
            trie.extend(node);
        }
        trie
    }

    #[test]
    fn decodes_exports() {
        let src = Source::new(Path::new("lib.dylib"));
        let mut addr = Vec::new();
        uleb(0x3f40, &mut addr);
        let mut reexport = Vec::new();
        uleb(1, &mut reexport);
        reexport.extend_from_slice(b"_other\0");
        let mut resolver = Vec::new();
        uleb(0x10, &mut resolver);
        uleb(0x20, &mut resolver);
        let trie = build(&[
            (b"_foo", 0, &addr),
            (b"_bar", EXPORT_SYMBOL_FLAGS_REEXPORT, &reexport),
            (b"_res", EXPORT_SYMBOL_FLAGS_STUB_AND_RESOLVER, &resolver),
            (b"_weak", EXPORT_SYMBOL_FLAGS_WEAK_DEFINITION, &addr),
        ]);
        let exports: Vec<_> = ExportTrieIter::new(&trie, 0, src)
            .collect::<Result<_>>()
            .unwrap();
        assert_eq!(exports.len(), 4);
        assert_eq!(exports[0].name, b"_foo");
        assert_eq!(exports[0].target, ExportTarget::Address(0x3f40));
        assert_eq!(
            exports[1].target,
            ExportTarget::Reexport {
                ordinal: 1,
                name: b"_other"
            }
        );
        assert_eq!(
            exports[2].target,
            ExportTarget::StubAndResolver {
                stub: 0x10,
                resolver: 0x20
            }
        );
        assert!(exports[3].is_weak());

        // A node that points back at the root.
        let cycle = [0u8, 1, b'a', 0, 0];
        let result: Vec<_> = ExportTrieIter::new(&cycle, 0, src).collect();
        assert!(result.last().unwrap().is_err());
        assert!(ExportTrieIter::new(&[], 0, src).next().is_none());
    }
}
