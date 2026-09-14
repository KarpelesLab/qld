//! Errors raised while reading, parsing and evaluating linker scripts.

use std::fmt;
use std::path::PathBuf;

/// A fatal problem in a linker script, with the position it was found at.
///
/// Line and column numbers are 1-based; the column counts bytes. The offset is
/// the byte offset in the file named by [`ScriptError::file`], which is the
/// included file when the problem is inside an `INCLUDE`d script.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScriptError {
    /// The script file the problem was found in.
    pub file: PathBuf,
    /// 1-based line number, or 0 when no position applies.
    pub line: u32,
    /// 1-based byte column, or 0 when no position applies.
    pub column: u32,
    /// Byte offset of the problem within the file.
    pub offset: u64,
    /// What went wrong, lower-case, in GNU style (`syntax error: ...`).
    pub message: String,
}

impl ScriptError {
    /// Creates an error at a known position.
    pub fn new(
        file: impl Into<PathBuf>,
        line: u32,
        column: u32,
        offset: u64,
        message: impl Into<String>,
    ) -> Self {
        Self {
            file: file.into(),
            line,
            column,
            offset,
            message: message.into(),
        }
    }
}

impl fmt::Display for ScriptError {
    /// Renders as `file:line:column: message`, the form GNU tools use.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.line == 0 {
            write!(f, "{}: {}", self.file.display(), self.message)
        } else {
            write!(
                f,
                "{}:{}:{}: {}",
                self.file.display(),
                self.line,
                self.column,
                self.message
            )
        }
    }
}

impl std::error::Error for ScriptError {}

impl From<Box<ScriptError>> for ScriptError {
    fn from(error: Box<ScriptError>) -> Self {
        *error
    }
}

impl From<ScriptError> for crate::Error {
    /// Converts into [`crate::Error::Script`], keeping the line and column.
    fn from(error: ScriptError) -> Self {
        crate::Error::Script(Box::new(error))
    }
}

/// Why an expression could not be evaluated.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum EvalError {
    /// A symbol used in the expression is not defined.
    UndefinedSymbol(Vec<u8>),
    /// `ADDR`, `SIZEOF`, `LOADADDR` or `ALIGNOF` named an unknown output
    /// section.
    UndefinedSection(Vec<u8>),
    /// `ORIGIN` or `LENGTH` named an unknown memory region.
    UndefinedRegion(Vec<u8>),
    /// `CONSTANT` named something other than `MAXPAGESIZE` or
    /// `COMMONPAGESIZE`.
    UnknownConstant(Vec<u8>),
    /// `/` or `%` by zero.
    DivisionByZero,
    /// The location counter was used where it has no value (outside
    /// `SECTIONS`, or while evaluating `MEMORY`).
    NoLocationCounter,
    /// An assignment would move `.` backwards inside an output section.
    DotBackwards {
        /// The current value of `.`.
        from: u64,
        /// The value the script tried to assign.
        to: u64,
    },
    /// `ASSERT` failed; carries the script's message.
    AssertionFailed(Vec<u8>),
    /// A value is not available yet (for example a section address before
    /// layout has assigned it). Layout contexts use this during early passes.
    NotYetKnown(String),
    /// Any other failure reported by the evaluation context.
    Other(String),
}

impl fmt::Display for EvalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let lossy = |bytes: &[u8]| String::from_utf8_lossy(bytes).into_owned();
        match self {
            Self::UndefinedSymbol(name) => write!(
                f,
                "undefined symbol `{}' referenced in expression",
                lossy(name)
            ),
            Self::UndefinedSection(name) => write!(
                f,
                "undefined section `{}' referenced in expression",
                lossy(name)
            ),
            Self::UndefinedRegion(name) => write!(
                f,
                "undefined MEMORY region `{}' referenced in expression",
                lossy(name)
            ),
            Self::UnknownConstant(name) => write!(
                f,
                "unknown constant `{}' referenced in expression",
                lossy(name)
            ),
            Self::DivisionByZero => f.write_str("division by zero"),
            Self::NoLocationCounter => f.write_str("location counter used where it has no value"),
            Self::DotBackwards { from, to } => write!(
                f,
                "cannot move location counter backwards (from {from:#x} to {to:#x})"
            ),
            Self::AssertionFailed(message) => f.write_str(&lossy(message)),
            Self::NotYetKnown(what) => write!(f, "{what} is not known yet"),
            Self::Other(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for EvalError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_and_conversion() {
        let error = ScriptError::new("a.ld", 3, 7, 42, "syntax error");
        assert_eq!(error.to_string(), "a.ld:3:7: syntax error");
        let converted: crate::Error = error.into();
        assert!(matches!(converted, crate::Error::Script(_)));
        assert_eq!(converted.to_string(), "a.ld:3:7: syntax error");

        let error = ScriptError::new("b.ld", 0, 0, 0, "cannot open");
        assert_eq!(error.to_string(), "b.ld: cannot open");
        assert!(
            crate::Error::from(error)
                .to_string()
                .contains("cannot open")
        );
    }

    #[test]
    fn eval_error_messages() {
        assert_eq!(
            EvalError::UndefinedSymbol(b"foo".to_vec()).to_string(),
            "undefined symbol `foo' referenced in expression"
        );
        assert!(
            EvalError::DotBackwards { from: 16, to: 8 }
                .to_string()
                .contains("0x10")
        );
    }
}
