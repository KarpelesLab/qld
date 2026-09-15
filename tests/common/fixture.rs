//! The `test.toml` fixture format, and the steps shared by the fixture and
//! differential runners: prepare a scratch directory, compile, link, run,
//! check expectations.
//!
//! The format is documented for fixture authors in `tests/README.md`.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use super::process::{self, Output};
use super::textdiff;
use super::toml::{self, Value};
use super::tools::{self, Triple, tools};

/// Path of the `qld` binary under test.
pub fn qld_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_qld"))
}

/// Root of the fixture directories.
pub fn fixtures_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// Scratch directory for a suite, under Cargo's per-package temp directory
/// (`target/tmp`).
pub fn scratch_root(suite: &str) -> PathBuf {
    Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("qld-tests")
        .join(suite)
}

/// Whether the reference linker (GNU ld) is expected to accept the link.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GnuLdExpectation {
    /// GNU ld links this fixture like qld does (default).
    Pass,
    /// GNU ld rejects the link, and qld intentionally accepts it
    /// (see `docs/compatibility.md`).
    Fail,
}

/// One parsed `tests/fixtures/<name>/test.toml`.
#[derive(Clone, Debug)]
pub struct Fixture {
    /// Directory name.
    pub name: String,
    /// Fixture directory (sources and `test.toml`).
    pub dir: PathBuf,
    /// Free-form description.
    pub description: String,
    /// Shell commands producing the link inputs.
    pub compile: Vec<String>,
    /// Compiler driver the `link` lines are passed to, if not a raw link.
    pub driver: Option<String>,
    /// Link command lines, in order (linker arguments, or driver arguments
    /// when `driver` is set).
    pub links: Vec<String>,
    /// The main output (for `expect.readelf`); defaults to the last link's `-o`.
    pub output: Option<String>,
    /// Shell command that runs the result.
    pub run: Option<String>,
    /// Expected standard output of `run`.
    pub expect_stdout: Option<String>,
    /// Expected exit code of `run`.
    pub expect_exit: i64,
    /// Patterns for `readelf` output of the main output (`!` = must not).
    pub expect_readelf: Vec<String>,
    /// Patterns for other files: `expect.readelf_files."libfoo.so" = [...]`.
    pub expect_readelf_files: Vec<(String, Vec<String>)>,
    /// Arguments given to `readelf -W` for pattern checks (default `-a`).
    pub readelf_args: Vec<String>,
    /// Targets the fixture applies to (empty: the host, if it is Linux).
    pub targets: Vec<String>,
    /// Run the determinism check.
    pub determinism: bool,
    /// What GNU ld does with this fixture.
    pub gnu_ld: GnuLdExpectation,
    /// Differential runner: property lines to ignore (substring patterns).
    pub diff_ignore: Vec<String>,
    /// Differential runner: skip this fixture, with a reason.
    pub diff_skip: Option<String>,
    /// Skip this fixture everywhere, with a reason.
    pub skip: Option<String>,
    /// Skip the GNU ld comparison when GNU ld is older than this
    /// `major.minor`: linker behaviour changes between binutils releases, and
    /// a fixture pinning newer behaviour would otherwise fail the validation
    /// and differential runs on older distributions.
    pub gnu_ld_min_version: Option<(u32, u32)>,
    /// Skip unless at least one of these paths exists. `*` matches within one
    /// path component, so `/usr/lib/llvm*/lib*/LLVMgold.so` covers the
    /// different places distributions put a plugin.
    pub requires_files: Vec<String>,
    /// Timeout for each command.
    pub timeout: Duration,
}

