//! Race-safe scratch directories for integration tests.
//!
//! [`scratch_dir`] returns a new, empty directory on every call:
//! `$CARGO_TARGET_TMPDIR/<suite>/<name>-<pid>-<seq>`, where `<seq>` counts
//! calls in this process. Two tests that pass the same name, in one process
//! or in two (`cargo nextest`, two `cargo test` runs sharing a target
//! directory), never share or delete each other's directory, and a test
//! that asks twice gets two directories.
//!
//! Nothing is removed when a test ends, so the files of a failed test stay
//! there to be inspected (`ls -t target/tmp/<suite>` lists the newest
//! first). Directories of earlier runs are pruned the first time a process
//! asks for a directory in that suite: an entry is removed only when its
//! name carries another process's id, it has not been modified for
//! [`STALE_AFTER`], and, on Linux, that process no longer exists. Entries
//! without a process id (left by the helpers this replaced) are removed
//! once they are older than [`STALE_AFTER`].
//!
//! A suite directory belongs to this module: nothing else may put files
//! there.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

/// How long a directory of another process is kept after its last change.
pub const STALE_AFTER: Duration = Duration::from_secs(10 * 60);

/// Returns a fresh, empty directory for test `name` of `suite`. `suite`
/// may have several components (`qld-tests/debug`); a `/` in `name`
/// becomes `-`, so every directory sits directly under the suite.
pub fn scratch_dir(suite: &str, name: &str) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(suite);
    prune_once(&root);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let name = name.replace(['/', '\\'], "-");
    let dir = root.join(format!("{name}-{}-{seq}", std::process::id()));
    // The name is unique, so a leftover can only come from a process with
    // the same id long ago; start empty regardless.
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)
        .unwrap_or_else(|e| panic!("cannot create {}: {e}", dir.display()));
    dir
}

/// Prunes `root` the first time this process uses it.
fn prune_once(root: &Path) {
    static DONE: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
    let mut done = DONE.lock().unwrap_or_else(|e| e.into_inner());
    if done.iter().any(|d| d == root) {
        return;
    }
    done.push(root.to_path_buf());
    // Pruning walks only this suite's directory; the lock is held so that
    // two threads of this process do not prune it twice.
    prune(root, std::process::id());
}

/// Removes the stale directories of other processes under `root`.
fn prune(root: &Path, own_pid: u32) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let stale = match owner(&entry.file_name().to_string_lossy()) {
            Some(pid) => pid != own_pid && is_old(&path) && !process_exists(pid),
            // Made before scratch directories were named this way.
            None => is_old(&path),
        };
        if stale {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

/// The process id in a `<name>-<pid>-<seq>` directory name.
fn owner(name: &str) -> Option<u32> {
    let mut parts = name.rsplitn(3, '-');
    let seq = parts.next()?;
    let pid = parts.next()?;
    let stem = parts.next()?;
    if stem.is_empty() || seq.is_empty() || !seq.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    pid.parse().ok()
}

/// Whether `path` was last modified [`STALE_AFTER`] ago or more.
fn is_old(path: &Path) -> bool {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .is_some_and(|age| age >= STALE_AFTER)
}

/// Whether process `pid` is running. Only Linux can tell cheaply without
/// `libc`; elsewhere the age alone decides.
fn process_exists(pid: u32) -> bool {
    cfg!(target_os = "linux") && Path::new("/proc").join(pid.to_string()).exists()
}
