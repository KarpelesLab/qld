//! Receives diagnostics through a custom [`DiagnosticSink`], here one that
//! renders them as JSON lines for an IDE or a build system.
//!
//! ```sh
//! cargo run --example custom_sink
//! ```
//!
//! The link below has an undefined symbol, so it fails with
//! [`qld::Error::Reported`] after reporting the error to the sink. Stages
//! report from many threads at once, so a sink must be `Send + Sync`; to
//! print in a deterministic order, collect first and sort by
//! [`Diagnostic::order`], as this sink does.

#[path = "support/objects.rs"]
mod objects;

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use qld::args::{InputAttrs, InputKind, LinkOptions, OutputBuffer, OutputKind};
use qld::diag::{Diagnostic, DiagnosticSink, Severity};

/// Collects diagnostics and renders them as JSON lines.
#[derive(Default)]
struct JsonLines {
    diagnostics: Mutex<Vec<Diagnostic>>,
    errors: AtomicUsize,
}

impl DiagnosticSink for JsonLines {
    fn emit(&self, diagnostic: Diagnostic) {
        if diagnostic.severity == Severity::Error {
            self.errors.fetch_add(1, Ordering::Relaxed);
        }
        if let Ok(mut diagnostics) = self.diagnostics.lock() {
            diagnostics.push(diagnostic);
        }
    }

    fn error_count(&self) -> usize {
        self.errors.load(Ordering::Relaxed)
    }
}

impl JsonLines {
    /// The diagnostics so far, one JSON object per line, in a deterministic
    /// order.
    fn render(&self) -> String {
        let mut diagnostics = match self.diagnostics.lock() {
            Ok(mut diagnostics) => std::mem::take(&mut *diagnostics),
            Err(_) => return String::new(),
        };
        diagnostics.sort_by(|a, b| a.order.cmp(&b.order).then(a.message.cmp(&b.message)));
        let mut out = String::new();
        for diagnostic in diagnostics {
            let locations: Vec<String> = diagnostic
                .locations
                .iter()
                .map(|location| quote(&location.to_string()))
                .collect();
            let details: Vec<String> = diagnostic.details.iter().map(|d| quote(d)).collect();
            out.push_str(&format!(
                "{{\"severity\":{},\"message\":{},\"locations\":[{}],\"details\":[{}]}}\n",
                quote(&diagnostic.severity.to_string()),
                quote(&diagnostic.message),
                locations.join(","),
                details.join(","),
            ));
        }
        out
    }
}

/// A JSON string literal.
fn quote(text: &str) -> String {
    let mut out = String::from('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if u32::from(c) < 0x20 => out.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn main() {
    // `main.o` calls `answer`, which nothing defines.
    let mut options = LinkOptions::new();
    options.kind = OutputKind::StaticExecutable;
    options.output_buffer = Some(OutputBuffer::new());
    options.push_input(
        InputKind::bytes("main.o", objects::main_object()),
        InputAttrs::default(),
    );

    let sink = JsonLines::default();
    let result = qld::link(&options, &sink);
    print!("{}", sink.render());
    match result {
        Ok(()) => println!("linked"),
        Err(qld::Error::Reported { errors }) => {
            println!("the link failed with {errors} error(s) reported above");
        }
        Err(error) => println!("the link failed: {error}"),
    }
}
