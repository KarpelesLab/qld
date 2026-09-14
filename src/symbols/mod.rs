//! String interning and the global symbol table.
//!
//! **Workstream W4.** Responsibilities:
//!
//! - Intern symbol names as `&[u8]` slices borrowed from the input mappings,
//!   with the hash computed once during parallel parsing and reused later.
//! - Provide the sharded concurrent symbol table: insert-or-update keyed by
//!   name (plus version, where the format has versions), with the shard chosen
//!   from the precomputed hash.
//! - Hold per-symbol state that parallel passes update: the current best
//!   definition, and atomic flag bits (needs GOT/PLT/copy relocation/TLS,
//!   address-taken).
//! - Take the "which definition wins" rule from the format backend rather than
//!   hard-coding ELF semantics.
//!
//! Resolution must not depend on which thread runs first: ties are broken by
//! input position. See `docs/architecture.md` ("Symbol resolution").