impl Fixture {
    /// Loads and validates `dir/test.toml`.
    pub fn load(dir: &Path) -> Result<Self, String> {
        let name = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let path = dir.join("test.toml");
        let text =
            std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let entries = toml::parse(&text).map_err(|e| format!("{}: {e}", path.display()))?;

        let mut fixture = Fixture {
            name,
            dir: dir.to_path_buf(),
            description: String::new(),
            compile: Vec::new(),
            driver: None,
            links: Vec::new(),
            output: None,
            run: None,
            expect_stdout: None,
            expect_exit: 0,
            expect_readelf: Vec::new(),
            expect_readelf_files: Vec::new(),
            readelf_args: vec!["-a".to_string()],
            targets: Vec::new(),
            determinism: false,
            gnu_ld: GnuLdExpectation::Pass,
            diff_ignore: Vec::new(),
            diff_skip: None,
            skip: None,
            requires_files: Vec::new(),
            gnu_ld_min_version: None,
            timeout: Duration::from_secs(60),
        };

        for entry in &entries {
            let key: Vec<&str> = entry.key.iter().map(String::as_str).collect();
            let at = |message: String| {
                format!(
                    "{}:{}: `{}`: {message}",
                    path.display(),
                    entry.line,
                    entry.key_display()
                )
            };
            let string = || match &entry.value {
                Value::String(s) => Ok(s.clone()),
                other => Err(at(format!(
                    "expected a string, found {}",
                    other.type_name()
                ))),
            };
            let strings = || match &entry.value {
                Value::String(s) => Ok(vec![s.clone()]),
                Value::Array(items) => items
                    .iter()
                    .map(|item| match item {
                        Value::String(s) => Ok(s.clone()),
                        other => Err(at(format!(
                            "expected an array of strings, found an array containing a {}",
                            other.type_name()
                        ))),
                    })
                    .collect(),
                other => Err(at(format!(
                    "expected a string or an array of strings, found {}",
                    other.type_name()
                ))),
            };
            match key.as_slice() {
                ["description"] => fixture.description = string()?,
                ["compile"] => fixture.compile = strings()?,
                ["driver"] => fixture.driver = Some(string()?),
                ["link"] => fixture.links = strings()?,
                ["output"] => fixture.output = Some(string()?),
                ["run"] => fixture.run = Some(string()?),
                ["targets"] => fixture.targets = strings()?,
                ["skip"] => fixture.skip = Some(string()?),
                ["requires_files"] => fixture.requires_files = strings()?,
                ["gnu_ld_min_version"] => {
                    let text = string()?;
                    let mut parts = text.split('.');
                    let parsed = parts
                        .next()
                        .and_then(|major| major.parse().ok())
                        .zip(parts.next().and_then(|minor| minor.parse().ok()));
                    fixture.gnu_ld_min_version = Some(
                        parsed.ok_or_else(|| at(format!("expected major.minor, found {text}")))?,
                    );
                }
                ["determinism"] => match entry.value {
                    Value::Bool(b) => fixture.determinism = b,
                    ref other => {
                        return Err(at(format!(
                            "expected a boolean, found {}",
                            other.type_name()
                        )));
                    }
                },
                ["timeout"] => match entry.value {
                    Value::Integer(n) if n > 0 => {
                        fixture.timeout = Duration::from_secs(n.unsigned_abs());
                    }
                    _ => return Err(at("expected a positive number of seconds".to_string())),
                },
                ["gnu_ld"] => {
                    fixture.gnu_ld = match string()?.as_str() {
                        "pass" => GnuLdExpectation::Pass,
                        "fail" => GnuLdExpectation::Fail,
                        other => {
                            return Err(at(format!(
                                "expected \"pass\" or \"fail\", found {other:?}"
                            )));
                        }
                    }
                }
                ["expect", "stdout"] => fixture.expect_stdout = Some(string()?),
                ["expect", "exit"] => match entry.value {
                    Value::Integer(n) => fixture.expect_exit = n,
                    ref other => {
                        return Err(at(format!(
                            "expected an integer, found {}",
                            other.type_name()
                        )));
                    }
                },
                ["expect", "readelf"] => fixture.expect_readelf = strings()?,
                ["expect", "readelf_args"] => {
                    fixture.readelf_args = process::split_words(&string()?).map_err(at)?;
                }
                ["expect", "readelf_files", file] => {
                    fixture
                        .expect_readelf_files
                        .push(((*file).to_string(), strings()?));
                }
                ["diff", "ignore"] => fixture.diff_ignore = strings()?,
                ["diff", "skip"] => fixture.diff_skip = Some(string()?),
                _ => return Err(at("unknown key".to_string())),
            }
        }

        if fixture.links.is_empty() && fixture.skip.is_none() {
            return Err(format!("{}: `link` is required", path.display()));
        }
        for link in &fixture.links {
            process::split_words(link).map_err(|e| format!("{}: link: {e}", path.display()))?;
        }
        Ok(fixture)
    }

    /// Loads every fixture under `root`, sorted by name.
    pub fn load_all(root: &Path) -> Result<Vec<Self>, String> {
        let mut dirs: Vec<PathBuf> = std::fs::read_dir(root)
            .map_err(|e| format!("{}: {e}", root.display()))?
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .filter(|path| path.join("test.toml").is_file())
            .collect();
        dirs.sort();
        let mut fixtures = Vec::new();
        let mut errors = Vec::new();
        for dir in dirs {
            match Self::load(&dir) {
                Ok(fixture) => fixtures.push(fixture),
                Err(error) => errors.push(error),
            }
        }
        if errors.is_empty() {
            Ok(fixtures)
        } else {
            Err(errors.join("\n"))
        }
    }

