//! Discovery of the host tools that integration tests drive: compilers,
//! `readelf`, reference linkers, `qemu-user`.
//!
//! Every tool can be overridden with an environment variable (an empty value
//! means "pretend it is not installed"):
//!
//! | Variable | Tool | Default search |
//! | --- | --- | --- |
//! | `QLD_TEST_CC` | C compiler driver | `cc`, `gcc`, `clang` |
//! | `QLD_TEST_CXX` | C++ compiler driver | `c++`, `g++`, `clang++` |
//! | `QLD_TEST_AR` | archiver | `ar`, `llvm-ar` |
//! | `QLD_TEST_READELF` | readelf | `readelf`, `llvm-readelf` |
//! | `QLD_TEST_GNU_LD` | GNU ld (BFD) | `ld.bfd`, then `ld` if it reports "GNU ld" |
//! | `QLD_TEST_LLD` | lld | `ld.lld`, `lld` |
//! | `QLD_TEST_MOLD` | mold | `mold` |
//! | `QLD_TEST_GOLD` | gold | `ld.gold`, `gold` |
//!
//! A test that needs a missing tool reports `SKIPPED: <reason>` and returns.

use std::ffi::OsStr;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

/// Prints the standard skip line. Tests call this and then return.
pub fn skip(reason: impl fmt::Display) {
    println!("SKIPPED: {reason}");
}

/// Whether `QLD_REQUIRE_TOOLS` asks for missing tools to be failures.
pub fn tools_required() -> bool {
    std::env::var_os("QLD_REQUIRE_TOOLS").is_some_and(|v| !v.is_empty() && v != "0")
}

