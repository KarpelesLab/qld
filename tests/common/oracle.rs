//! Version gating for reference linkers ("oracles").
//!
//! Some comparison tests pin the output of a particular lld release: lld
//! changes what it relaxes and what it accepts between releases, so an older
//! `ld.lld` produces different (not wrong) code, or rejects the link. Such a
//! test calls [`lld_at_least`] with the oldest release whose behaviour it
//! encodes; with an older `ld.lld` it prints
//!
//! ```text
//! <test>: skipped: needs ld.lld >= 22, found 20.1.8 (/usr/bin/ld.lld)
//! ```
//!
//! to standard error and returns early instead of failing. A version that
//! cannot be read is not a reason to skip: the test runs and shows what the
//! oracle did. Under `QLD_REQUIRE_TOOLS=1` a too-old oracle fails the test
//! instead, so a CI job that pins a version cannot silently stop comparing.
//!
//! The minimum versions, and why each test needs it, are listed in
//! `docs/testing.md`.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};

/// A `major.minor.patch` release number.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

impl Version {
    /// Parses `20`, `20.1` or `20.1.8`, ignoring a suffix such as `-rc1` or
    /// `git` on the last component.
    pub fn parse(text: &str) -> Option<Self> {
        let mut parts = text.split('.');
        let mut number = |required: bool| -> Option<u32> {
            match parts.next() {
                Some(part) => {
                    let digits: String = part.chars().take_while(char::is_ascii_digit).collect();
                    digits.parse().ok()
                }
                None if required => None,
                None => Some(0),
            }
        };
        let major = number(true)?;
        let minor = number(false)?;
        let patch = number(false)?;
        Some(Self {
            major,
            minor,
            patch,
        })
    }
}

/// Extracts the release from `ld.lld --version` output, such as
/// `LLD 20.1.8 (compatible with GNU linkers)`,
/// `Ubuntu LLD 22.1.0 (compatible with GNU linkers)` or
/// `Homebrew LLD 21.1.2 (compatible with GNU linkers)`.
pub fn parse_lld_version(text: &str) -> Option<Version> {
    let line = text.lines().find(|l| l.contains("LLD "))?;
    let mut words = line.split_whitespace();
    words.find(|w| *w == "LLD")?;
    Version::parse(words.next()?)
}

/// The release of the `ld.lld` at `path`, asked once per path and process.
pub fn lld_version(path: &Path) -> Option<Version> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, Option<Version>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    // A test that panicked while holding the lock does not make the cache
    // unusable: its entries are complete or absent.
    let mut cache = cache.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(known) = cache.get(path) {
        return *known;
    }
    let version = Command::new(path)
        .arg("--version")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| parse_lld_version(&String::from_utf8_lossy(&o.stdout)));
    cache.insert(path.to_path_buf(), version);
    version
}

/// Whether the `ld.lld` at `path` is release `min_major` or newer. When it
/// is older, prints why `test` is skipped and returns `false` (or panics
/// under `QLD_REQUIRE_TOOLS`); the caller then returns.
pub fn lld_at_least(path: &Path, min_major: u32, test: &str) -> bool {
    let Some(found) = lld_version(path) else {
        // Unknown: compare anyway rather than hide a real difference.
        return true;
    };
    if found.major >= min_major {
        return true;
    }
    let message = format!(
        "{test}: skipped: needs ld.lld >= {min_major}, found {found} ({})",
        path.display()
    );
    assert!(!super::tools::tools_required(), "{message}");
    eprintln!("{message}");
    false
}