    /// Output files named by `-o` in each link line, in link order.
    pub fn link_outputs(&self) -> Vec<String> {
        let mut outputs = Vec::new();
        for link in &self.links {
            let words = process::split_words(link).unwrap_or_default();
            let mut found = None;
            let mut iter = words.iter();
            while let Some(word) = iter.next() {
                if word == "-o" || word == "--output" {
                    found = iter.next().cloned();
                } else if let Some(rest) = word.strip_prefix("--output=") {
                    found = Some(rest.to_string());
                } else if let Some(rest) = word.strip_prefix("-o")
                    && !rest.is_empty()
                    && !word.starts_with("--")
                {
                    found = Some(rest.to_string());
                }
            }
            outputs.push(found.unwrap_or_else(|| "a.out".to_string()));
        }
        outputs
    }

    /// The file `expect.readelf` inspects.
    pub fn main_output(&self) -> String {
        self.output
            .clone()
            .or_else(|| self.link_outputs().pop())
            .unwrap_or_else(|| "a.out".to_string())
    }
}

/// Keeps the fixtures selected by `QLD_FIXTURE` (comma-separated substrings
/// of fixture names). Everything is selected when it is unset.
/// The `(major, minor)` version of a GNU binutils tool, from `--version`.
#[must_use]
pub fn binutils_version(tool: &Path) -> Option<(u32, u32)> {
    let output = std::process::Command::new(tool)
        .arg("--version")
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let first = text.lines().next()?;
    if !first.contains("GNU") {
        return None;
    }
    let version = first.split_whitespace().last()?;
    let mut parts = version.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts
        .next()?
        .trim_end_matches(|c: char| !c.is_ascii_digit())
        .parse()
        .ok()?;
    Some((major, minor))
}

/// Skips a fixture whose `gnu_ld_min_version` is newer than `ld`.
pub fn check_gnu_ld_version(fixture: &Fixture, ld: &Path) -> Result<(), Status> {
    let Some(wanted) = fixture.gnu_ld_min_version else {
        return Ok(());
    };
    match binutils_version(ld) {
        Some(have) if have >= wanted => Ok(()),
        Some((major, minor)) => Err(Status::skip(format!(
            "needs GNU ld {}.{} or newer, found {major}.{minor}",
            wanted.0, wanted.1
        ))),
        None => Err(Status::skip("cannot read GNU ld's version".to_string())),
    }
}

/// Skips the fixture unless one of its `requires_files` patterns matches an
/// existing path. `*` matches within a single path component, which is enough
/// for the version directories distributions use (`/usr/lib/llvm*/lib*/…`).
pub fn check_required_files(fixture: &Fixture) -> Result<(), Status> {
    if fixture.requires_files.is_empty() {
        return Ok(());
    }
    for pattern in &fixture.requires_files {
        if glob_exists(Path::new(pattern)) {
            return Ok(());
        }
    }
    Err(Status::skip(format!(
        "none of these exist: {}",
        fixture.requires_files.join(", ")
    )))
}

/// Whether any existing path matches `pattern` (components may contain `*`).
fn glob_exists(pattern: &Path) -> bool {
    let mut candidates = vec![PathBuf::new()];
    for component in pattern.components() {
        let part = component.as_os_str().to_string_lossy().into_owned();
        if !part.contains('*') {
            for candidate in &mut candidates {
                candidate.push(&part);
            }
            continue;
        }
        let mut next = Vec::new();
        for candidate in &candidates {
            let dir = if candidate.as_os_str().is_empty() {
                Path::new(".")
            } else {
                candidate.as_path()
            };
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if glob_matches(&part, &name) {
                    next.push(candidate.join(name));
                }
            }
        }
        if next.is_empty() {
            return false;
        }
        candidates = next;
    }
    candidates.iter().any(|path| path.exists())
}

/// `*` matches any run of characters inside one path component.
fn glob_matches(pattern: &str, name: &str) -> bool {
    let mut parts = pattern.split('*');
    let Some(first) = parts.next() else {
        return true;
    };
    let Some(mut rest) = name.strip_prefix(first) else {
        return false;
    };
    let mut last: Option<&str> = None;
    for part in parts {
        if let Some(previous) = last.replace(part)
            && !previous.is_empty()
        {
            match rest.find(previous) {
                Some(at) => rest = &rest[at + previous.len()..],
                None => return false,
            }
        }
    }
    match last {
        None => rest.is_empty(),
        Some(tail) => rest.len() >= tail.len() && rest.ends_with(tail),
    }
}

pub fn filter_from_env(fixtures: Vec<Fixture>) -> Vec<Fixture> {
    let Ok(filter) = std::env::var("QLD_FIXTURE") else {
        return fixtures;
    };
    let wanted: Vec<&str> = filter
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if wanted.is_empty() {
        return fixtures;
    }
    fixtures
        .into_iter()
        .filter(|f| wanted.iter().any(|w| f.name.contains(w)))
        .collect()
}

/// Outcome of one fixture job.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// Everything checked out. The note, if any, qualifies the pass.
    Pass(Option<String>),
    /// Not run.
    Skip {
        /// Why.
        reason: String,
        /// Whether a missing tool is the cause (`QLD_REQUIRE_TOOLS` turns
        /// these into failures).
        missing_tool: bool,
    },
    /// Something went wrong; the message is readable on its own.
    Fail(String),
}

