//! Integration tests for `qld::demangle`.
//!
//! - `vectors`: the committed vectors in `tests/data/demangle/`, checked on
//!   every run. Each line is `mangled<TAB>expected`; files named
//!   `*.verbose.txt` are checked with [`Options::verbose`], the others with
//!   the default options. A missing expected output means the name must not
//!   demangle.
//! - `mutations`: randomized corruption of the vectors, asserting that
//!   nothing panics and output stays bounded.
//! - `sweep_system_libraries` (ignored, run with `--ignored`): demangles every
//!   mangled symbol of the host's libraries and compares with `c++filt`.
//!   `QLD_DEMANGLE_TSV=<file>` uses a precomputed `mangled<TAB>c++filt` file
//!   instead; `QLD_DEMANGLE_MISMATCHES=<file>` writes the differences.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use qld::demangle::{
    MAX_DEPTH, MAX_OUTPUT, Options, Scheme, demangle, try_demangle_scheme, try_demangle_with,
};
use rayon::prelude::*;

fn data_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/demangle")
}

/// Reads every vector file: (file name, verbose, mangled, expected).
fn vectors() -> Vec<(String, bool, String, Option<String>)> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(data_dir())
        .expect("tests/data/demangle")
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "txt"))
        .collect();
    files.sort();
    let mut out = Vec::new();
    for path in files {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let verbose = name.ends_with(".verbose.txt");
        let text = std::fs::read_to_string(&path).unwrap();
        for line in text.lines() {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (mangled, expected) = match line.split_once('\t') {
                Some((m, e)) => (m.to_string(), Some(e.to_string())),
                None => (line.to_string(), None),
            };
            out.push((name.clone(), verbose, mangled, expected));
        }
    }
    out
}

