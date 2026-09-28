//! Diagnostics: warnings, errors and notes reported during a link.
//!
//! A diagnostic is a [`Severity`], a one-line message, and any number of
//! input [`Location`]s, free-form detail lines and notes. Sinks render them;
//! [`Stderr`] does so the way lld does, which GNU ld's own consumers accept:
//!
//! ```text
//! qld: error: undefined symbol: foo
//! >>> referenced by main.c:3 (/home/me/main.c:3)
//! >>>               main.o:(.text+0x1d)
//! >>> did you mean: foo_impl?
//! ```
//!
//! Diagnostics are emitted from parallel stages, so a sink must be `Send` and
//! `Sync`. Because emission order then depends on thread scheduling, a sink
//! that must produce deterministic output (the CLI does) buffers everything
//! and sorts by [`Diagnostic::order`] before rendering. The sort is stable,
//! so diagnostics a single stage emits in a fixed order — which every stage
//! of qld does — keep that order, and a note emitted with its parent's
//! `order` stays directly under it.
//!
//! [`Stderr`] therefore renders nothing until [`DiagnosticSink::flush`] is
//! called, which `qld` does once the link is over, and which [`Stderr`]'s
//! `Drop` does as a backstop.

use std::fmt;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};

/// Number of errors [`Stderr`] reports before it stops, when
/// [`LinkOptions::error_limit`](crate::args::LinkOptions::error_limit) says
/// nothing. lld's default, and its wording for the message that replaces the
/// rest.
pub const DEFAULT_ERROR_LIMIT: u64 = 20;

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

impl Severity {
    /// The ANSI colour lld gives this severity: red, magenta, cyan.
    const fn color(self) -> &'static str {
        match self {
            Self::Note => "\x1b[0;36m",
            Self::Warning => "\x1b[0;35m",
            Self::Error => "\x1b[0;31m",
        }
    }
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

/// How a [`Location`] relates to the diagnostic it hangs under: the words
/// before it on the `>>>` line.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Relation {
    /// `>>> referenced by`: the place that refers to the symbol or section.
    #[default]
    ReferencedBy,
    /// `>>> defined at`: a definition, with the place inside the file.
    DefinedAt,
    /// `>>> defined in`: a definition whose place inside the file is not
    /// known, typically in a shared library.
    DefinedIn,
    /// `>>>` with no words: the location speaks for itself.
    Bare,
}

impl Relation {
    /// The words before the location, without the trailing space.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::ReferencedBy => "referenced by",
            Self::DefinedAt => "defined at",
            Self::DefinedIn => "defined in",
            Self::Bare => "",
        }
    }
}

/// Where a diagnostic points, in the input.
#[derive(Clone, Debug, Default)]
pub struct Location {
    /// How this location relates to the diagnostic.
    pub relation: Relation,
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

/// A source position recovered from debug information or a linker script.
#[derive(Clone, Debug)]
pub struct SourceLocation {
    /// Source file path as recorded by the compiler.
    pub file: String,
    /// 1-based line number.
    pub line: u32,
    /// 1-based column, when the producer recorded one.
    pub column: Option<u32>,
}

impl SourceLocation {
    /// Creates a position with no column.
    #[must_use]
    pub fn new(file: impl Into<String>, line: u32) -> Self {
        Self {
            file: file.into(),
            line,
            column: None,
        }
    }

    /// `file:line` or `file:line:column`, GNU's and Clang's form.
    #[must_use]
    pub fn display(&self) -> String {
        match self.column {
            Some(column) => format!("{}:{}:{column}", self.file, self.line),
            None => format!("{}:{}", self.file, self.line),
        }
    }

    /// `base.c:line[:column]`, the short form lld puts first.
    fn short(&self) -> String {
        let base = self
            .file
            .rsplit(['/', '\\'])
            .next()
            .filter(|base| !base.is_empty())
            .unwrap_or(&self.file);
        match self.column {
            Some(column) => format!("{base}:{}:{column}", self.line),
            None => format!("{base}:{}", self.line),
        }
    }
}

impl Location {
    /// Sets how this location relates to its diagnostic.
    #[must_use]
    pub fn relation(mut self, relation: Relation) -> Self {
        self.relation = relation;
        self
    }

    /// `file.o(member):(.text+0x12)`: the place in the input file, without
    /// the source position.
    #[must_use]
    pub fn object(&self) -> String {
        let mut text = self.file.display().to_string();
        if let Some(member) = &self.member {
            text.push_str(&format!("({member})"));
        }
        if let Some(section) = &self.section {
            text.push_str(&format!(":({section}"));
            if let Some(offset) = self.offset {
                text.push_str(&format!("+{offset:#x}"));
            }
            text.push(')');
        }
        text
    }

