//! The determinism harness: relink the same inputs with different thread
//! counts and require byte-identical outputs.
//!
//! Used by fixtures with `determinism = true`. Linkers without `--threads`
//! (GNU ld, gold) are relinked once without it, which still checks that the
//! harness and the fixture are stable.

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
    let variants: Vec<(String, Vec<String>)> = if linker.supports_threads() {
        thread_counts()
            .into_iter()
            .map(|n| (format!("threads-{n}"), vec![format!("--threads={n}")]))
            .collect()
    } else {
        vec![("relink".to_string(), Vec::new())]
    };
    let mut problems = String::new();
    for (label, extra) in &variants {
        let dir = scratch.join(label);
        fixture::fresh_dir(&dir)?;
        fixture::copy_dir(pristine, &dir)
            .map_err(|e| Status::Fail(format!("cannot copy {}: {e}", pristine.display())))?;
        match fixture::link(fixture, env, linker, &dir, extra, log) {
            Ok(()) => {}
            Err(LinkError::Unimplemented(message)) => return Err(Status::skip(message)),
            Err(LinkError::Failed(message)) => {
                return Err(Status::Fail(format!(
                    "determinism relink ({label}) failed: {message}"
                )));
            }
            Err(LinkError::Status(status)) => return Err(status),
        }
        if let Err(message) = compare_outputs(reference, &dir, &outputs) {
            let _ = writeln!(
                problems,
                "output differs with {}:\n{message}",
                extra.join(" ")
            );
        }
    }
    if problems.is_empty() {
        let labels: Vec<&str> = variants.iter().map(|(label, _)| label.as_str()).collect();
        Ok(format!("deterministic across {}", labels.join(", ")))
    } else {
        Err(Status::Fail(format!(
            "non-deterministic output:\n{problems}"
        )))
    }
}
