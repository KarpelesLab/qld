//! LTO through the GNU linker plugin API.
//!
//! **Workstream W17** (roadmap M6). This is the only part of qld that uses
//! FFI: it `dlopen`s the plugin the compiler toolchain supplies
//! (`liblto_plugin.so` for GCC, `LLVMgold.so` for LLVM) and implements the
//! linker half of the plugin interface. Building qld still requires no C
//! toolchain. The module is gated by the `plugin` cargo feature, on by
//! default. See `docs/optimizations.md` ("LTO").
//!
//! # Using it
//!
//! A [`Session`] drives one link's plugins:
//!
//! ```no_run
//! # fn main() -> qld::Result<()> {
//! use qld::diag::Collect;
//! use qld::plugin::{FileResolution, InputFile, Session, SessionOptions, SymbolResolution};
//!
//! let diagnostics = Collect::new();
//! let mut session = Session::new(SessionOptions::default())?;
//! session.load_plugin(
//!     "/usr/lib/llvm/22/lib64/LLVMgold.so".as_ref(),
//!     &["O2".to_owned()],
//!     &diagnostics,
//! )?;
//! if let Some(file) = session.claim(&InputFile::new("a.o", 0, 1234, 0), &diagnostics)? {
//!     println!("{} symbols", file.symbols.len());
//! }
//! let output = session.all_symbols_read(
//!     |file| {
//!         FileResolution::Included(
//!             file.symbols
//!                 .iter()
//!                 .map(|symbol| {
//!                     if symbol.kind.is_undefined() {
//!                         SymbolResolution::ResolvedExec
//!                     } else {
//!                         SymbolResolution::PrevailingDef
//!                     }
//!                 })
//!                 .collect(),
//!         )
//!     },
//!     &diagnostics,
//! )?;
//! // Link `output.files` in place of the claimed files, then:
//! session.finish(&diagnostics)?;
//! # Ok(())
//! # }
//! ```
//!
//! # Interface coverage
//!
//! The transfer vector offers everything GCC 13–15 and LLVM 18–22 use, plus
//! the section-ordering calls of gold's interface:
//!
//! - Values: API version, GNU ld version, output kind, output name, options.
//! - Registration: claim-file (both versions), all-symbols-read, cleanup,
//!   new-input.
//! - Symbols: `add_symbols` (both versions), `get_symbols` (three versions:
//!   the first reports [`SymbolResolution::PrevailingDefIronlyExp`] as
//!   [`PrevailingDef`](SymbolResolution::PrevailingDef); the third returns
//!   "no symbols" for claimed files not in the link).
//! - Files: `get_input_file`, `get_view` (a read-only mapping kept for the
//!   session), `release_input_file`, `add_input_file`, `add_input_library`,
//!   `set_extra_library_path`.
//! - `message`, with `printf`-style formatting of integer and string
//!   arguments, turned into diagnostics.
//! - ELF section queries (count, type, name, contents, alignment, size),
//!   section ordering and unique-segment requests, returned as data.
//! - `get_wrap_symbols` and API-level negotiation (qld selects level 1).
//!
//! # Safety model
//!
//! The FFI lives in two private modules: `dl` (loading) and `host` (the
//! callbacks and global state); their documentation gives the invariants.
//! See [`Session`] for the one-session-per-process rule and
//! [`options`] for what GCC's plugin needs from the environment.

mod abi;
#[cfg(unix)]
mod dl;
#[cfg_attr(not(unix), allow(dead_code))]
mod format;
#[cfg(unix)]
mod host;
pub mod options;
mod session;
mod types;

pub use options::{PluginFlavor, PluginOption};
pub use session::{PluginInfo, Session, SessionOptions};
pub use types::{
    ClaimedFile, ClaimedSymbol, FileResolution, InputFile, LtoOutput, MessageLevel, OutputKind,
    PluginMessage, SectionKind, SectionRef, SymbolKind, SymbolResolution, SymbolType,
    UniqueSegment, Visibility,
};
