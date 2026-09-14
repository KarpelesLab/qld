//! qld is a fast, parallel linker that accepts GNU ld, gold, lld and mold
//! command lines, and can also be driven as a library.
//!
//! # Status
//!
//! Pre-alpha. The pipeline is being implemented milestone by milestone; see
//! `ROADMAP.md`. Nothing outside this crate's root re-exports should be
//! considered stable, and the root API will not be stable until 1.0.
//!
//! # Layout
//!
//! Modules follow the link pipeline described in `docs/architecture.md`:
//!
//! | Module | Role |
//! | --- | --- |
//! | [`args`] | Option model and the argv front ends |
//! | [`input`] | Mapping input files, identifying formats, archives |
//! | [`symbols`] | String interning and the global symbol table |
//! | [`passes`] | Format-neutral passes: GC, ICF, section merging |
//! | [`script`] | GNU linker script parser and evaluator |
//! | [`output`] | Output file writer and post-write steps |
//! | [`elf`], [`coff`], [`macho`] | Format backends |
//! | [`arch`] | Instruction-level helpers shared across formats |
//! | [`debug`] | DWARF handling: compression, indexes, line lookup |
//! | [`plugin`] | LTO plugin host (feature `plugin`) |
//!
//! Format backends own their own symbol precedence and layout rules. The
//! shared modules must not depend on a backend.

#![deny(unsafe_code)]

pub mod arch;
pub mod args;
pub mod coff;
pub mod debug;
pub mod diag;
pub mod elf;
pub mod error;
pub mod ids;
pub mod input;
pub mod macho;
pub mod output;
pub mod passes;
#[cfg(feature = "plugin")]
pub mod plugin;
pub mod script;
pub mod symbols;
pub mod target;

pub use args::{LinkOptions, ParseOutcome, parse_gnu, parse_gnu_with};
pub use diag::{Diagnostic, DiagnosticSink, Severity};
pub use error::{Error, Result};
pub use ids::{FileId, SectionId, SymbolId};
pub use target::{Architecture, BinaryFormat, Endianness, OperatingSystem, PointerWidth, Target};

/// Program name used to prefix diagnostics.
pub const PROGRAM_NAME: &str = "qld";

/// Version string, printed by `--version` and `-v`.
///
/// Build systems detect a GNU-compatible linker by looking for `GNU` in this
/// output, so the wording must not change. See `docs/compatibility.md`.
#[must_use]
pub fn version_line() -> String {
    format!(
        "qld {} (compatible with GNU linkers)",
        env!("CARGO_PKG_VERSION")
    )
}

/// Runs a link described by `options`.
///
/// # Errors
///
/// Returns [`Error::Unimplemented`] until the milestone 1 pipeline lands, and
/// after that any fatal error from the link.
pub fn link(options: &LinkOptions, _diagnostics: &dyn DiagnosticSink) -> Result<()> {
    let _ = options;
    Err(Error::Unimplemented(
        "linking (roadmap M1: static ELF x86-64)".into(),
    ))
}