#[test]
fn vectors_match() {
    let vectors = vectors();
    assert!(vectors.len() > 100, "too few vectors: {}", vectors.len());
    let mut failures = Vec::new();
    for (file, verbose, mangled, expected) in &vectors {
        let options = if *verbose {
            Options::verbose()
        } else {
            Options::new()
        };
        let got = try_demangle_with(mangled.as_bytes(), options);
        if got != *expected {
            failures.push(format!(
                "{file}: {mangled}\n  expected: {expected:?}\n       got: {got:?}"
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} vectors differ:\n{}",
        failures.len(),
        vectors.len(),
        failures.join("\n")
    );
}

#[test]
fn unchanged_when_not_mangled() {
    assert_eq!(demangle(b"memcpy"), "memcpy");
    assert_eq!(demangle(b"_Zfoo"), "_Zfoo");
    assert_eq!(demangle(b""), "");
}

/// A small deterministic generator (xorshift64*).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

const ALPHABET: &[u8] = b"_0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz$.";

fn mutate(rng: &mut Rng, input: &[u8]) -> Vec<u8> {
    let mut out = input.to_vec();
    for _ in 0..=rng.below(4) {
        match rng.below(6) {
            0 if !out.is_empty() => {
                let at = rng.below(out.len());
                out[at] = ALPHABET[rng.below(ALPHABET.len())];
            }
            1 => {
                let at = rng.below(out.len() + 1);
                out.insert(at, ALPHABET[rng.below(ALPHABET.len())]);
            }
            2 if !out.is_empty() => {
                let at = rng.below(out.len());
                out.remove(at);
            }
            3 => {
                let at = rng.below(out.len() + 1);
                out.truncate(at);
            }
            4 if out.len() > 2 => {
                // Duplicate a slice: makes substitution-heavy names.
                let start = rng.below(out.len());
                let end = (start + rng.below(16)).min(out.len());
                let piece = out[start..end].to_vec();
                let at = rng.below(out.len() + 1);
                out.splice(at..at, piece);
            }
            _ => {
                let at = rng.below(out.len() + 1);
                let digits = format!("{}", rng.below(40));
                out.splice(at..at, digits.bytes());
            }
        }
    }
    out
}

#[test]
fn mutations_never_panic() {
    let seeds: Vec<Vec<u8>> = vectors().into_iter().map(|v| v.2.into_bytes()).collect();
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let iterations = if cfg!(debug_assertions) {
        100_000
    } else {
        1_000_000
    };
    for i in 0..iterations {
        let seed = &seeds[i % seeds.len()];
        let input = mutate(&mut rng, seed);
        for options in [Options::new(), Options::verbose()] {
            if let Some(text) = try_demangle_with(&input, options) {
                assert!(
                    text.len() <= MAX_OUTPUT,
                    "{}",
                    String::from_utf8_lossy(&input)
                );
            }
        }
        if i % 8 == 0
            && let Some(parts) = qld::demangle::parts(&input, Options::new())
        {
            assert!(parts.full.len() <= MAX_OUTPUT);
        }
    }
}

#[test]
fn hostile_inputs_are_bounded() {
    // Deep nesting.
    let deep = format!("_Z1f{}i", "P".repeat(100_000));
    assert!(try_demangle_with(deep.as_bytes(), Options::new()).is_none());
    let deep = format!("_Z1fI{}iE", "IJ".repeat(50_000));
    let _ = try_demangle_with(deep.as_bytes(), Options::new());
    // Exponential expansion through substitutions: each parameter doubles.
    let mut name = String::from("_Z1fPi");
    for i in 0..60 {
        name.push_str(&format!("S{}_", base36(i)));
    }
    let _ = try_demangle_with(name.as_bytes(), Options::new());
    let mut name = String::from("_ZN1aI");
    for _ in 0..40 {
        name.push_str("S_S_");
    }
    name.push_str("EE");
    let _ = try_demangle_with(name.as_bytes(), Options::new());
    // Rust v0 back-references that fan out.
    let mut name = String::from("_RINvC1a1bT");
    for _ in 0..200 {
        name.push_str("B8_");
    }
    name.push_str("EE");
    let _ = try_demangle_with(name.as_bytes(), Options::new());
    // Very long inputs are rejected outright.
    let long = format!("_Z{}", "1a".repeat(1 << 16));
    assert!(try_demangle_with(long.as_bytes(), Options::new()).is_none());
}

fn base36(mut n: u32) -> String {
    let digits = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";
    let mut out = Vec::new();
    loop {
        out.push(digits[(n % 36) as usize]);
        n /= 36;
        if n == 0 {
            break;
        }
    }
    out.reverse();
    String::from_utf8(out).unwrap()
}

#[test]
fn deep_inputs_fit_a_small_stack() {
    let mut names: Vec<String> = Vec::new();
    // Nesting just below and far above the recursion limit.
    for n in [MAX_DEPTH - 8, MAX_DEPTH / 2, 1000] {
        names.push(format!("_Z1f{}i", "P".repeat(n)));
        names.push(format!("_Z1f{}i", "A1_".repeat(n)));
        names.push(format!("_Z1fIX{}fp_EEv", "ng".repeat(n)));
        names.push(format!("_Z1f{}v{}", "PFv".repeat(n / 2), "E".repeat(n / 2)));
        names.push(format!(
            "_Z1f{}i{}",
            "N1aI".repeat(n / 2),
            "EE".repeat(n / 2)
        ));
        names.push(format!("_R{}C1a{}", "Nv".repeat(n), "1b".repeat(n)));
        names.push(format!("_RINvC1a1b{}uE", "R".repeat(n)));
    }
    // Debug builds have much larger frames.
    let stack = if cfg!(debug_assertions) { 1024 } else { 128 };
    std::thread::Builder::new()
        .stack_size(stack * 1024)
        .spawn(move || {
            for name in names {
                let _ = try_demangle_with(name.as_bytes(), Options::verbose());
            }
        })
        .unwrap()
        .join()
        .unwrap();
}

// ----- the sweep -----

fn tool(name: &str) -> bool {
    Command::new(name)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

fn glob_dir(dir: &Path, pred: impl Fn(&str) -> bool) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(&pred)
        })
        .collect();
    out.sort();
    out
}

fn subdirs(dir: &Path) -> Vec<PathBuf> {
    glob_dir(dir, |_| true)
        .into_iter()
        .filter(|p| p.is_dir())
        .collect()
}

fn library_files() -> Vec<PathBuf> {
    let so = |n: &str| n.contains(".so");
    let mut files = glob_dir(Path::new("/usr/lib64"), so);
    for gcc in subdirs(Path::new("/usr/lib/gcc")) {
        for version in subdirs(&gcc) {
            files.extend(glob_dir(&version, |n| n.starts_with("libstdc++.so")));
        }
    }
    for llvm in subdirs(Path::new("/usr/lib/llvm")) {
        files.extend(glob_dir(&llvm.join("lib64"), |n| {
            n.contains(".so") || n.ends_with(".a")
        }));
    }
    if let Some(home) = std::env::var_os("HOME") {
        for toolchain in subdirs(&Path::new(&home).join(".rustup/toolchains")) {
            files.extend(glob_dir(&toolchain.join("lib"), so));
            for target in subdirs(&toolchain.join("lib/rustlib")) {
                files.extend(glob_dir(&target.join("lib"), |n| {
                    n.ends_with(".rlib") || n.contains(".so")
                }));
            }
        }
    }
    files
}