    /// `main.c:3 (/home/me/main.c:3)` when debug information gave a source
    /// position and it names a directory, `main.c:3` when it does not, and
    /// `None` when there is none.
    #[must_use]
    pub fn source_text(&self) -> Option<String> {
        let source = self.source.as_ref()?;
        let full = source.display();
        let short = source.short();
        Some(if short == full {
            full
        } else {
            format!("{short} ({full})")
        })
    }
}

impl fmt::Display for Location {
    /// Renders as `file.o(member):(.text+0x12)`, the form lld uses, prefixed
    /// with `source:line (` … `)` when a source position is known.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.source {
            Some(source) => write!(f, "{} ({})", source.display(), self.object()),
            None => f.write_str(&self.object()),
        }
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
    /// Context lines rendered under the message as `>>> {detail}`, the way
    /// lld writes them (`defined at a.o:(.text+0x0)`).
    pub details: Vec<String>,
    /// Extra notes, rendered under the message as `>>> note: {note}`.
    pub notes: Vec<String>,
    /// Sort key for deterministic output: usually the command-line position of
    /// the input file involved. A note that belongs to another diagnostic
    /// carries that diagnostic's key, and the stable sort keeps it under it.
    pub order: u64,
}

impl Diagnostic {
    /// Creates a diagnostic with no locations or notes.
    pub fn new(severity: Severity, message: impl Into<String>) -> Self {
        Self {
            severity,
            message: message.into(),
            locations: Vec::new(),
            details: Vec::new(),
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

    /// Adds a context line, rendered as `>>> {detail}`.
    #[must_use]
    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.details.push(detail.into());
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

    /// Renders the diagnostic the way lld does, prefixed with `program`.
    ///
    /// The severity is coloured when `color` is set. Every line after the
    /// first starts with `>>>`, and a location that has a source position
    /// takes two of them, the second aligned under the first's location:
    ///
    /// ```text
    /// qld: error: undefined symbol: foo
    /// >>> referenced by main.c:3 (/home/me/main.c:3)
    /// >>>               main.o:(.text+0x1d)
    /// ```
    ///
    /// A diagnostic with any `>>>` line ends with a blank line, as lld's do,
    /// so that a wall of them stays readable.
    #[must_use]
    pub fn render(&self, program: &str, color: bool) -> String {
        let mut text = if color {
            format!(
                "{program}: {}{}: \x1b[0m{}\n",
                self.severity.color(),
                self.severity,
                self.message
            )
        } else {
            format!("{program}: {}: {}\n", self.severity, self.message)
        };
        for location in &self.locations {
            let label = location.relation.label();
            match location.source_text() {
                Some(source) => {
                    if label.is_empty() {
                        text.push_str(&format!(">>> {source}\n>>> {}\n", location.object()));
                    } else {
                        text.push_str(&format!(">>> {label} {source}\n"));
                        // Align the object reference under the source one.
                        text.push_str(&format!(
                            ">>> {:width$} {}\n",
                            "",
                            location.object(),
                            width = label.len()
                        ));
                    }
                }
                None if label.is_empty() => {
                    text.push_str(&format!(">>> {}\n", location.object()));
                }
                None => text.push_str(&format!(">>> {label} {}\n", location.object())),
            }
        }
        for detail in &self.details {
            text.push_str(&format!(">>> {detail}\n"));
        }
        for note in &self.notes {
            text.push_str(&format!(">>> note: {note}\n"));
        }
        if !self.locations.is_empty() || !self.details.is_empty() || !self.notes.is_empty() {
            text.push('\n');
        }
        text
    }
}

/// Receives diagnostics during a link.
///
/// Implementations must be cheap to call from many threads at once.
pub trait DiagnosticSink: Send + Sync {
    /// Reports one diagnostic.
    fn emit(&self, diagnostic: Diagnostic);

    /// Number of diagnostics with [`Severity::Error`] seen so far. With
    /// `--fatal-warnings` a sink may count warnings here too.
    fn error_count(&self) -> usize;

    /// Renders anything the sink is holding back. A sink that renders as it
    /// goes, or that only stores diagnostics, does nothing.
    ///
    /// Calling it more than once is safe: everything already rendered is
    /// gone from the sink.
    fn flush(&self) {}
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
    /// order: sorted by [`Diagnostic::order`], stably, so notes stay under
    /// the diagnostic they were emitted with.
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

/// When [`Stderr`] colours its output.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Color {
    /// Colour when the target is a terminal.
    #[default]
    Auto,
    Always,
    Never,
}

impl Color {
    const fn code(self) -> u8 {
        match self {
            Self::Auto => 0,
            Self::Always => 1,
            Self::Never => 2,
        }
    }

    const fn from_code(code: u8) -> Self {
        match code {
            1 => Self::Always,
            2 => Self::Never,
            _ => Self::Auto,
        }
    }
}

/// Where [`Stderr`] writes.
enum Target {
    /// The process's standard error.
    Stderr,
    /// A writer supplied by the caller, for tests and for library users who
    /// want the CLI's rendering somewhere else.
    Writer(Mutex<Box<dyn Write + Send>>),
}

impl fmt::Debug for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stderr => f.write_str("Stderr"),
            Self::Writer(_) => f.write_str("Writer"),
        }
    }
}