impl Status {
    /// A skip caused by a missing tool.
    pub fn missing(what: impl Into<String>) -> Self {
        Self::Skip {
            reason: format!("{} not found", what.into()),
            missing_tool: true,
        }
    }

    /// A skip for any other reason.
    pub fn skip(reason: impl Into<String>) -> Self {
        Self::Skip {
            reason: reason.into(),
            missing_tool: false,
        }
    }
}

/// The linker a fixture is linked with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Linker {
    /// The qld binary built by this package.
    Qld,
    /// GNU ld (BFD).
    GnuLd,
    /// LLVM lld.
    Lld,
    /// mold.
    Mold,
    /// gold.
    Gold,
    /// Any other linker binary.
    Custom(PathBuf),
}

impl Linker {
    /// Parses a linker name: `qld`, `ld`/`bfd`/`gnu`, `lld`, `mold`, `gold`,
    /// or a path.
    pub fn parse(name: &str) -> Self {
        match name {
            "" | "qld" => Self::Qld,
            "ld" | "bfd" | "ld.bfd" | "gnu" => Self::GnuLd,
            "lld" | "ld.lld" => Self::Lld,
            "mold" => Self::Mold,
            "gold" | "ld.gold" => Self::Gold,
            other => Self::Custom(PathBuf::from(other)),
        }
    }

    /// Reads a linker choice from an environment variable, defaulting to qld.
    pub fn from_env(var: &str) -> Self {
        Self::parse(&std::env::var(var).unwrap_or_default())
    }

    /// Display name.
    pub fn name(&self) -> String {
        match self {
            Self::Qld => "qld".into(),
            Self::GnuLd => "GNU ld".into(),
            Self::Lld => "lld".into(),
            Self::Mold => "mold".into(),
            Self::Gold => "gold".into(),
            Self::Custom(path) => path.display().to_string(),
        }
    }

    /// Short name usable in a directory name.
    pub fn slug(&self) -> String {
        match self {
            Self::Qld => "qld".into(),
            Self::GnuLd => "bfd".into(),
            Self::Lld => "lld".into(),
            Self::Mold => "mold".into(),
            Self::Gold => "gold".into(),
            Self::Custom(_) => "custom".into(),
        }
    }

    /// Whether the linker takes `--threads=N`.
    pub fn supports_threads(&self) -> bool {
        matches!(self, Self::Qld | Self::Lld | Self::Mold)
    }

    /// Finds the linker binary for a target.
    pub fn binary(&self, target: &TargetEnv) -> Result<PathBuf, Status> {
        let t = tools();
        let cross = |tool: &str| {
            target
                .cross
                .then(|| target.triple.cross_tool(tool))
                .flatten()
        };
        match self {
            Self::Qld => Ok(qld_binary()),
            Self::GnuLd => if target.cross {
                cross("ld.bfd").or_else(|| cross("ld"))
            } else {
                t.gnu_ld.clone()
            }
            .ok_or_else(|| Status::missing(format!("GNU ld for {}", target.triple))),
            Self::Lld => t.lld.clone().ok_or_else(|| Status::missing("lld")),
            Self::Mold => t.mold.clone().ok_or_else(|| Status::missing("mold")),
            Self::Gold => if target.cross {
                cross("ld.gold")
            } else {
                t.gold.clone()
            }
            .ok_or_else(|| Status::missing("gold")),
            Self::Custom(path) => {
                tools::find_program(path).ok_or_else(|| Status::missing(path.display().to_string()))
            }
        }
    }
}

/// The toolchain for one target: host or cross.
#[derive(Clone, Debug)]
pub struct TargetEnv {
    /// The target.
    pub triple: Triple,
    /// Whether this is not the host target.
    pub cross: bool,
    /// C compiler driver.
    pub cc: Option<PathBuf>,
    /// C++ compiler driver.
    pub cxx: Option<PathBuf>,
    /// Archiver.
    pub ar: Option<PathBuf>,
    /// `qemu-user` for running cross binaries.
    pub qemu: Option<PathBuf>,
    /// Sysroot for `QEMU_LD_PREFIX`.
    pub qemu_sysroot: Option<PathBuf>,
}

