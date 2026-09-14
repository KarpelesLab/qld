//! Diagnostics: warnings, errors and notes reported during a link.
//!
//! Diagnostics are emitted from parallel stages, so a sink must be `Send` and
//! `Sync`. Because emission order then depends on thread scheduling, a sink
//! that must produce deterministic output (the CLI does) sorts by
//! [`Diagnostic::order`] before rendering.

use std::fmt;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

/// How serious a diagnostic is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// Informational; attached to a preceding warning or error.
    Note,
    /// The link continues, but something is likely wrong.
    Warning,
    /// The link will fail, though qld keeps going to report more problems.
    Error,
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::Note => "note",
            Self::Warning => "warning",
            Self::Error => "error",
        };
        f.write_str(text)
    }
}

/// Where a diagnostic points, in the input.
#[derive(Clone, Debug, Default)]
pub struct Location {
    /// Input file.
    pub file: PathBuf,
    /// Archive member, when the file is an archive.
    pub member: Option<String>,
    /// Input section name.
    pub section: Option<String>,
    /// Offset within the section.
    pub offset: Option<u64>,
    /// Source file and line, when debug information could supply them.
    pub source: Option<SourceLocation>,
}

/// A source position recovered from debug information.
#[derive(Clone, Debug)]
pub struct SourceLocation {
    /// Source file path as recorded by the compiler.
    pub file: String,
    /// 1-based line number.
    pub line: u32,
}

impl fmt::Display for Location {
    /// Renders as `file.o(member):(.text+0x12)`, the form lld uses.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(source) = &self.source {
            write!(f, "{}:{} (", source.file, source.line)?;
        }
        write!(f, "{}", self.file.display())?;
        if let Some(member) = &self.member {
            write!(f, "({member})")?;
        }
        if let Some(section) = &self.section {
            write!(f, ":({section}")?;
            if let Some(offset) = self.offset {
                write!(f, "+{offset:#x}")?;
            }
            f.write_str(")")?;
        }
        if self.source.is_some() {
            f.write_str(")")?;
        }
        Ok(())
    }
}

/// One reported problem.
#[derive(Clone, Debug)]
pub struct Diagnostic {
    /// Severity of the problem.
    pub severity: Severity,
    /// Main message, lower-case and without a trailing period, as GNU tools
    /// write it: `undefined symbol: foo`.
    pub message: String,
    /// Places in the input this diagnostic refers to.
    pub locations: Vec<Location>,
    /// Extra notes, rendered under the message.
    pub notes: Vec<String>,
    /// Sort key for deterministic output: usually the command-line position of
    /// the input file involved.
    pub order: u64,
}

impl Diagnostic {
    /// Creates a diagnostic with no locations or notes.
    pub fn new(severity: Severity, message: impl Into<String>) -> Self {
        Self {
            severity,
            message: message.into(),
            locations: Vec::new(),
            notes: Vec::new(),
            order: u64::MAX,
        }
    }

    /// Creates an error diagnostic.
    pub fn error(message: impl Into<String>) -> Self {
        Self::new(Severity::Error, message)
    }

    /// Creates a warning diagnostic.
    pub fn warning(message: impl Into<String>) -> Self {
        Self::new(Severity::Warning, message)
    }

    /// Adds a location.
    #[must_use]
    pub fn at(mut self, location: Location) -> Self {
        self.locations.push(location);
        self
    }

    /// Adds a note.
    #[must_use]
    pub fn note(mut self, note: impl Into<String>) -> Self {
        self.notes.push(note.into());
        self
    }

    /// Sets the deterministic sort key.
    #[must_use]
    pub fn order(mut self, order: u64) -> Self {
        self.order = order;
        self
    }
}

/// Receives diagnostics during a link.
///
/// Implementations must be cheap to call from many threads at once.
pub trait DiagnosticSink: Send + Sync {
    /// Reports one diagnostic.
    fn emit(&self, diagnostic: Diagnostic);

    /// Number of diagnostics with [`Severity::Error`] seen so far.
    fn error_count(&self) -> usize;
}

/// A sink that stores diagnostics for the caller to inspect. Used by library
/// users and by tests.
#[derive(Debug, Default)]
pub struct Collect {
    diagnostics: Mutex<Vec<Diagnostic>>,
    errors: AtomicUsize,
}

impl Collect {
    /// Creates an empty collector.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Removes and returns the diagnostics collected so far, in deterministic
    /// order.
    ///
    /// # Panics
    ///
    /// Panics if a previous call panicked while holding the internal lock.
    #[must_use]
    pub fn take_sorted(&self) -> Vec<Diagnostic> {
        let mut diagnostics = std::mem::take(&mut *self.diagnostics.lock().expect("poisoned"));
        diagnostics.sort_by_key(|diagnostic| diagnostic.order);
        diagnostics
    }
}

impl DiagnosticSink for Collect {
    fn emit(&self, diagnostic: Diagnostic) {
        if diagnostic.severity == Severity::Error {
            self.errors.fetch_add(1, Ordering::Relaxed);
        }
        self.diagnostics.lock().expect("poisoned").push(diagnostic);
    }

    fn error_count(&self) -> usize {
        self.errors.load(Ordering::Relaxed)
    }
}

/// A sink that writes GNU-style messages to stderr as they arrive.
#[derive(Debug)]
pub struct Stderr {
    program: String,
    errors: AtomicUsize,
}

impl Stderr {
    /// Creates a sink that prefixes messages with `program`.
    #[must_use]
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            errors: AtomicUsize::new(0),
        }
    }
}

impl DiagnosticSink for Stderr {
    fn emit(&self, diagnostic: Diagnostic) {
        if diagnostic.severity == Severity::Error {
            self.errors.fetch_add(1, Ordering::Relaxed);
        }
        let mut text = format!(
            "{}: {}: {}\n",
            self.program, diagnostic.severity, diagnostic.message
        );
        for location in &diagnostic.locations {
            text.push_str(&format!(">>> referenced by {location}\n"));
        }
        for note in &diagnostic.notes {
            text.push_str(&format!(">>> note: {note}\n"));
        }
        eprint!("{text}");
    }

    fn error_count(&self) -> usize {
        self.errors.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collect_counts_errors_and_sorts() {
        let sink = Collect::new();
        sink.emit(Diagnostic::warning("second").order(2));
        sink.emit(Diagnostic::error("first").order(1));
        assert_eq!(sink.error_count(), 1);
        let diagnostics = sink.take_sorted();
        assert_eq!(diagnostics[0].message, "first");
        assert_eq!(diagnostics[1].message, "second");
    }

    #[test]
    fn location_renders_like_lld() {
        let location = Location {
            file: PathBuf::from("main.o"),
            section: Some(".text".into()),
            offset: Some(0x1a),
            ..Location::default()
        };
        assert_eq!(location.to_string(), "main.o:(.text+0x1a)");
    }
}
