//! The section reachability graph that garbage collection walks.
//!
//! Nodes are input sections, numbered densely `0..n` by [`SectionId`]. The
//! backend numbers sections in input order (command-line position, then
//! position in the file); every deterministic tie-break in the passes relies
//! on that.
//!
//! An edge `a -> b` means "if `a` is live, `b` is live". The backend derives
//! edges from:
//!
//! - **relocations**: the section holding the relocation points to the
//!   section defining the target symbol;
//! - **`SHF_LINK_ORDER`**: the section a dependent section links to points to
//!   the dependent section (a live `.text.foo` keeps its `.ARM.exidx.text.foo`
//!   or `__patchable_function_entries` entry);
//! - **COMDAT groups**: every member must keep the whole group. Rather than
//!   `k²` edges, link the members in a cycle — see
//!   [`GraphBuilder::keep_together`];
//! - anything else format-specific (`__start_`/`__stop_` references, Mach-O
//!   `S_ATTR_LIVE_SUPPORT`, ...).
//!
//! Roots are the sections that are live unconditionally: those holding the
//! entry point, `-u` symbols, exported symbols, `KEEP` sections, init/fini
//! arrays, `SHF_GNU_RETAIN`, non-alloc sections and so on.
//!
//! Edges are stored as a [`Csr`], which the relocation scan can fill in
//! parallel with [`SectionGraph::build_parallel`].

use rayon::prelude::*;

use super::csr::{Csr, CsrBuilder, InputError};
use crate::ids::SectionId;

/// A section graph: per-section outgoing edges plus a root set.
///
/// All edge targets and roots are checked to be in range at construction.
#[derive(Clone, Debug)]
pub struct SectionGraph {
    edges: Csr<SectionId>,
    roots: Vec<SectionId>,
}

impl SectionGraph {
    /// Wraps an edge table (row `i` = edges out of section `i`) and roots.
    ///
    /// # Errors
    ///
    /// Returns [`InputError::OutOfRange`] if an edge target or root is not a
    /// valid section.
    pub fn new(edges: Csr<SectionId>, roots: Vec<SectionId>) -> Result<Self, InputError> {
        let len = edges.rows();
        let out_of_range = |id: &SectionId| id.index() >= len;
        if let Some(bad) = edges.values().par_iter().find_first(|id| out_of_range(id)) {
            return Err(InputError::OutOfRange {
                what: "edge target section",
                index: u64::from(bad.as_u32()),
                len,
            });
        }
        if let Some(bad) = roots.iter().find(|id| out_of_range(id)) {
            return Err(InputError::OutOfRange {
                what: "root section",
                index: u64::from(bad.as_u32()),
                len,
            });
        }
        Ok(Self { edges, roots })
    }

    /// Builds the graph in parallel with [`Csr::build_parallel`]:
    /// `count(section)` bounds the number of edges out of a section, and
    /// `fill(section, slot)` writes them and returns how many it wrote.
    ///
    /// # Errors
    ///
    /// As [`SectionGraph::new`], plus [`InputError::TooLarge`] on overflow.
    pub fn build_parallel<C, F>(
        sections: usize,
        count: C,
        fill: F,
        roots: Vec<SectionId>,
    ) -> Result<Self, InputError>
    where
        C: Fn(SectionId) -> usize + Sync,
        F: Fn(SectionId, &mut [SectionId]) -> usize + Sync,
    {
        if u32::try_from(sections).is_err() {
            return Err(InputError::TooLarge("section count"));
        }
        let edges = Csr::build_parallel(
            sections,
            SectionId::from_u32(0),
            |row| count(SectionId::new(row)),
            |row, slot| fill(SectionId::new(row), slot),
        )?;
        Self::new(edges, roots)
    }

    /// Number of sections (nodes).
    #[inline]
    #[must_use]
    pub fn num_sections(&self) -> usize {
        self.edges.rows()
    }

    /// Total number of edges.
    #[inline]
    #[must_use]
    pub fn num_edges(&self) -> usize {
        self.edges.num_values()
    }

