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
//! - The linker (workstream W25), driven by [`link()`](fn@link):
//!
//! | Module | Role |
//! | --- | --- |
//! | [`config`] | Per-architecture settings derived from the options |
//! | [`inputs`] | Search paths, objects, archives, dylibs, `.tbd` stubs |
//! | [`object`] | Objects with their atoms and global symbols |
//! | [`resolve`] | Symbol precedence and duplicate reports |
//! | [`state`] | Resolved symbols, atom numbering, coalescing, dead stripping |
//! | [`scan`] | Stubs, `__got`, `__thread_ptrs`, imports, dylib ordinals |
//! | [`layout`] | Output sections, segments, addresses |
//! | [`reloc`] | Relocation decoding and application (arm64, x86_64) |
//! | [`thunks`] | arm64 range-extension thunks |
//! | [`addr`] | Final addresses of symbols and synthetic entries |
//! | [`sections`] | Section contents |
//! | [`unwind`], [`eh_frame`] | `__unwind_info` and `__eh_frame` |
//! | [`fixups`] | Chained fixups and rebase/bind opcodes |
//! | [`trie`] | The export trie |
//! | [`symtab`], [`stabs`] | Symbol tables and the STABS debug map |
//! | [`write`](mod@write) | Header, load commands, `__LINKEDIT` |
//! | [`codesign`], [`sha256`] | Ad-hoc code signature |
//! | [`fat`] | Universal binaries |
//! | [`buf`] | Byte helpers |

pub mod addr;
pub mod buf;
pub mod codesign;
pub mod config;
pub mod eh_frame;
pub mod fat;
pub mod fixups;
pub mod inputs;
pub mod layout;
pub mod link;
pub mod object;
pub mod read;
pub mod reloc;
pub mod resolve;
pub mod scan;
pub mod sections;
pub mod sha256;
pub mod stabs;
pub mod state;
pub mod symtab;
pub mod thunks;
pub mod trie;
pub mod unwind;
pub mod write;

pub use link::{link, link_to_bytes};