/// Searches `PATH` for an executable. A name containing a path separator is
/// checked as given.
pub fn find_program(name: impl AsRef<OsStr>) -> Option<PathBuf> {
    let name = Path::new(name.as_ref());
    if name.as_os_str().is_empty() {
        return None;
    }
    if name.components().count() > 1 {
        return is_executable(name).then(|| name.to_path_buf());
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if is_executable(&candidate) {
            return Some(candidate);
        }
        if cfg!(windows) {
            let candidate = candidate.with_extension("exe");
            if is_executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

/// Returns the first of `names` found in `PATH`.
pub fn find_first(names: &[&str]) -> Option<PathBuf> {
    names.iter().find_map(find_program)
}

/// Looks up the override variable `var`, falling back to searching `names`.
pub fn find_with_override(var: &str, names: &[&str]) -> Option<PathBuf> {
    match std::env::var_os(var) {
        Some(value) if value.is_empty() => None,
        Some(value) => find_program(value),
        None => find_first(names),
    }
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// Runs `program args` and returns its stdout, or `None` if it did not run
/// successfully.
pub fn capture(program: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new(program)
        .args(args)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// A normalized target triple: `arch-os[-env]`, without the vendor field.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Triple {
    /// CPU architecture, canonicalized (`x86_64`, `aarch64`, `riscv64`, …).
    pub arch: String,
    /// Operating system (`linux`, `darwin`, `windows`, …).
    pub os: String,
    /// Environment / ABI (`gnu`, `musl`, `gnueabihf`), possibly empty.
    pub env: String,
}

impl Triple {
    /// Parses `x86_64-pc-linux-gnu`, `x86_64-linux-gnu`, `aarch64-unknown-linux-musl`,
    /// `arm64-apple-darwin23.0.0` and similar spellings.
    pub fn parse(text: &str) -> Option<Self> {
        let parts: Vec<&str> = text.trim().split('-').filter(|p| !p.is_empty()).collect();
        let arch = match *parts.first()? {
            "amd64" => "x86_64",
            "arm64" => "aarch64",
            "i386" | "i486" | "i586" => "i686",
            other => other,
        }
        .to_string();
        let known_os = [
            "linux", "darwin", "windows", "freebsd", "netbsd", "openbsd", "none",
        ];
        let os_index = parts
            .iter()
            .enumerate()
            .skip(1)
            .find(|(_, p)| known_os.iter().any(|os| p.starts_with(os)))
            .map(|(i, _)| i);
        let (os, env) = match os_index {
            Some(i) => {
                let os = known_os
                    .iter()
                    .find(|os| parts[i].starts_with(*os))
                    .copied()
                    .unwrap_or("unknown");
                (os.to_string(), parts[i + 1..].join("-"))
            }
            None => match parts.len() {
                1 => ("unknown".to_string(), String::new()),
                2 => (parts[1].to_string(), String::new()),
                _ => (parts[2].to_string(), parts[3..].join("-")),
            },
        };
        Some(Self { arch, os, env })
    }

    /// Whether this is a Linux target (and therefore produces ELF).
    pub fn is_linux(&self) -> bool {
        self.os == "linux"
    }

    /// Candidate cross-tool prefixes, in search order (`aarch64-linux-gnu-`,
    /// `aarch64-unknown-linux-gnu-`, …).
    pub fn tool_prefixes(&self) -> Vec<String> {
        let tail = if self.env.is_empty() {
            self.os.clone()
        } else {
            format!("{}-{}", self.os, self.env)
        };
        ["", "unknown-", "pc-", "none-"]
            .iter()
            .map(|vendor| format!("{}-{vendor}{tail}-", self.arch))
            .collect()
    }

    /// Finds a cross tool such as `gcc` or `ld.bfd` for this triple.
    pub fn cross_tool(&self, tool: &str) -> Option<PathBuf> {
        self.tool_prefixes()
            .iter()
            .find_map(|prefix| find_program(format!("{prefix}{tool}")))
    }

    /// The `qemu-user` architecture name for this triple.
    pub fn qemu_arch(&self) -> &str {
        match self.arch.as_str() {
            "i686" => "i386",
            a if a.starts_with("armv") => "arm",
            a => a,
        }
    }

    /// Finds `qemu-<arch>` (or `qemu-<arch>-static`).
    pub fn qemu(&self) -> Option<PathBuf> {
        let arch = self.qemu_arch();
        find_first(&[&format!("qemu-{arch}"), &format!("qemu-{arch}-static")])
    }

    /// A sysroot directory for `QEMU_LD_PREFIX`, if one is installed.
    pub fn qemu_sysroot(&self) -> Option<PathBuf> {
        self.tool_prefixes()
            .iter()
            .map(|prefix| PathBuf::from("/usr").join(prefix.trim_end_matches('-')))
            .find(|dir| dir.is_dir())
    }
}

impl fmt::Display for Triple {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.arch, self.os)?;
        if !self.env.is_empty() {
            write!(f, "-{}", self.env)?;
        }
        Ok(())
    }
}

/// The tools found on this machine. Discovered once per test binary.
#[derive(Debug)]
pub struct Tools {
    /// POSIX shell, used to run fixture `compile` and `run` commands.
    pub sh: Option<PathBuf>,
    /// C compiler driver.
    pub cc: Option<PathBuf>,
    /// C++ compiler driver.
    pub cxx: Option<PathBuf>,
    /// Archiver.
    pub ar: Option<PathBuf>,
    /// `readelf` (GNU preferred, `llvm-readelf` accepted).
    pub readelf: Option<PathBuf>,
    /// GNU ld (BFD).
    pub gnu_ld: Option<PathBuf>,
    /// lld (ELF).
    pub lld: Option<PathBuf>,
    /// mold.
    pub mold: Option<PathBuf>,
    /// gold.
    pub gold: Option<PathBuf>,
    /// The target the host C compiler produces code for (`cc -dumpmachine`).
    pub host: Option<Triple>,
}

/// Returns the tools found on this machine.
pub fn tools() -> &'static Tools {
    static TOOLS: OnceLock<Tools> = OnceLock::new();
    TOOLS.get_or_init(Tools::discover)
}

impl Tools {
    fn discover() -> Self {
        let cc = find_with_override("QLD_TEST_CC", &["cc", "gcc", "clang"]);
        let host = cc
            .as_deref()
            .and_then(|cc| capture(cc, &["-dumpmachine"]))
            .and_then(|text| Triple::parse(&text));
        let gnu_ld = match std::env::var_os("QLD_TEST_GNU_LD") {
            Some(value) if value.is_empty() => None,
            Some(value) => find_program(value),
            None => find_program("ld.bfd").or_else(|| {
                find_program("ld").filter(|ld| {
                    capture(ld, &["--version"]).is_some_and(|v| v.starts_with("GNU ld"))
                })
            }),
        };
        Self {
            sh: find_first(&["sh"]),
            cc,
            cxx: find_with_override("QLD_TEST_CXX", &["c++", "g++", "clang++"]),
            ar: find_with_override("QLD_TEST_AR", &["ar", "llvm-ar"]),
            readelf: find_with_override("QLD_TEST_READELF", &["readelf", "llvm-readelf"]),
            gnu_ld,
            lld: find_with_override("QLD_TEST_LLD", &["ld.lld", "lld"]),
            mold: find_with_override("QLD_TEST_MOLD", &["mold"]),
            gold: find_with_override("QLD_TEST_GOLD", &["ld.gold", "gold"]),
            host,
        }
    }
}

/// Whether a compiler driver is clang (which needs `--ld-path=` rather than
/// `-B` to pick a linker).
pub fn driver_is_clang(driver: &Path) -> bool {
    static CACHE: OnceLock<std::sync::Mutex<Vec<(PathBuf, bool)>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(&(_, known)) = cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|(path, _)| path == driver)
    {
        return known;
    }
    let is_clang = capture(driver, &["--version"]).is_some_and(|v| v.contains("clang"));
    cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((driver.to_path_buf(), is_clang));
    is_clang
}
