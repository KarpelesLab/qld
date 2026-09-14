//! ELF backend: parsing, resolution rules, layout and relocations.
//!
//! **Workstreams W7 (reading), W8 (static layout, writing and the driver)
//! and W11 (dynamic linking).** See `ROADMAP.md` M1 and M2, and
//! `docs/formats.md`.
//!
//! # Submodules
//!
//! In pipeline order (stage numbers from `docs/architecture.md`):
//!
//! | Module | Role |
//! | --- | --- |
//! | [`read`] | Zero-copy parsing of objects and shared objects (W7) |
//! | [`link`](mod@link) | The driver: sequences the stages below |
//! | [`inputs`] | 2–3: `-l` search, archives with lazy members, shared objects, input scripts, target inference |
//! | [`object`] | 4: per-object symbols for resolution, section classes, merge splitting, COMDAT groups, notes |
//! | [`dso`] | 4–5: shared object symbols, `DT_NEEDED` and `--as-needed`, undefined symbols of shared libraries |
//! | [`resolve`] | 5: ELF precedence rules, COMDAT group claims during resolution rounds, duplicate symbol diagnostics |
//! | [`lto`] | 5: LTO: IR inputs claimed through plugins, resolutions reported, resolution rerun with the generated objects |
//! | [`xref`] | 5: `-y`/`--trace-symbol`, `--warn-common` and the `--cref` table |
//! | [`export`] | 5: output mode, merged visibility, exports, preemptibility, version scripts |
//! | [`sections`] | Dense input section numbering, liveness and the ICF fold map |
//! | [`refs`] | Relocation targets: from a symbol index in a file to its definition |
//! | [`rules`] | GNU ld's default x86-64 layout as data (a `SECTIONS` script can replace it, M3) |
//! | [`place`] | 10a: output section assignment and orphan placement |
//! | [`defined`] | Linker-defined symbols (`_end`, `_DYNAMIC`, `__start_SEC`, `--defsym`, …) |
//! | [`ehframe`] | `.eh_frame` records: GC edges, FDE liveness, CIE deduplication |
//! | [`gc`] | 7: `--gc-sections` graph and roots, `--print-gc-sections`, `--why-live` |
//! | [`reloc`] | 6, 11: per-relocation decisions shared by the scan and the writer |
//! | [`scan`] | 6: relocation scan: symbol needs, dynamic relocation counts, undefined symbols |
//! | [`common`] | Common symbol allocation |
//! | [`merge`] | 8: merged sections through [`crate::passes::merge`] |
//! | [`icf`] | 8: `--icf` through [`crate::passes::icf`] |
//! | [`synth`] | 9: GOT, PLT, copy relocations, `.interp`, build-id and property notes, `.comment` |
//! | [`dynsym`] | 9: `.dynsym`, `.dynstr`, hash tables, symbol versions, `.dynamic` |
//! | [`symtab`] | `.symtab` and `.strtab` planning and writing |
//! | [`layout`] | 10: output section contents, segments (RELRO included), addresses |
//! | [`values`] | Symbol, section, GOT and PLT addresses after layout |
//! | [`write`](mod@write) | 11: parallel chunked writing, relocation in place, `.rela.dyn`, `.eh_frame_hdr` |
//! | [`emit`] | `--emit-relocs`: input relocations rewritten into `.rela` trailers |
//! | [`map`] | `-Map` and `-M`, and where the `--cref` table goes |
//! | [`relocatable`] | 10–11 for `-r`: combined sections, groups, symbol table and rewritten relocations |
//! | [`arch`] | Per-architecture relocation classification, relaxation and PLT encodings (x86-64) |
//!
//! # Scope
//!
//! x86-64 static executables (roadmap M1), and dynamic executables, PIEs,
//! static PIEs, shared objects linked against shared libraries, and
//! relocatable output (M2). Other architectures return
//! [`Error::Unimplemented`](crate::Error::Unimplemented) naming M4.
//!
//! GOT and PLT entries are owned by global symbols or local (file, symbol)
//! pairs and planned from the [`SymbolFlags`](crate::symbols::SymbolFlags)
//! the scan sets; synthetic sections are a closed list the rules place like
//! input sections; and the layout rules are data.
//!
//! # Threads
//!
//! Without `--threads`, the inputs are mapped in a pool of at most 16
//! threads, and the link runs in a pool of one thread per 4 MiB of input (at
//! most 16): small links are faster on few threads, and rayon's global pool
//! (a thread per core) is never started. Results never depend on the thread
//! count; fixtures check this byte for byte.

pub mod arch;
pub mod common;
pub mod defined;
pub mod dso;
pub mod dynsym;
pub mod ehframe;
pub mod emit;
pub mod export;
pub mod gc;
pub mod icf;
pub mod inputs;
pub mod layout;
pub mod link;
pub mod lto;
pub mod map;
pub mod merge;
pub mod object;
pub mod place;
pub mod read;
pub mod refs;
pub mod reloc;
pub mod relocatable;
pub mod resolve;
pub mod rules;
pub mod scan;
pub mod sections;
pub mod symtab;
pub mod synth;
pub mod values;
pub mod write;
pub mod xref;

pub use link::link;
