//! Format-neutral passes that run between resolution and layout.
//!
//! **Workstream W6.** Responsibilities:
//!
//! - Garbage collection (`--gc-sections`): a parallel mark over the section
//!   graph from the root set, with atomic mark bits. See [`gc`].
//! - Identical code folding (`--icf`): parallel hashing and iterative
//!   refinement of equivalence classes. See [`icf`].
//! - Mergeable section handling (`SHF_MERGE`, `SHF_STRINGS`), in two phases
//!   at two pipeline stages. See [`merge`].
//!   1. **Split**, during object parsing (stage 4): [`split_section`] cuts one
//!      input section into pieces and hashes each piece. It depends on no
//!      other section, so the backend calls it from its parallel per-file
//!      parsing. The resulting [`SplitSection`] maps an input offset to a
//!      (piece, addend within piece) [`PieceRef`] before any deduplication,
//!      which is how the relocation scan (stage 6) records references into
//!      merge sections.
//!   2. **Deduplicate and lay out**, after garbage collection and before ICF
//!      (stage 8): [`merge_split_sections`] takes the live split sections
//!      with their output groups, in input order, and optionally a per-piece
//!      liveness bitmap. It deduplicates pieces in parallel, assigns output
//!      offsets in first-occurrence order (tail merging at `-O2`), and
//!      answers (section, piece) and (section, offset) queries; the output
//!      writer copies the contents with [`MergedSections::write_group`].
//!
//!   [`merge_sections`] runs both phases at once, for callers that have every
//!   section at hand.
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
//!
//! Inconsistent arguments are reported as an [`InputError`]. That is a bug in
//! qld, not in an input file, so it converts into
//! [`crate::Error::Internal`] with `?`. Validating input files is the
//! backend's job, except for the contents of mergeable sections, which
//! [`split_section`] reports as a [`MalformedMerge`]; the backend turns that
//! into an error naming the file with [`MalformedMerge::into_error`].

pub mod bitset;
pub mod csr;
pub mod gc;
pub mod graph;
mod hash;
pub mod icf;
pub mod merge;

pub use bitset::{AtomicBitSet, BitSet};
pub use csr::{Csr, CsrBuilder, InputError};
pub use gc::{GcMarker, LiveSet, ReferenceTree, collect_garbage, mark_reachable, why_live};
pub use graph::{GraphBuilder, SectionGraph};
pub use icf::{
    IcfGroup, IcfInput, IcfMode, IcfReloc, IcfReport, IcfResult, IcfSection, IcfTarget,
    fold_identical,
};
pub use merge::{
    MalformedMerge, MergeError, MergeGroup, MergeInput, MergeKind, MergeProblem, MergeSection,
    MergedGroup, MergedSections, OutputPiece, PieceRef, SplitSection, merge_sections,
    merge_split_sections, split_section,
};
