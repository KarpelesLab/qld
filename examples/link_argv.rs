//! Links from a GNU ld command line, as the `qld` binary does.
//!
//! ```sh
//! cargo run --example link_argv -- -o hello /usr/lib/crt1.o hello.o -lc
//! ```
//!
//! The binary adds process-level behavior on top of this (`--fork`, exit
//! codes, plugin fatal errors exiting the process); a library caller gets
//! everything else from the same two calls, [`qld::parse_gnu`] and
//! [`qld::link`].

use std::process::ExitCode;

use qld::ParseOutcome;
use qld::diag::{Diagnostic, DiagnosticSink, Stderr};

fn main() -> ExitCode {
    // argv[0] selects the flavor, as for the binary: `ld.qld` is GNU,
    // `ld64.qld` is Apple's ld64.
    let args: Vec<_> = std::iter::once("ld".into())
        .chain(std::env::args_os().skip(1))
        .collect();
    let diagnostics = Stderr::new(qld::PROGRAM_NAME);
    match link(&args, &diagnostics) {
        Ok(()) => ExitCode::SUCCESS,
        Err(qld::Error::Reported { .. }) => ExitCode::FAILURE,
        Err(error) => {
            eprintln!("{}: error: {error}", qld::PROGRAM_NAME);
            ExitCode::FAILURE
        }
    }
}

fn link(args: &[std::ffi::OsString], diagnostics: &dyn DiagnosticSink) -> qld::Result<()> {
    let options = match qld::parse_gnu(args)? {
        ParseOutcome::Link(options) => options,
        ParseOutcome::Help => {
            print!("{}", qld::args::usage());
            return Ok(());
        }
        ParseOutcome::Version => {
            println!("{}", qld::version_line());
            return Ok(());
        }
    };
    // Parsing does no reporting: warnings such as an unknown `-z` keyword
    // come back in the options, for the caller to emit.
    if !options.no_warnings {
        for warning in &options.warnings {
            diagnostics.emit(Diagnostic::warning(warning.clone()));
        }
    }
    if options.fatal_warnings && !options.warnings.is_empty() {
        return Err(qld::Error::Reported {
            errors: options.warnings.len(),
        });
    }
    qld::link(&options, diagnostics)
}
