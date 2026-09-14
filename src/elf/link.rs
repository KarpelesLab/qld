//! The ELF link driver: runs the pipeline in `docs/architecture.md` for an ELF
//! target, from resolved options to a written output file.
//!
//! **Workstream W8** implements this.

use crate::args::LinkOptions;
use crate::diag::DiagnosticSink;
use crate::error::{Error, Result};

/// Links an ELF output described by `options`.
///
/// Called by [`crate::link`] inside a rayon pool sized by `--threads`.
///
/// # Errors
///
/// Returns [`Error::Unimplemented`] until milestone M1 lands.
pub fn link(options: &LinkOptions, diagnostics: &dyn DiagnosticSink) -> Result<()> {
    let _ = (options, diagnostics);
    Err(Error::Unimplemented(
        "linking (roadmap M1: static ELF x86-64)".into(),
    ))
}