/// A sink that writes GNU-style messages to stderr, in
/// [`Diagnostic::order`], when [`DiagnosticSink::flush`] is called.
///
/// This is the sink the `qld` binary uses. It honours `--color-diagnostics`,
/// `--error-limit`, `--fatal-warnings` and `-w`, which
/// [`configure`](Self::configure) takes from the parsed options.
#[derive(Debug)]
pub struct Stderr {
    program: String,
    target: Target,
    pending: Mutex<Vec<Diagnostic>>,
    errors: AtomicUsize,
    color: AtomicU8,
    /// `u64::MAX` means no limit.
    error_limit: AtomicU64,
    fatal_warnings: AtomicBool,
    no_warnings: AtomicBool,
}

impl Stderr {
    /// Creates a sink that prefixes messages with `program` and writes them
    /// to the process's standard error.
    #[must_use]
    pub fn new(program: impl Into<String>) -> Self {
        Self::with_target(program, Target::Stderr)
    }

    /// Creates a sink that renders exactly as [`new`](Self::new) does but
    /// writes to `writer`. Colour is off unless
    /// [`set_color`](Self::set_color) turns it on.
    pub fn with_writer(program: impl Into<String>, writer: impl Write + Send + 'static) -> Self {
        let sink = Self::with_target(program, Target::Writer(Mutex::new(Box::new(writer))));
        sink.color.store(Color::Never.code(), Ordering::Relaxed);
        sink
    }

    fn with_target(program: impl Into<String>, target: Target) -> Self {
        Self {
            program: program.into(),
            target,
            pending: Mutex::new(Vec::new()),
            errors: AtomicUsize::new(0),
            color: AtomicU8::new(Color::Auto.code()),
            error_limit: AtomicU64::new(DEFAULT_ERROR_LIMIT),
            fatal_warnings: AtomicBool::new(false),
            no_warnings: AtomicBool::new(false),
        }
    }

    /// Applies a link's diagnostic options: `--color-diagnostics`,
    /// `--error-limit`, `--fatal-warnings` and `-w`/`--no-warnings`.
    ///
    /// `-w` cancels `--fatal-warnings`, as it does in lld.
    pub fn configure(&self, options: &crate::args::LinkOptions) {
        self.set_color(match options.color {
            crate::args::ColorChoice::Always => Color::Always,
            crate::args::ColorChoice::Never => Color::Never,
            _ => Color::Auto,
        });
        self.set_error_limit(options.error_limit);
        self.no_warnings
            .store(options.no_warnings, Ordering::Relaxed);
        self.fatal_warnings.store(
            options.fatal_warnings && !options.no_warnings,
            Ordering::Relaxed,
        );
    }

    fn set_color(&self, color: Color) {
        self.color.store(color.code(), Ordering::Relaxed);
    }

    /// Sets how many errors are reported before the rest are replaced by one
    /// message, as `--error-limit` does. `Some(0)` and `None` both mean no
    /// limit; `None` is what [`configure`](Self::configure) sees when the
    /// option was not given, and it keeps [`DEFAULT_ERROR_LIMIT`].
    fn set_error_limit(&self, limit: Option<u64>) {
        let limit = match limit {
            None => DEFAULT_ERROR_LIMIT,
            Some(0) => u64::MAX,
            Some(limit) => limit,
        };
        self.error_limit.store(limit, Ordering::Relaxed);
    }

