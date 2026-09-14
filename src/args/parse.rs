//! argv front ends.
//!
//! **Workstream W1** owns this module. The current body handles only `--help`
//! and `--version`, so that the binary is usable; everything else returns
//! [`Error::Unimplemented`].
//!
//! The rules this must follow — single- versus double-dash long options, the
//! `-o` exception, joined and separate values, `-z` keywords, response files,
//! `=`/`$SYSROOT` prefixes, positional state, and the
//! implemented/accepted-ignored/unsupported policy — are in
//! `docs/compatibility.md`.

use std::ffi::OsString;

use crate::args::options::LinkOptions;
use crate::error::{Error, Result};

/// What a command line asked for.
#[derive(Clone, Debug)]
pub enum ParseOutcome {
    /// Perform a link with these options.
    Link(Box<LinkOptions>),
    /// Print usage and exit successfully.
    Help,
    /// Print the version line and exit successfully.
    Version,
}

/// Parses a GNU ld / gold / lld / mold command line.
///
/// `args` includes `argv[0]`, which also selects the flavor when it is a name
/// such as `ld64.qld`.
///
/// # Errors
///
/// Returns [`Error::Option`] for an unknown or malformed option.
pub fn parse_gnu<S: AsRef<std::ffi::OsStr>>(args: &[S]) -> Result<ParseOutcome> {
    let mut saw_argument = false;
    for arg in args.iter().skip(1) {
        let arg = arg.as_ref();
        saw_argument = true;
        if arg == "--help" || arg == "-help" {
            return Ok(ParseOutcome::Help);
        }
        if arg == "--version" || arg == "-V" || arg == "-v" {
            return Ok(ParseOutcome::Version);
        }
    }

    if !saw_argument {
        return Err(Error::Option("no input files".into()));
    }

    let _unparsed: Vec<OsString> = args.iter().map(|arg| arg.as_ref().to_os_string()).collect();
    Err(Error::Unimplemented(
        "command-line parsing (roadmap M0, workstream W1)".into(),
    ))
}

/// Returns the `--help` text.
#[must_use]
pub fn usage() -> String {
    format!(
        "Usage: qld [options] file...\n\
         \n\
         qld is a linker compatible with the GNU ld, gold, lld and mold command lines.\n\
         \n\
         This is {version}, a pre-alpha build: option parsing and linking are not\n\
         implemented yet. See ROADMAP.md.\n\
         \n\
         Options:\n\
         \x20 --help                      Print this message\n\
         \x20 --version, -v, -V           Print the version\n",
        version = crate::version_line()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_help_and_version() {
        assert!(matches!(
            parse_gnu(&["qld", "--help"]).unwrap(),
            ParseOutcome::Help
        ));
        assert!(matches!(
            parse_gnu(&["qld", "--version"]).unwrap(),
            ParseOutcome::Version
        ));
        assert!(matches!(
            parse_gnu(&["qld", "-v"]).unwrap(),
            ParseOutcome::Version
        ));
    }

    #[test]
    fn version_line_is_detected_as_gnu_compatible() {
        // autoconf and libtool look for "GNU" in `ld -v` output.
        assert!(crate::version_line().contains("GNU"));
    }

    #[test]
    fn empty_command_line_is_an_error() {
        assert!(parse_gnu(&["qld"]).is_err());
    }
}
