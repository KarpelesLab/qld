//! Format-neutral passes that run between resolution and layout.
//!
//! **Workstream W6.** Responsibilities:
//!
//! - Garbage collection (`--gc-sections`): a parallel mark over the section
//!   graph from the root set, with atomic mark bits.
//! - Identical code folding (`--icf`): parallel hashing and iterative
//!   refinement of equivalence classes.
//! - Mergeable section handling (`SHF_MERGE`, `SHF_STRINGS`): parallel
//!   splitting, deduplication, and deterministic offset assignment. Tail
//!   merging at `-O2`.
//!
//! Each pass takes the graph the backend built and gives back a decision; the
//! backend applies it. Results must not depend on thread scheduling: break
//! every tie by input order. See `docs/optimizations.md`.