    /// Whether the output is coloured.
    fn colored(&self) -> bool {
        match Color::from_code(self.color.load(Ordering::Relaxed)) {
            Color::Always => true,
            Color::Never => false,
            Color::Auto => match self.target {
                Target::Stderr => std::io::stderr().is_terminal(),
                Target::Writer(_) => false,
            },
        }
    }

    /// Renders everything buffered so far and empties the buffer.
    ///
    /// The diagnostics are sorted by [`Diagnostic::order`] with a stable
    /// sort, then truncated to `--error-limit` errors.
    fn take_rendered(&self) -> String {
        let mut pending = std::mem::take(&mut *self.pending.lock().expect("poisoned"));
        pending.sort_by_key(|diagnostic| diagnostic.order);
        let color = self.colored();
        let limit = self.error_limit.load(Ordering::Relaxed);
        let mut text = String::new();
        let mut errors = 0u64;
        for diagnostic in &pending {
            if diagnostic.severity == Severity::Error {
                if errors >= limit {
                    // lld's wording, and its behaviour: nothing after the
                    // limit is shown, not even warnings.
                    let stop = Diagnostic::error(
                        "too many errors emitted, stopping now (use --error-limit=0 to see all \
                         errors)",
                    );
                    text.push_str(&stop.render(&self.program, color));
                    break;
                }
                errors = errors.saturating_add(1);
            }
            text.push_str(&diagnostic.render(&self.program, color));
        }
        text
    }

    /// Writes `text` wherever this sink points.
    fn write(&self, text: &str) {
        if text.is_empty() {
            return;
        }
        match &self.target {
            Target::Stderr => {
                let stderr = std::io::stderr();
                let mut lock = stderr.lock();
                let _ = lock.write_all(text.as_bytes());
                let _ = lock.flush();
            }
            Target::Writer(writer) => {
                if let Ok(mut writer) = writer.lock() {
                    let _ = writer.write_all(text.as_bytes());
                    let _ = writer.flush();
                }
            }
        }
    }
}

impl DiagnosticSink for Stderr {
    fn emit(&self, mut diagnostic: Diagnostic) {
        if diagnostic.severity == Severity::Warning {
            if self.no_warnings.load(Ordering::Relaxed) {
                return;
            }
            if self.fatal_warnings.load(Ordering::Relaxed) {
                diagnostic.severity = Severity::Error;
            }
        }
        if diagnostic.severity == Severity::Error {
            self.errors.fetch_add(1, Ordering::Relaxed);
        }
        self.pending.lock().expect("poisoned").push(diagnostic);
    }

    fn error_count(&self) -> usize {
        self.errors.load(Ordering::Relaxed)
    }

    fn flush(&self) {
        let text = self.take_rendered();
        self.write(&text);
    }
}

impl Drop for Stderr {
    /// Renders anything a caller did not [`flush`](DiagnosticSink::flush),
    /// so that no diagnostic is ever lost.
    fn drop(&mut self) {
        self.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `Stderr` writing into a buffer the test can read back.
    #[derive(Clone, Default)]
    struct Buffer(std::sync::Arc<Mutex<Vec<u8>>>);

    impl Write for Buffer {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("poisoned").extend_from_slice(data);
            Ok(data.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Buffer {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().expect("poisoned")).into_owned()
        }
    }

    fn buffered() -> (Stderr, Buffer) {
        let buffer = Buffer::default();
        (Stderr::with_writer("qld", buffer.clone()), buffer)
    }

    fn referenced(file: &str, section: &str, offset: u64) -> Location {
        Location {
            file: PathBuf::from(file),
            section: Some(section.into()),
            offset: Some(offset),
            ..Location::default()
        }
    }

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
        let location = referenced("main.o", ".text", 0x1a);
        assert_eq!(location.to_string(), "main.o:(.text+0x1a)");
        assert_eq!(location.object(), "main.o:(.text+0x1a)");
        assert!(location.source_text().is_none());
    }

    #[test]
    fn location_renders_archive_members_and_sources() {
        let mut location = referenced("libc.a", ".text", 0);
        location.member = Some("printf.o".into());
        assert_eq!(location.object(), "libc.a(printf.o):(.text+0x0)");
        location.source = Some(SourceLocation {
            file: "/home/me/printf.c".into(),
            line: 12,
            column: Some(3),
        });
        assert_eq!(
            location.source_text().as_deref(),
            Some("printf.c:12:3 (/home/me/printf.c:12:3)")
        );
        // A bare file name is not repeated.
        location.source = Some(SourceLocation::new("printf.c", 12));
        assert_eq!(location.source_text().as_deref(), Some("printf.c:12"));
    }