fn symbols_of(path: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for args in [&["-D"][..], &[][..]] {
        let Ok(output) = Command::new("nm")
            .args(args)
            .arg(path)
            .stderr(Stdio::null())
            .output()
        else {
            continue;
        };
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let Some(name) = line.split_whitespace().last() else {
                continue;
            };
            let name = name.split('@').next().unwrap_or(name);
            let mangled =
                name.starts_with("_Z") || name.starts_with("_R") || name.starts_with("_GLOBAL_");
            if mangled
                && name
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'$' | b'.'))
            {
                out.push(name.to_string());
            }
        }
    }
    out
}

/// (mangled, c++filt output) pairs.
fn corpus() -> Option<Vec<(String, String)>> {
    if let Some(path) = std::env::var_os("QLD_DEMANGLE_TSV") {
        let text = std::fs::read_to_string(path).ok()?;
        return Some(
            text.lines()
                .filter_map(|l| l.split_once('\t'))
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .collect(),
        );
    }
    if !tool("nm") || !tool("c++filt") {
        eprintln!("skipping: nm or c++filt not installed");
        return None;
    }
    let files = library_files();
    let mut symbols: Vec<String> = files.par_iter().flat_map_iter(|f| symbols_of(f)).collect();
    symbols.par_sort_unstable();
    symbols.dedup();
    eprintln!(
        "{} files, {} distinct mangled symbols",
        files.len(),
        symbols.len()
    );
    let mut child = Command::new("c++filt")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .ok()?;
    let mut stdin = child.stdin.take()?;
    let input = symbols.join("\n") + "\n";
    let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()));
    let output = child.wait_with_output().ok()?;
    writer.join().ok()?.ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() != symbols.len() {
        eprintln!(
            "c++filt output has {} lines for {} symbols",
            lines.len(),
            symbols.len()
        );
        return None;
    }
    Some(
        symbols
            .into_iter()
            .zip(lines.into_iter().map(str::to_string))
            .collect(),
    )
}

#[test]
#[ignore = "sweeps the host's libraries; run with --ignored"]
fn sweep_system_libraries() {
    let Some(corpus) = corpus() else {
        return;
    };
    #[derive(Default)]
    struct Tally {
        total: usize,
        accepted: usize,
        matched: usize,
        extra: usize,
    }
    let results: Vec<(Option<Scheme>, bool, bool, bool, String)> = corpus
        .par_iter()
        .map(|(mangled, expected)| {
            let got = try_demangle_scheme(mangled.as_bytes(), Options::verbose());
            let accepted = expected != mangled;
            let scheme = got
                .as_ref()
                .map(|(_, s)| *s)
                .or_else(|| qld::demangle::scheme(mangled.as_bytes()));
            let text = got.map(|(t, _)| t);
            let matched = accepted && text.as_deref() == Some(expected.as_str());
            let extra = !accepted && text.is_some();
            let line = if accepted && !matched {
                format!(
                    "{mangled}\n  c++filt: {expected}\n      qld: {}\n",
                    text.as_deref().unwrap_or("<none>")
                )
            } else if extra {
                format!(
                    "{mangled}\n  c++filt: <rejected>\n      qld: {}\n",
                    text.as_deref().unwrap_or("")
                )
            } else {
                String::new()
            };
            (scheme, accepted, matched, extra, line)
        })
        .collect();
    let mut tallies: BTreeMap<String, Tally> = BTreeMap::new();
    let mut mismatches = String::new();
    let mut extras = String::new();
    for (scheme, accepted, matched, extra, line) in &results {
        let key = scheme.map_or("unknown".to_string(), |s| format!("{s:?}"));
        let tally = tallies.entry(key).or_default();
        tally.total += 1;
        tally.accepted += usize::from(*accepted);
        tally.matched += usize::from(*matched);
        tally.extra += usize::from(*extra);
        if *accepted && !*matched {
            mismatches.push_str(line);
        } else if *extra {
            extras.push_str(line);
        }
    }
    for (scheme, t) in &tallies {
        let rate = if t.accepted == 0 {
            100.0
        } else {
            t.matched as f64 * 100.0 / t.accepted as f64
        };
        eprintln!(
            "{scheme:>10}: {} symbols, c++filt accepts {}, qld matches {} ({rate:.4}%), qld also demangles {} that c++filt rejects",
            t.total, t.accepted, t.matched, t.extra
        );
    }
    if let Some(path) = std::env::var_os("QLD_DEMANGLE_MISMATCHES") {
        std::fs::write(
            &path,
            format!("{mismatches}\n# accepted only by qld\n{extras}"),
        )
        .unwrap();
    } else {
        for line in mismatches.lines().take(60) {
            eprintln!("{line}");
        }
    }
}
