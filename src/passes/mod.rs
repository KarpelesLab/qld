//! Format-neutral passes that run between resolution and layout.
//!
//! **Workstream W6.** Responsibilities:
//!
//! - Garbage collection (`--gc-sections`): a parallel mark over the section
//!   graph from the root set, with atomic mark bits. See [`gc`].
//! - Identical code folding (`--icf`): parallel hashing and iterative
//!   refinement of equivalence classes. See [`icf`].
//! - Mergeable section handling (`SHF_MERGE`, `SHF_STRINGS`): parallel
//!   splitting, deduplication, and deterministic offset assignment. Tail
//!   merging at `-O2`. See [`merge`].
//!
//! Each pass takes the data the backend built and gives back a decision; the
//! backend applies it. The passes know nothing about ELF, COFF or Mach-O:
//! sections are dense [`SectionId`](crate::SectionId)s, numbered in input
//! order, and per-section lists are flat [`Csr`] tables the backend can fill
//! in parallel.
//!
//! Results never depend on thread scheduling or on hash values: every tie is
//! broken by input order (lowest `SectionId` first), and every hash match is
//! confirmed by exact comparison. The passes run on the caller's rayon pool.
//! See `docs/optimizations.md`.

pub mod bitset;
pub mod csr;
pub mod gc;
pub mod graph;
mod hash;
pub mod icf;

pub use bitset::{AtomicBitSet, BitSet};
pub use csr::{Csr, CsrBuilder, InputError};
pub use gc::{GcMarker, LiveSet, ReferenceTree, collect_garbage, why_live};
pub use graph::{GraphBuilder, SectionGraph};
pub use icf::{
    IcfGroup, IcfInput, IcfMode, IcfReloc, IcfReport, IcfResult, IcfSection, IcfTarget,
    fold_identical,
};