    /// The sections `section` points to, in the order the backend gave them.
    /// Empty if `section` is out of range.
    #[inline]
    #[must_use]
    pub fn edges(&self, section: SectionId) -> &[SectionId] {
        self.edges.row(section.index())
    }

    /// The root set, in the order the backend gave it.
    #[inline]
    #[must_use]
    pub fn roots(&self) -> &[SectionId] {
        &self.roots
    }

    /// The underlying edge table.
    #[inline]
    #[must_use]
    pub fn edge_table(&self) -> &Csr<SectionId> {
        &self.edges
    }
}

/// Builds a [`SectionGraph`] one edge at a time.
///
/// Suited to small graphs and tests; large links should prefer
/// [`SectionGraph::build_parallel`].
#[derive(Clone, Debug)]
pub struct GraphBuilder {
    edges: CsrBuilder<SectionId>,
    roots: Vec<SectionId>,
}

impl GraphBuilder {
    /// Creates a builder for a graph of `sections` sections.
    #[must_use]
    pub fn new(sections: usize) -> Self {
        Self {
            edges: CsrBuilder::new(sections),
            roots: Vec::new(),
        }
    }

    /// Adds the edge `from -> to`: if `from` is live, `to` is live.
    pub fn add_edge(&mut self, from: SectionId, to: SectionId) {
        self.edges.push(from.index(), to);
    }

    /// Marks `section` as a root.
    pub fn add_root(&mut self, section: SectionId) {
        self.roots.push(section);
    }

    /// Makes `members` live together: if any one is live, all are. Used for
    /// COMDAT groups. Adds a cycle of `members.len()` edges.
    pub fn keep_together(&mut self, members: &[SectionId]) {
        if members.len() < 2 {
            return;
        }
        for pair in members.windows(2) {
            self.add_edge(pair[0], pair[1]);
        }
        if let (Some(&last), Some(&first)) = (members.last(), members.first()) {
            self.add_edge(last, first);
        }
    }

    /// Builds the graph.
    ///
    /// # Errors
    ///
    /// Returns [`InputError::OutOfRange`] if an edge endpoint or root is not a
    /// valid section, or [`InputError::TooLarge`] if the section count does
    /// not fit a [`SectionId`].
    pub fn build(self) -> Result<SectionGraph, InputError> {
        if u32::try_from(self.edges.rows()).is_err() {
            return Err(InputError::TooLarge("section count"));
        }
        SectionGraph::new(self.edges.build()?, self.roots)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(index: usize) -> SectionId {
        SectionId::new(index)
    }

    #[test]
    fn builder_and_parallel_agree() {
        let mut builder = GraphBuilder::new(4);
        builder.add_edge(id(0), id(1));
        builder.add_edge(id(0), id(2));
        builder.keep_together(&[id(2), id(3)]);
        builder.add_root(id(0));
        let built = builder.build().unwrap();

        let parallel = SectionGraph::build_parallel(
            4,
            |_| 3,
            |section, slot| match section.index() {
                0 => {
                    slot[0] = id(1);
                    slot[1] = id(2);
                    2
                }
                2 => {
                    slot[0] = id(3);
                    1
                }
                3 => {
                    slot[0] = id(2);
                    1
                }
                _ => 0,
            },
            vec![id(0)],
        )
        .unwrap();
        for section in 0..4 {
            assert_eq!(built.edges(id(section)), parallel.edges(id(section)));
        }
        assert_eq!(built.num_edges(), 4);
        assert_eq!(built.roots(), parallel.roots());
    }

    #[test]
    fn rejects_out_of_range() {
        let mut builder = GraphBuilder::new(2);
        builder.add_edge(id(0), id(2));
        assert!(matches!(
            builder.build(),
            Err(InputError::OutOfRange { index: 2, .. })
        ));
        let mut builder = GraphBuilder::new(2);
        builder.add_root(id(5));
        assert!(builder.build().is_err());
        let mut builder = GraphBuilder::new(2);
        builder.add_edge(id(7), id(0));
        assert!(builder.build().is_err());
    }
}
