//! ELF backend: parsing, resolution rules, layout and relocations.
//!
//! **Workstreams W7 (reading) and W8 (layout, writing and the driver).** See
//! `ROADMAP.md` M1 and M2, and `docs/formats.md`.
//!
//! # Submodules
//!
//! In pipeline order (stage numbers from `docs/architecture.md`):
//!
//! | Module | Role |
//! | --- | --- |
//! | [`read`] | Zero-copy parsing of objects and shared objects (W7) |
//! | [`link`](mod@link) | The driver: sequences the stages below |
//! | [`inputs`] | 2–3: `-l` search, archives with lazy members, input scripts, target inference |
//! | [`object`] | 4: per-object symbols for resolution, section classes, merge splitting, COMDAT groups, notes |
//! | [`resolve`] | 5: ELF precedence rules, COMDAT deduplication, duplicate symbol diagnostics |
//! | [`sections`] | Dense input section numbering, liveness and the ICF fold map |
//! | [`refs`] | Relocation targets: from a symbol index in a file to its definition |
//! | [`rules`] | GNU ld's default x86-64 layout as data (a `SECTIONS` script can replace it, M3) |
//! | [`place`] | 10a: output section assignment and orphan placement |
//! | [`defined`] | Linker-defined symbols (`_end`, `__start_SEC`, `--defsym`, …) |
//! | [`ehframe`] | `.eh_frame` records: GC edges, FDE liveness, CIE deduplication |
//! | [`gc`] | 7: `--gc-sections` graph and roots, `--print-gc-sections`, `--why-live` |
//! | [`scan`] | 6: relocation scan: symbol flags (GOT, IFUNC), undefined symbols |
//! | [`common`] | Common symbol allocation |
//! | [`merge`] | 8: merged sections through [`crate::passes::merge`] |
//! | [`icf`] | 8: `--icf` through [`crate::passes::icf`] |
//! | [`synth`] | 9: GOT, IFUNC PLT and `IRELATIVE`, build-id and property notes, `.comment` |
//! | [`symtab`] | `.symtab` and `.strtab` planning and writing |
//! | [`layout`] | 10: output section contents, segments, addresses |
//! | [`values`] | Symbol and section addresses after layout |
//! | [`write`](mod@write) | 11: parallel chunked writing, relocation in place, `.eh_frame_hdr` |
//! | [`map`] | `-Map` and `-M` |
//! | [`arch`] | Per-architecture relocation classification and relaxation (x86-64) |
//!
//! # Scope
//!
//! Static x86-64 executables (roadmap M1). Shared objects, PIE and `-r`
//! return [`Error::Unimplemented`](crate::Error::Unimplemented) naming M2,
//! as do shared object inputs; other architectures name M4.
//!
//! The pieces M2 builds on are already generic: GOT entries are owned by
//! global symbols or local (file, symbol) pairs and planned from the
//! [`SymbolFlags`](crate::symbols::SymbolFlags) the scan sets; synthetic
//! sections are a closed list the rules place like input sections; and the
//! layout rules are data.
//!
//! # Threads
//!
//! Without `--threads`, a link runs in a pool of one thread per 16 MiB of
//! input (at most 32): small links are faster on few threads. Results never
//! depend on the thread count; fixtures check this byte for byte.

pub mod arch;
pub mod common;
pub mod defined;
pub mod ehframe;
pub mod gc;
pub mod icf;
pub mod inputs;
pub mod layout;
pub mod link;
pub mod map;
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
