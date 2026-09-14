//! String interning and the global symbol table.
//!
//! **Workstream W4.** This module owns:
//!
//! - [`SymbolName`]: symbol names as `&[u8]` slices borrowed from the input
//!   mappings, plus an optional version, with a deterministic `foldhash` hash
//!   computed once, during parallel parsing, and reused afterwards.
//! - [`SymbolTable`]: the sharded concurrent table (`hashbrown` tables behind
//!   per-shard locks, shard chosen from the precomputed hash). Interning is a
//!   batch operation whose symbol IDs follow first occurrence in input order,
//!   so they never depend on thread scheduling; see the [`table`] module for
//!   how and what it costs.
//! - Per-symbol state in struct-of-arrays vectors indexed by
//!   [`SymbolId`](crate::SymbolId): the current best [`Definition`], and
//!   atomic [`SymbolFlags`] (needs GOT/PLT/copy relocation/TLS,
//!   address-taken, exported, referenced) that any thread can set lock-free.
//! - The [`Resolver`] trait, through which a format backend says which
//!   definition wins. ELF's rules are here only as a reference and test
//!   implementation, [`elf_reference::ElfReferenceRules`]; the ELF backend owns
//!   the real ones.
//!
//! Resolution must not depend on which thread runs first: ties are broken by
//! [`InputPosition`]. See `docs/architecture.md` ("Symbol resolution").
//!
//! This module must not depend on any format backend.

mod definition;
pub mod elf_reference;
mod flags;
mod name;
pub mod table;

pub use definition::{Definition, DefinitionKind, Resolver, takes_precedence};
pub use flags::SymbolFlags;
pub use name::{InputPosition, SymbolName};
pub use table::{InternJob, SymbolTable};
