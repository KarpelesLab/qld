//! qld is a fast, parallel linker that accepts GNU ld, gold, lld and mold
//! command lines, and can also be driven as a library.
//!
//! # Status
//!
//! Pre-alpha. The pipeline is being implemented milestone by milestone; see
//! `ROADMAP.md`. Nothing outside this crate's root re-exports should be
//! considered stable, and the root API will not be stable until 1.0.
//!
//! # Library use
//!
//! Build [`LinkOptions`] by hand or from a command line ([`parse_gnu`]), and
//! call [`link`]. Diagnostics go to a [`DiagnosticSink`] of your choice;
//! nothing is printed and the process never exits. Inputs can be byte
//! buffers ([`InputKind::bytes`], [`MemoryFiles`]), the output can come back
//! as bytes ([`link_to_memory`], [`OutputBuffer`]), a link can be cancelled
//! from another thread ([`CancelToken`]), and it runs in the caller's rayon
//! pool when called inside [`rayon::ThreadPool::install`].
//!
//! ```no_run
//! use qld::{InputAttrs, InputKind, LinkOptions, OutputKind};
//! use qld::diag::Collect;
//!
//! # let object: Vec<u8> = Vec::new();
//! let mut options = LinkOptions::new();
//! options.kind = OutputKind::StaticExecutable;
//! options.push_input(InputKind::bytes("main.o", object), InputAttrs::default());
//! let diagnostics = Collect::new();
//! let image: Vec<u8> = qld::link_to_memory(&options, &diagnostics)?;
//! # Ok::<(), qld::Error>(())
//! ```
//!
//! The `examples/` directory has complete programs: `link_argv`,
//! `in_memory`, `custom_sink`, `rayon_pool` and `cancel`.
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
//! | [`demangle`] | Itanium C++ and Rust symbol demangling for diagnostics |
//! | [`hints`] | Suggestions for undefined symbols: missing `-l`, versions, near misses |
//! | [`plugin`] | LTO plugin host (feature `plugin`) |
//!
//! Format backends own their own symbol precedence and layout rules. The
//! shared modules must not depend on a backend.
//!
//! Only [`args`], [`diag`], [`error`] and [`target`] are documented; the
//! other modules are public for qld's own tests and tools, and are not
//! covered by semantic versioning.

#![deny(unsafe_code)]

#[doc(hidden)]
pub mod arch;
pub mod args;
#[doc(hidden)]
pub mod coff;
#[doc(hidden)]
pub mod debug;
#[doc(hidden)]
pub mod demangle;
pub mod diag;
#[doc(hidden)]
pub mod elf;
pub mod error;
#[doc(hidden)]
pub mod hints;
#[doc(hidden)]
pub mod ids;
#[doc(hidden)]
pub mod input;
#[doc(hidden)]
pub mod macho;
#[doc(hidden)]
pub mod output;
#[doc(hidden)]
pub mod passes;
#[cfg(feature = "plugin")]
#[doc(hidden)]
pub mod plugin;
#[doc(hidden)]
pub mod script;
#[doc(hidden)]
pub mod symbols;
pub mod target;

pub use args::{
    CancelToken, InputAttrs, InputKind, LinkOptions, OutputBuffer, OutputKind, ParseOutcome,
    parse_gnu, parse_gnu_with,
};
pub use diag::{Diagnostic, DiagnosticSink, Severity};
pub use error::{Error, Result};
#[doc(hidden)]
pub use ids::{FileId, SectionId, SymbolId};
pub use input::source::{InputProvider, MemoryFiles};
pub use script::ScriptError;
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
/// With `--threads`, parallel stages run in a rayon pool of that size created
/// for the duration of the link. Without it, the format driver chooses: the
/// ELF driver sizes a pool from the input (small links run faster on few
/// threads), never larger than the current pool. To run in a pool you already
/// own, call `link` (or a format driver such as [`elf::link`](fn@elf::link))
/// inside your pool's `install`.
///
/// A successful link runs [`LinkOptions::on_output_complete`] once the
/// output is complete: the ELF driver runs it before freeing its data and
/// unmapping the inputs, and `link` runs it before returning `Ok` if the
/// driver did not.
///
/// # Example
///
/// ```no_run
/// use qld::diag::Stderr;
///
/// let args = ["ld", "-o", "hello", "crt1.o", "hello.o", "-lc"];
/// if let qld::ParseOutcome::Link(options) = qld::parse_gnu(&args)? {
///     qld::link(&options, &Stderr::new(qld::PROGRAM_NAME))?;
/// }
/// # Ok::<(), qld::Error>(())
/// ```
///
/// # Errors
///
/// Returns any fatal error from the link, including
/// [`Error::Unimplemented`] for targets and features not supported yet,
/// [`Error::Reported`] when errors were reported to `diagnostics`, and
/// [`Error::Cancelled`] when [`LinkOptions::cancel`] was cancelled.
pub fn link(options: &LinkOptions, diagnostics: &dyn DiagnosticSink) -> Result<()> {
    options.check_cancelled()?;
    let format = options.target.map(|target| target.format);
    if options.output_buffer.is_some()
        && !matches!(
            format,
            None | Some(BinaryFormat::Elf | BinaryFormat::Pe | BinaryFormat::MachO)
        )
    {
        return Err(Error::Unimplemented(format!(
            "in-memory output for {format:?} links (roadmap M9)"
        )));
    }
    let run = || match format {
        None | Some(BinaryFormat::Elf) => elf::link(options, diagnostics),
        Some(BinaryFormat::Pe) => coff::link(options, diagnostics),
        Some(BinaryFormat::MachO) => macho::link(options, diagnostics),
        Some(format) => Err(Error::Unimplemented(format!(
            "{format:?} output (see ROADMAP.md)"
        ))),
    };
    match options.threads {
        Some(threads) => rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .map_err(|error| Error::Internal(format!("cannot create thread pool: {error}")))?
            .install(run),
        None => run(),
    }
    .inspect(|()| options.output_complete())
}

/// Runs a link described by `options` and returns the output image instead
/// of writing [`LinkOptions::output`], which then only names the output.
///
/// This is [`link`] with a fresh [`OutputBuffer`] in
/// [`LinkOptions::output_buffer`]. Side outputs (`-Map`,
/// `--dependency-file`) are still written as files.
///
/// # Errors
///
/// As [`link`].
pub fn link_to_memory(options: &LinkOptions, diagnostics: &dyn DiagnosticSink) -> Result<Vec<u8>> {
    let buffer = OutputBuffer::new();
    let mut options = options.clone();
    options.output_buffer = Some(buffer.clone());
    link(&options, diagnostics)?;
    buffer
        .take()
        .ok_or_else(|| Error::Internal("the link driver wrote no in-memory output".into()))
}