impl TargetEnv {
    /// Resolves the toolchain for a target (`None` = the host).
    pub fn resolve(target: Option<&str>) -> Result<Self, Status> {
        let t = tools();
        let host = t
            .host
            .clone()
            .ok_or_else(|| Status::missing("a C compiler (to determine the host target)"))?;
        let triple = match target {
            None => host.clone(),
            Some(text) => {
                Triple::parse(text).ok_or_else(|| Status::Fail(format!("bad target {text:?}")))?
            }
        };
        if !triple.is_linux() {
            return Err(Status::skip(format!(
                "target {triple} is not Linux; ELF fixtures need a Linux target"
            )));
        }
        if triple == host {
            return Ok(Self {
                triple,
                cross: false,
                cc: t.cc.clone(),
                cxx: t.cxx.clone(),
                ar: t.ar.clone(),
                qemu: None,
                qemu_sysroot: None,
            });
        }
        let cc = triple
            .cross_tool("gcc")
            .ok_or_else(|| Status::missing(format!("cross compiler for {triple}")))?;
        let qemu = triple
            .qemu()
            .ok_or_else(|| Status::missing(format!("qemu-{}", triple.qemu_arch())))?;
        Ok(Self {
            cxx: triple.cross_tool("g++"),
            ar: triple.cross_tool("ar").or_else(|| t.ar.clone()),
            qemu_sysroot: triple.qemu_sysroot(),
            qemu: Some(qemu),
            cc: Some(cc),
            triple,
            cross: true,
        })
    }

    /// Maps a tool name used in a fixture command to the binary for this
    /// target. `None` means "not a tool the harness knows".
    fn known_tool(&self, name: &str) -> Option<Result<PathBuf, Status>> {
        let found = |tool: &Option<PathBuf>, what: &str| {
            tool.clone()
                .ok_or_else(|| Status::missing(format!("{what} for {}", self.triple)))
        };
        match name {
            "cc" | "gcc" => Some(found(&self.cc, "C compiler")),
            "c++" | "g++" => Some(found(&self.cxx, "C++ compiler")),
            "ar" => Some(found(&self.ar, "ar")),
            _ => None,
        }
    }

    /// Rewrites the first word of a shell command to the target's tool, and
    /// checks that the program exists.
    fn rewrite_command(&self, command: &str) -> Result<String, Status> {
        let trimmed = command.trim_start();
        let first_len = trimmed
            .find(|c: char| c.is_whitespace())
            .unwrap_or(trimmed.len());
        let (first, rest) = trimmed.split_at(first_len);
        match self.known_tool(first) {
            Some(tool) => Ok(format!(
                "{}{rest}",
                process::shell_quote(&tool?.to_string_lossy())
            )),
            None => {
                let is_builtin = matches!(
                    first,
                    "cd" | "echo" | "printf" | "test" | "[" | "set" | "export" | "exit" | "true"
                ) || first.contains('=')
                    || first.starts_with("./")
                    || first.contains('$');
                if !is_builtin && tools::find_program(first).is_none() {
                    return Err(Status::missing(first.to_string()));
                }
                Ok(command.to_string())
            }
        }
    }

    fn apply_env(&self, command: &mut Command) {
        let set = |command: &mut Command, var: &str, tool: &Option<PathBuf>| {
            if let Some(tool) = tool {
                command.env(var, tool);
            }
        };
        set(command, "CC", &self.cc);
        set(command, "CXX", &self.cxx);
        set(command, "AR", &self.ar);
        command.env("QLD_TARGET", self.triple.to_string());
        if let Some(sysroot) = &self.qemu_sysroot {
            command.env("QEMU_LD_PREFIX", sysroot);
        }
    }
}

/// Log of commands run for a job, shown when it fails.
#[derive(Default, Debug)]
pub struct Log(pub String);

impl Log {
    fn command(&mut self, description: &str, output: &Output) {
        let _ = writeln!(self.0, "$ {description}");
        let _ = writeln!(self.0, "  [{}]", output.describe_status());
        for (label, text) in [
            ("stdout", output.stdout_text()),
            ("stderr", output.stderr_text()),
        ] {
            if !text.is_empty() {
                let _ = writeln!(self.0, "  {label}:");
                for line in text.lines().take(60) {
                    let _ = writeln!(self.0, "    {line}");
                }
            }
        }
    }
}

