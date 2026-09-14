//! Fatal error type.
//!
//! Only errors that stop the link are returned as [`Error`]. Anything the link
//! can continue past is a [`crate::diag::Diagnostic`] instead.
//!
//! Parsing code must never panic on malformed input: bounds-check every
//! offset, use checked arithmetic, and return [`Error::Malformed`].

use std::fmt;
use std::path::PathBuf;

/// Result alias used throughout qld.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// A fatal linker error.
#[non_exhaustive]
#[derive(Debug)]
pub enum Error {
    /// An input file could not be parsed.
    Malformed {
        /// The file the problem was found in.
        file: PathBuf,
        /// Archive member name, when the file is an archive.
        member: Option<String>,
        /// Byte offset of the problem within the file.
        offset: u64,
        /// What was wrong, phrased as a noun ("section header table").
        what: String,
    },
    /// An I/O operation failed.
    Io {
        /// The path being operated on, if any.
        path: Option<PathBuf>,
        /// The underlying error.
        source: std::io::Error,
    },
    /// A command-line option was not recognized, or its value was invalid.
    Option(String),
    /// The link failed because of errors already reported to the diagnostic
    /// sink (for example undefined symbols).
    Reported {
        /// How many errors were reported.
        errors: usize,
    },
    /// A feature that qld intends to support is not implemented yet.
    ///
    /// Every use of this must name the roadmap milestone that will remove it.
    Unimplemented(String),
}

impl Error {
    /// Creates an [`Error::Io`] that names the path involved.
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: Some(path.into()),
            source,
        }
    }

    /// Creates an [`Error::Malformed`] for a file with no archive member.
    pub fn malformed(path: impl Into<PathBuf>, offset: u64, what: impl Into<String>) -> Self {
        Self::Malformed {
            file: path.into(),
            member: None,
            offset,
            what: what.into(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed {
                file,
                member,
                offset,
                what,
            } => {
                write!(f, "{}", file.display())?;
                if let Some(member) = member {
                    write!(f, "({member})")?;
                }
                write!(f, ": malformed {what} at offset {offset:#x}")
            }
            Self::Io {
                path: Some(path),
                source,
            } => write!(f, "{}: {source}", path.display()),
            Self::Io { path: None, source } => write!(f, "{source}"),
            Self::Option(message) => write!(f, "{message}"),
            Self::Reported { errors } if *errors == 1 => write!(f, "1 error"),
            Self::Reported { errors } => write!(f, "{errors} errors"),
            Self::Unimplemented(what) => write!(f, "not implemented yet: {what}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(source: std::io::Error) -> Self {
        Self::Io { path: None, source }
    }
}
