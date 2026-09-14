//! The `qld` command-line linker.
//!
//! This binary is a thin wrapper: it selects a command-line flavor, parses
//! argv and calls [`qld::link`]. Everything it can do is available through
//! the library too.

use std::process::ExitCode;

use qld::args::ParseOutcome;
use qld::diag::{DiagnosticSink, Stderr};

fn main() -> ExitCode {
    let diagnostics = Stderr::new(qld::PROGRAM_NAME);
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();

    match run(&args, &diagnostics) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{}: error: {error}", qld::PROGRAM_NAME);
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[std::ffi::OsString], diagnostics: &dyn DiagnosticSink) -> qld::Result<()> {
    match qld::parse_gnu(args)? {
        ParseOutcome::Help => {
            print!("{}", qld::args::usage());
            Ok(())
        }
        ParseOutcome::Version => {
            println!("{}", qld::version_line());
            Ok(())
        }
        ParseOutcome::Link(options) => qld::link(&options, diagnostics),
    }
}