/// Recursively copies `from` into `to` (which is created).
pub fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else if kind.is_symlink() {
            let link = std::fs::read_link(entry.path())?;
            symlink(&link, &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn symlink(original: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(original, link)
}

#[cfg(not(unix))]
fn symlink(original: &Path, link: &Path) -> std::io::Result<()> {
    std::fs::copy(original, link).map(|_| ())
}

/// Creates an empty scratch directory, removing any previous contents.
pub fn fresh_dir(dir: &Path) -> Result<(), Status> {
    if dir.exists() {
        std::fs::remove_dir_all(dir)
            .map_err(|e| Status::Fail(format!("cannot clear {}: {e}", dir.display())))?;
    }
    std::fs::create_dir_all(dir)
        .map_err(|e| Status::Fail(format!("cannot create {}: {e}", dir.display())))
}

/// Copies the fixture sources into `dir` and runs the `compile` commands.
pub fn prepare(
    fixture: &Fixture,
    env: &TargetEnv,
    dir: &Path,
    log: &mut Log,
) -> Result<(), Status> {
    let sh = tools().sh.clone().ok_or_else(|| Status::missing("sh"))?;
    fresh_dir(dir)?;
    copy_dir(&fixture.dir, dir)
        .map_err(|e| Status::Fail(format!("cannot copy fixture to {}: {e}", dir.display())))?;
    for command in &fixture.compile {
        let script = env.rewrite_command(command)?;
        let mut child = process::shell(&sh, &script, dir);
        env.apply_env(&mut child);
        let output = process::run(&mut child, fixture.timeout)
            .map_err(|e| Status::Fail(format!("cannot run `{command}`: {e}")))?;
        log.command(command, &output);
        if !output.success() {
            return Err(Status::Fail(format!(
                "compile command failed ({}): {command}\n{}",
                output.describe_status(),
                output.stderr_text()
            )));
        }
    }
    Ok(())
}

/// Why a link did not succeed.
#[derive(Debug)]
pub enum LinkError {
    /// qld reported "not implemented yet".
    Unimplemented(String),
    /// The linker failed.
    Failed(String),
    /// The link could not be attempted (missing tool, I/O error).
    Status(Status),
}

impl From<Status> for LinkError {
    fn from(status: Status) -> Self {
        Self::Status(status)
    }
}

/// Runs every `link` line in `dir` with `linker`, appending `extra` linker
/// arguments (passed through `-Wl,` in driver mode).
pub fn link(
    fixture: &Fixture,
    env: &TargetEnv,
    linker: &Linker,
    dir: &Path,
    extra: &[String],
    log: &mut Log,
) -> Result<(), LinkError> {
    link_with_vars(fixture, env, linker, dir, extra, &[], log)
}

/// [`link`], with extra environment variables for the link commands.
pub fn link_with_vars(
    fixture: &Fixture,
    env: &TargetEnv,
    linker: &Linker,
    dir: &Path,
    extra: &[String],
    vars: &[(String, String)],
    log: &mut Log,
) -> Result<(), LinkError> {
    let linker_binary = linker.binary(env)?;
    for line in &fixture.links {
        let words = process::split_words(line).map_err(Status::Fail)?;
        let (mut command, description) = match &fixture.driver {
            Some(driver) => {
                let driver_binary = match env.known_tool(driver) {
                    Some(tool) => tool?,
                    None => tools::find_program(driver).ok_or_else(|| Status::missing(driver))?,
                };
                // The driver finds `ld` through the shim directory, which
                // holds a symlink to the linker under test.
                let shim = dir.join(".linker-shim");
                let _ = std::fs::remove_dir_all(&shim);
                std::fs::create_dir_all(&shim)
                    .map_err(|e| Status::Fail(format!("cannot create {}: {e}", shim.display())))?;
                let shim_ld = shim.join("ld");
                symlink(&linker_binary, &shim_ld).map_err(|e| {
                    Status::Fail(format!("cannot create {}: {e}", shim_ld.display()))
                })?;
                let mut command = Command::new(&driver_binary);
                let selector = if tools::driver_is_clang(&driver_binary) {
                    format!("--ld-path={}", shim_ld.display())
                } else {
                    format!("-B{}/", shim.display())
                };
                command.arg(&selector).args(&words);
                let mut description = format!("{driver} {selector} {line}");
                for arg in extra {
                    command.arg(format!("-Wl,{arg}"));
                    let _ = write!(description, " -Wl,{arg}");
                }
                (command, description)
            }
            None => {
                let mut command = Command::new(&linker_binary);
                command.args(&words).args(extra);
                let mut description = format!("{} {line}", linker.slug());
                for arg in extra {
                    let _ = write!(description, " {arg}");
                }
                (command, description)
            }
        };
        command.current_dir(dir).env("LC_ALL", "C");
        env.apply_env(&mut command);
        let mut description = description;
        for (var, value) in vars {
            command.env(var, value);
            description = format!("{var}={value} {description}");
        }
        let output = process::run(&mut command, fixture.timeout)
            .map_err(|e| Status::Fail(format!("cannot run `{description}`: {e}")))?;
        log.command(&description, &output);
        if !output.success() {
            let stderr = output.stderr_text();
            if *linker == Linker::Qld
                && let Some(line) = stderr.lines().find(|l| l.contains("not implemented yet"))
            {
                return Err(LinkError::Unimplemented(line.trim().to_string()));
            }
            return Err(LinkError::Failed(format!(
                "link failed ({}): {description}\n{stderr}",
                output.describe_status()
            )));
        }
    }
    Ok(())
}

/// Runs the fixture's `run` command in `dir` (under qemu for cross targets).
pub fn run_program(
    fixture: &Fixture,
    env: &TargetEnv,
    dir: &Path,
    log: &mut Log,
) -> Result<Option<Output>, Status> {
    let Some(run) = &fixture.run else {
        return Ok(None);
    };
    let sh = tools().sh.clone().ok_or_else(|| Status::missing("sh"))?;
    let script = match &env.qemu {
        Some(qemu) => {
            // Insert qemu after any leading VAR=value assignments.
            let words = process::split_words(run).map_err(Status::Fail)?;
            let split = words.iter().take_while(|w| w.contains('=')).count();
            let mut quoted: Vec<String> = words.iter().map(|w| process::shell_quote(w)).collect();
            quoted.insert(split, process::shell_quote(&qemu.to_string_lossy()));
            quoted.join(" ")
        }
        None => run.clone(),
    };
    let mut command = process::shell(&sh, &script, dir);
    env.apply_env(&mut command);
    let output = process::run(&mut command, fixture.timeout)
        .map_err(|e| Status::Fail(format!("cannot run `{run}`: {e}")))?;
    log.command(run, &output);
    Ok(Some(output))
}

/// Checks `expect.exit` and `expect.stdout` against a run.
pub fn check_run(fixture: &Fixture, output: &Output, problems: &mut Vec<String>) {
    let exit_ok =
        !output.timed_out && output.status.code() == i32::try_from(fixture.expect_exit).ok();
    if !exit_ok {
        problems.push(format!(
            "`{}`: expected exit code {}, got {}\nstderr:\n{}",
            fixture.run.as_deref().unwrap_or_default(),
            fixture.expect_exit,
            output.describe_status(),
            output.stderr_text()
        ));
    }
    if let Some(expected) = &fixture.expect_stdout {
        let actual = output.stdout_text();
        if *expected != actual {
            problems.push(format!(
                "stdout of `{}` differs:\n{}",
                fixture.run.as_deref().unwrap_or_default(),
                textdiff::diff(expected, &actual)
            ));
        }
    }
}

/// Checks `expect.readelf` and `expect.readelf_files` in `dir`.
pub fn check_readelf(
    fixture: &Fixture,
    dir: &Path,
    problems: &mut Vec<String>,
) -> Result<(), Status> {
    let mut checks: Vec<(String, &[String])> = Vec::new();
    if !fixture.expect_readelf.is_empty() {
        checks.push((fixture.main_output(), &fixture.expect_readelf));
    }
    for (file, patterns) in &fixture.expect_readelf_files {
        checks.push((file.clone(), patterns));
    }
    if checks.is_empty() {
        return Ok(());
    }
    let readelf = tools()
        .readelf
        .clone()
        .ok_or_else(|| Status::missing("readelf"))?;
    for (file, patterns) in checks {
        let text = match super::readelf::run(&readelf, &fixture.readelf_args, &dir.join(&file)) {
            Ok(text) => text,
            Err(error) => {
                problems.push(error);
                continue;
            }
        };
        let saved = dir.join(format!("readelf.{}.txt", file.replace('/', "_")));
        let _ = std::fs::write(&saved, &text);
        for pattern in patterns {
            let (negated, needle) = match pattern.strip_prefix('!') {
                Some(needle) => (true, needle),
                None => (false, pattern.as_str()),
            };
            if text.contains(needle) == negated {
                let (verb, detail) = if negated {
                    let line = text
                        .lines()
                        .find(|l| l.contains(needle))
                        .unwrap_or_default();
                    (
                        "must not appear",
                        format!("\n    found in: {}", line.trim()),
                    )
                } else {
                    ("must appear", String::new())
                };
                problems.push(format!(
                    "readelf -W {} {file}: {needle:?} {verb}{detail}\n    (full output: {})",
                    fixture.readelf_args.join(" "),
                    saved.display()
                ));
            }
        }
    }
    Ok(())
}

/// Options for [`run_job`].
#[derive(Clone, Debug)]
pub struct JobOptions {
    /// Linker to test.
    pub linker: Linker,
    /// Scratch directory for this suite.
    pub scratch: PathBuf,
}

/// One fixture/target combination.
#[derive(Clone, Debug)]
pub struct Job {
    /// Index into the fixture list.
    pub fixture: usize,
    /// Target, or `None` for the host.
    pub target: Option<String>,
    /// Display name: `fixture` or `fixture[target]`.
    pub name: String,
}

/// Expands fixtures into jobs, one per target.
pub fn jobs(fixtures: &[Fixture]) -> Vec<Job> {
    let mut jobs = Vec::new();
    for (index, fixture) in fixtures.iter().enumerate() {
        if fixture.targets.is_empty() {
            jobs.push(Job {
                fixture: index,
                target: None,
                name: fixture.name.clone(),
            });
        } else {
            for target in &fixture.targets {
                jobs.push(Job {
                    fixture: index,
                    target: Some(target.clone()),
                    name: format!("{}[{target}]", fixture.name),
                });
            }
        }
    }
    jobs
}

/// Result of a job.
#[derive(Debug)]
pub struct JobResult {
    /// Job display name.
    pub name: String,
    /// Outcome.
    pub status: Status,
    /// Commands run.
    pub log: Log,
    /// Scratch directory.
    pub dir: PathBuf,
    /// Wall time.
    pub elapsed: Duration,
}

/// Directory-safe job name.
pub fn job_slug(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Runs one fixture job end to end.
pub fn run_job(fixture: &Fixture, job: &Job, options: &JobOptions) -> JobResult {
    let start = Instant::now();
    let dir = options.scratch.join(job_slug(&job.name));
    let mut log = Log::default();
    let status = match run_job_inner(fixture, job, options, &dir, &mut log) {
        Ok(status) | Err(status) => status,
    };
    JobResult {
        name: job.name.clone(),
        status,
        log,
        dir,
        elapsed: start.elapsed(),
    }
}

fn run_job_inner(
    fixture: &Fixture,
    job: &Job,
    options: &JobOptions,
    dir: &Path,
    log: &mut Log,
) -> Result<Status, Status> {
    if let Some(reason) = &fixture.skip {
        return Err(Status::skip(format!("fixture disabled: {reason}")));
    }
    check_required_files(fixture)?;
    let env = TargetEnv::resolve(job.target.as_deref())?;
    let work = dir.join("work");
    prepare(fixture, &env, &work, log)?;

    // Snapshot the compiled inputs for the determinism relinks.
    let pristine = dir.join("pristine");
    if fixture.determinism {
        fresh_dir(&pristine)?;
        copy_dir(&work, &pristine)
            .map_err(|e| Status::Fail(format!("cannot snapshot {}: {e}", work.display())))?;
    }

    if options.linker == Linker::GnuLd {
        check_gnu_ld_version(fixture, &options.linker.binary(&env)?)?;
    }
    let expect_gnu_failure =
        options.linker == Linker::GnuLd && fixture.gnu_ld == GnuLdExpectation::Fail;
    match link(fixture, &env, &options.linker, &work, &[], log) {
        Ok(()) if expect_gnu_failure => {
            return Err(Status::Fail(
                "test.toml says GNU ld rejects this link (gnu_ld = \"fail\"), but it succeeded"
                    .to_string(),
            ));
        }
        Ok(()) => {}
        Err(LinkError::Unimplemented(message)) => return Err(Status::skip(message)),
        Err(LinkError::Failed(_)) if expect_gnu_failure => {
            return Ok(Status::Pass(Some(
                "GNU ld rejects this link, as expected".into(),
            )));
        }
        Err(LinkError::Failed(message)) => return Err(Status::Fail(message)),
        Err(LinkError::Status(status)) => return Err(status),
    }

    let mut problems = Vec::new();
    if let Some(output) = run_program(fixture, &env, &work, log)? {
        check_run(fixture, &output, &mut problems);
    }
    check_readelf(fixture, &work, &mut problems)?;

    let mut note = None;
    if fixture.determinism && problems.is_empty() {
        match super::determinism::check(fixture, &env, &options.linker, &pristine, &work, dir, log)
        {
            Ok(summary) => note = Some(summary),
            Err(Status::Fail(message)) => problems.push(message),
            Err(other) => return Err(other),
        }
    }

    if problems.is_empty() {
        Ok(Status::Pass(note))
    } else {
        Err(Status::Fail(problems.join("\n\n")))
    }
}

/// Prints per-job lines and a summary; returns the failure report, if any.
pub fn report(suite: &str, results: &[JobResult]) -> Option<String> {
    let require_tools = tools::tools_required();
    let (mut passed, mut skipped, mut failed) = (0, 0, 0);
    let mut failures = String::new();
    for result in results {
        let secs = result.elapsed.as_secs_f64();
        match &result.status {
            Status::Pass(note) => {
                passed += 1;
                match note {
                    Some(note) => println!("PASS    {} ({secs:.2}s; {note})", result.name),
                    None => println!("PASS    {} ({secs:.2}s)", result.name),
                }
            }
            Status::Skip {
                reason,
                missing_tool,
            } if !(require_tools && *missing_tool) => {
                skipped += 1;
                println!("SKIPPED: {}: {reason}", result.name);
            }
            Status::Skip { reason, .. } | Status::Fail(reason) => {
                failed += 1;
                println!("FAIL    {} ({secs:.2}s)", result.name);
                let _ = writeln!(
                    failures,
                    "\n==== FAIL {} ====\n{reason}\n\nscratch dir: {}\ncommands:\n{}",
                    result.name,
                    result.dir.display(),
                    result.log.0
                );
            }
        }
    }
    let summary = format!("{suite}: {passed} passed, {skipped} skipped, {failed} failed");
    println!("{summary}");
    (failed > 0).then(|| format!("{failures}\n{summary}"))
}
