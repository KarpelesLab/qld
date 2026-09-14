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
//! - [`resolve_symbols`]: the format-neutral driver that inserts definitions
//!   and extracts archive members, round by round, to a fixpoint, over any
//!   file type implementing [`ResolveFile`]. Each round's work is
//!   proportional to the files that became live in it.
//! - [`RoundHook`] and [`resolve_symbols_with`]: a per-round hook between
//!   loading files and inserting their symbols, and [`GroupClaims`], the
//!   deterministic claim table a backend uses there to deduplicate COMDAT
//!   groups before insertion.
//! - Undefined and duplicate symbols as sorted data ([`UndefinedSymbol`],
//!   [`DuplicateSymbol`]) for diagnostics to render.
//!
//! Resolution must not depend on which thread runs first: ties are broken by
//! [`InputPosition`]. See `docs/architecture.md` ("Symbol resolution").
//!
//! This module must not depend on any format backend.

mod claims;
mod definition;
pub mod elf_reference;
mod flags;
mod name;
mod report;
pub mod resolve;
pub mod table;
mod util;

pub use claims::{ClaimRound, GroupClaims};
pub use definition::{Definition, DefinitionKind, Resolver, takes_precedence};
pub use flags::SymbolFlags;
pub use name::{InputPosition, SymbolName};
pub use report::{DuplicateSymbol, SymbolReference, UndefinedSymbol};
pub use resolve::{
    Resolution, ResolveFile, RoundFile, RoundHook, SymbolUse, resolve_symbols, resolve_symbols_with,
};
pub use table::{InternJob, SymbolTable};