    #[test]
    fn relations_are_not_all_referenced_by() {
        let rendered = Diagnostic::error("duplicate symbol: dup")
            .at(referenced("a.o", ".text", 0).relation(Relation::DefinedAt))
            .at(referenced("b.o", ".text", 0).relation(Relation::DefinedAt))
            .render("qld", false);
        assert_eq!(
            rendered,
            "qld: error: duplicate symbol: dup\n\
             >>> defined at a.o:(.text+0x0)\n\
             >>> defined at b.o:(.text+0x0)\n\n"
        );
    }

    #[test]
    fn source_line_gets_its_own_aligned_line() {
        let mut location = referenced("main.o", ".text", 0x1d);
        location.source = Some(SourceLocation::new("/home/me/main.c", 3));
        let rendered = Diagnostic::error("undefined symbol: foo")
            .at(location)
            .render("qld", false);
        assert_eq!(
            rendered,
            "qld: error: undefined symbol: foo\n\
             >>> referenced by main.c:3 (/home/me/main.c:3)\n\
             >>>               main.o:(.text+0x1d)\n\n"
        );
    }

    #[test]
    fn stderr_sorts_and_flushes_once() {
        let (sink, buffer) = buffered();
        sink.emit(Diagnostic::error("second").order(2));
        sink.emit(Diagnostic::error("first").order(1));
        assert_eq!(buffer.text(), "", "nothing is written before the flush");
        sink.flush();
        assert_eq!(buffer.text(), "qld: error: first\nqld: error: second\n");
        sink.flush();
        assert_eq!(buffer.text(), "qld: error: first\nqld: error: second\n");
    }

    #[test]
    fn notes_stay_under_their_parent() {
        let (sink, buffer) = buffered();
        sink.emit(Diagnostic::error("undefined symbol: b").order(7));
        sink.emit(Diagnostic::new(Severity::Note, "b is in libb.so").order(7));
        sink.emit(Diagnostic::error("undefined symbol: a").order(3));
        sink.flush();
        assert_eq!(
            buffer.text(),
            "qld: error: undefined symbol: a\n\
             qld: error: undefined symbol: b\n\
             qld: note: b is in libb.so\n"
        );
    }

    #[test]
    fn error_limit_truncates() {
        let (sink, buffer) = buffered();
        sink.set_error_limit(Some(2));
        for index in 0..5u64 {
            sink.emit(Diagnostic::error(format!("problem {index}")).order(index));
        }
        sink.flush();
        assert_eq!(
            buffer.text(),
            "qld: error: problem 0\n\
             qld: error: problem 1\n\
             qld: error: too many errors emitted, stopping now (use --error-limit=0 to see all \
             errors)\n"
        );
        assert_eq!(sink.error_count(), 5, "every error is still counted");
    }

    #[test]
    fn no_limit_shows_everything() {
        let (sink, buffer) = buffered();
        sink.set_error_limit(Some(0));
        for index in 0..64u64 {
            sink.emit(Diagnostic::error("problem").order(index));
        }
        sink.flush();
        assert_eq!(buffer.text().lines().count(), 64);
    }

    #[test]
    fn fatal_warnings_promote_and_no_warnings_suppress() {
        let (sink, buffer) = buffered();
        sink.fatal_warnings.store(true, Ordering::Relaxed);
        sink.emit(Diagnostic::warning("textrel"));
        sink.flush();
        assert_eq!(buffer.text(), "qld: error: textrel\n");
        assert_eq!(sink.error_count(), 1);

        let (sink, buffer) = buffered();
        sink.no_warnings.store(true, Ordering::Relaxed);
        sink.emit(Diagnostic::warning("textrel"));
        sink.flush();
        assert_eq!(buffer.text(), "");
    }

    #[test]
    fn color_matches_lld() {
        let (sink, buffer) = buffered();
        sink.set_color(Color::Always);
        sink.emit(Diagnostic::error("undefined symbol: foo"));
        sink.flush();
        assert_eq!(
            buffer.text(),
            "qld: \x1b[0;31merror: \x1b[0mundefined symbol: foo\n"
        );
    }

    #[test]
    fn drop_flushes() {
        let buffer = Buffer::default();
        {
            let sink = Stderr::with_writer("qld", buffer.clone());
            sink.emit(Diagnostic::error("lost otherwise"));
        }
        assert_eq!(buffer.text(), "qld: error: lost otherwise\n");
    }
}
