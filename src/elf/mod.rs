//! ELF backend: parsing, resolution rules, layout and relocations.
//!
//! **Workstreams W7 (reading) and W8 (layout and writing).** See `ROADMAP.md`
//! M1 and M2, and `docs/formats.md`.
//!
//! # Submodules
//!
//! | Module | Pipeline stage (`docs/architecture.md`) |
//! | --- | --- |
//! | [`read`] | Zero-copy parsing of objects and shared objects (W7) |
//! | [`link`](mod@link) | The driver: runs the stages below in order |
//! | [`inputs`] | 2–3: search paths, archives, input scripts, target inference |
//! | [`object`] | 4: per-object symbols, section classes, merge splitting |
//! | [`resolve`] | 5: ELF precedence rules, COMDAT deduplication, duplicates |
//! | [`sections`] | Dense input section numbering and per-section state |
//! | [`rules`] | Default output section rules (GNU ld's built-in script, as data) |
//! | [`place`] | 10a: output section assignment, orphans |
//!
//! Only x86-64 static executables are produced so far; the other output
//! kinds return [`Error::Unimplemented`](crate::Error::Unimplemented) naming
//! their milestone.

pub mod arch;
pub mod common;
pub mod defined;
pub mod ehframe;
pub mod gc;
pub mod inputs;
pub mod layout;
pub mod link;
pub mod merge;
pub mod object;
pub mod place;
pub mod read;
pub mod refs;
pub mod resolve;
pub mod rules;
pub mod scan;
pub mod sections;
pub mod symtab;
pub mod synth;
pub mod values;
pub mod write;

pub use link::link;
