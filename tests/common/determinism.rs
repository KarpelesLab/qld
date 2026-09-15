//! The determinism harness: relink the same inputs with different thread
//! counts and require byte-identical outputs.
//!
//! Used by fixtures with `determinism = true`. Linkers without `--threads`
//! (GNU ld, gold) are relinked once without it, which still checks that the
//! harness and the fixture are stable. qld is also relinked with every
//! output backing (`QLD_OUTPUT_BACKING`), which must not change a byte, and
//! with `--build-id=sha1` under every backing, compared with each other.

use std::fmt::Write as _;
use std::path::Path;

use super::fixture::{self, Fixture, LinkError, Linker, Log, Status, TargetEnv};

/// Thread counts to link with: 1, 2 and N, where N is the available
/// parallelism (at least 4, so the check is meaningful on small machines).
pub fn thread_counts() -> Vec<usize> {
    let n = std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .max(4);
    vec![1, 2, n]
}

/// Values of `QLD_OUTPUT_BACKING` relinked with qld; the first is compared
/// with the others for the build-id variants.
const BACKINGS: [&str; 3] = ["write", "mmap", "memory"];

/// One determinism relink.
struct Variant {
    label: String,
    extra: Vec<String>,
    vars: Vec<(String, String)>,
    /// The variant whose outputs this one must match, instead of the first
    /// link's.
    compare_with: Option<&'static str>,
}

impl Variant {
    fn new(label: String, extra: &[&str]) -> Self {
        Self {
            label,
            extra: extra.iter().map(|s| (*s).to_string()).collect(),
            vars: Vec::new(),
            compare_with: None,
        }
    }

    fn var(mut self, backing: &str) -> Self {
        self.vars
            .push(("QLD_OUTPUT_BACKING".to_string(), backing.to_string()));
        self
    }

    fn compare_with(mut self, label: &'static str) -> Self {
        self.compare_with = Some(label);
        self
    }
}

/// Compares the files named in `outputs` between two directories.
pub fn compare_outputs(first: &Path, second: &Path, outputs: &[String]) -> Result<(), String> {
    let mut problems = String::new();
    for output in outputs {
        let read = |dir: &Path| {
            std::fs::read(dir.join(output))
                .map_err(|e| format!("{}: {e}", dir.join(output).display()))
        };
        let (a, b) = (read(first)?, read(second)?);
        if a != b {
            let offset = a
                .iter()
                .zip(&b)
                .position(|(x, y)| x != y)
                .unwrap_or(a.len().min(b.len()));
            let _ = writeln!(
                problems,
                "{output}: {} is {} bytes, {} is {} bytes, first difference at offset {offset:#x}",
                first.display(),
                a.len(),
                second.display(),
                b.len()
            );
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems)
    }
}

/// Relinks `pristine` (the compiled, not yet linked fixture) once per thread
/// count and compares each output with the one in `reference`.
///
/// Returns a short summary for the pass note.
pub fn check(
    fixture: &Fixture,
    env: &TargetEnv,
    linker: &Linker,
    pristine: &Path,
    reference: &Path,
    scratch: &Path,
    log: &mut Log,
) -> Result<String, Status> {
    let outputs = fixture.link_outputs();
    let mut variants: Vec<Variant> = if linker.supports_threads() {
        thread_counts()
            .into_iter()
            .map(|n| Variant::new(format!("threads-{n}"), &[&format!("--threads={n}")]))
            .collect()
    } else {
        vec![Variant::new("relink".to_string(), &[])]
    };
    if *linker == Linker::Qld {
        // Every output backing writes the same bytes, with and without a
        // build-id (hashed while writing with `write`, over the finished
        // image with the others).
        for backing in BACKINGS {
            variants.push(Variant::new(format!("backing-{backing}"), &[]).var(backing));
        }
        for backing in BACKINGS {
            variants.push(
                Variant::new(format!("build-id-{backing}"), &["--build-id=sha1"])
                    .var(backing)
                    .compare_with("build-id-write"),
            );
        }
    }
    let mut problems = String::new();
    for variant in &variants {
        let Variant {
            label,
            extra,
            vars,
            compare_with,
        } = variant;
        let dir = scratch.join(label);
        fixture::fresh_dir(&dir)?;
        fixture::copy_dir(pristine, &dir)
            .map_err(|e| Status::Fail(format!("cannot copy {}: {e}", pristine.display())))?;
        let reference = match compare_with {
            Some(other) => scratch.join(other),
            None => reference.to_path_buf(),
        };
        let reference = reference.as_path();
        match fixture::link_with_vars(fixture, env, linker, &dir, extra, vars, log) {
            Ok(()) => {}
            Err(LinkError::Unimplemented(message)) => return Err(Status::skip(message)),
            Err(LinkError::Failed(message)) => {
                return Err(Status::Fail(format!(
                    "determinism relink ({label}) failed: {message}"
                )));
            }
            Err(LinkError::Status(status)) => return Err(status),
        }
        if reference == dir {
            continue;
        }
        if let Err(message) = compare_outputs(reference, &dir, &outputs) {
            let _ = writeln!(problems, "output differs with {label}:\n{message}");
        }
    }
    if problems.is_empty() {
        let labels: Vec<&str> = variants.iter().map(|v| v.label.as_str()).collect();
        Ok(format!("deterministic across {}", labels.join(", ")))
    } else {
        Err(Status::Fail(format!(
            "non-deterministic output:\n{problems}"
        )))
    }
}
