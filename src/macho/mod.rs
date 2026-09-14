//! Mach-O backend (Apple platforms).
//!
//! Roadmap M8. Scope is in `docs/formats.md`: objects and dylibs, `.tbd`
//! stubs, subsection atomization, chained fixups, compact unwind, ad-hoc code
//! signing, and universal (fat) binaries.
//!
//! - [`read`]: the input side (workstream W15). Universal binaries, `MH_OBJECT`
//!   files with atomization for dead stripping, compact unwind and
//!   `__eh_frame` records, dylibs with their export tries and chained-fixup
//!   imports, and text-based stubs (`.tbd` v3–v5).
//!
//! Layout, synthesis (`__unwind_info`, chained fixups, the export trie),
//! code signing and the ld64 driver are not started.

pub mod read;
