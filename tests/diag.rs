//! End-to-end checks of the diagnostics framework (workstream W51).
//!
//! The unit tests in `src/diag.rs` pin the rendering; these run the `qld`
//! binary and compare the shape of what it prints with GNU ld 2.4x and
//! lld 2x, which `docs/compatibility.md` records. A test prints `SKIPPED:`
//! and passes when a tool it needs is missing.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A fresh, empty directory for one test.
fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("diag-tests")
        .join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn tool(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

fn host_ok() -> bool {
    cfg!(all(target_os = "linux", target_arch = "x86_64"))
}

macro_rules! require {
    ($($name:literal),*) => {
        if !host_ok() {
            println!("SKIPPED: host is not x86-64 Linux");
            return;
        }
        $(
            if tool($name).is_none() {
                println!("SKIPPED: {} not found", $name);
                return;
            }
        )*
    };
}

fn run(dir: &Path, program: &str, args: &[&str]) -> Output {
    Command::new(program)
        .args(args)
        .current_dir(dir)
        .env("LC_ALL", "C")
        .output()
        .unwrap_or_else(|error| panic!("cannot run {program}: {error}"))
}

fn qld(dir: &Path, args: &[&str]) -> Output {
    run(dir, env!("CARGO_BIN_EXE_qld"), args)
}

/// Runs `qld`, expects it to fail with status 1 — what GNU ld and lld exit
/// with for every diagnostic these tests provoke — and returns its standard
/// error.
fn qld_err(dir: &Path, args: &[&str]) -> String {
    let output = qld(dir, args);
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert_eq!(
        output.status.code(),
        Some(1),
        "qld {} should have failed with status 1:\n{stderr}",
        args.join(" ")
    );
    stderr
}

fn compile(dir: &Path, name: &str, source: &str, flags: &[&str]) {
    fs::write(dir.join(format!("{name}.c")), source).unwrap();
    let mut args = vec!["-c", "-O0", "-fno-pie"];
    args.extend_from_slice(flags);
    let source = format!("{name}.c");
    let object = format!("{name}.o");
    args.extend_from_slice(&[source.as_str(), "-o", object.as_str()]);
    let output = run(dir, "cc", &args);
    assert!(
        output.status.success(),
        "cc failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

const MAIN: &str = "int missing(void);\nint dup(void) { return 1; }\nint main(void) { return \
                    missing() + dup(); }\n";
const OTHER: &str = "int dup(void) { return 2; }\n";

/// An undefined symbol renders like lld's: the message, then the source
/// position from DWARF, then the object reference aligned under it.
#[test]
fn undefined_symbol_shape() {
    require!("cc");
    let dir = scratch("undefined");
    compile(&dir, "main", MAIN, &["-g"]);
    let stderr = qld_err(&dir, &["--no-fork", "-o", "out", "main.o"]);

    assert!(
        stderr.starts_with("qld: error: undefined symbol: missing\n"),
        "{stderr}"
    );
    let source = stderr
        .lines()
        .find(|line| line.starts_with(">>> referenced by "))
        .unwrap_or_else(|| panic!("no `referenced by` line:\n{stderr}"));
    assert!(source.contains("main.c:3"), "{stderr}");
    // The object reference is on its own line, indented under the source.
    assert!(
        stderr.contains(">>>               main.o:(.text+"),
        "{stderr}"
    );
    // GNU ld and lld print no summary count.
    assert!(!stderr.contains("1 error"), "{stderr}");
}

/// Without debug information there is no source line, and the object
/// reference takes the `referenced by` line itself, as in lld.
#[test]
fn undefined_symbol_without_debug_info() {
    require!("cc");
    let dir = scratch("undefined-nodebug");
    compile(&dir, "main", MAIN, &[]);
    let stderr = qld_err(&dir, &["--no-fork", "-o", "out", "main.o"]);
    assert!(
        stderr.contains(">>> referenced by main.o:(.text+"),
        "{stderr}"
    );
    assert!(!stderr.contains(">>>               "), "{stderr}");
}

/// Two strong definitions: lld's `duplicate symbol:` wording, and every
/// location labelled `defined at`, not `referenced by`.
#[test]
fn duplicate_symbol_shape() {
    require!("cc");
    let dir = scratch("duplicate");
    compile(&dir, "main", MAIN, &[]);
    compile(&dir, "other", OTHER, &[]);
    let stderr = qld_err(&dir, &["--no-fork", "-o", "out", "main.o", "other.o"]);
    assert!(
        stderr.contains("qld: error: duplicate symbol: dup\n"),
        "{stderr}"
    );
    assert!(stderr.contains(">>> defined at main.o:(.text+"), "{stderr}");
    assert!(
        stderr.contains(">>> defined at other.o:(.text+"),
        "{stderr}"
    );
}

/// Errors come out sorted by `Diagnostic::order` — input order here — and
/// that holds whatever the thread count is.
#[test]
fn output_is_deterministic_across_thread_counts() {
    require!("cc");
    let dir = scratch("deterministic");
    for (index, name) in ["a", "b", "c", "d"].iter().enumerate() {
        let source = format!(
            "int missing_{name}(void);\nint use_{name}(void) {{ return missing_{name}(); }}\n"
        );
        compile(&dir, name, &source, &[]);
        let _ = index;
    }
    compile(
        &dir,
        "main",
        "int use_a(void); int use_b(void); int use_c(void); int use_d(void);\nint main(void) { \
         return use_a() + use_b() + use_c() + use_d(); }\n",
        &[],
    );
    let mut seen: Option<String> = None;
    for threads in ["--threads=1", "--threads=2", "--threads=4"] {
        let stderr = qld_err(
            &dir,
            &[
                "--no-fork",
                threads,
                "--error-limit=0",
                "-o",
                "out",
                "main.o",
                "a.o",
                "b.o",
                "c.o",
                "d.o",
            ],
        );
        match &seen {
            None => seen = Some(stderr),
            Some(first) => assert_eq!(first, &stderr, "{threads} threads differ"),
        }
    }
    let stderr = seen.unwrap();
    let order: Vec<&str> = stderr
        .lines()
        .filter_map(|line| line.strip_prefix("qld: error: undefined symbol: "))
        .collect();
    assert_eq!(
        order,
        ["missing_a", "missing_b", "missing_c", "missing_d"],
        "{stderr}"
    );
}

/// `--error-limit` truncates with lld's message, and `--error-limit=0`
/// shows everything.
#[test]
fn error_limit_matches_lld() {
    require!("cc");
    let dir = scratch("error-limit");
    let mut source = String::new();
    let mut body = String::from("int main(void) { return 0");
    for index in 0..30 {
        source.push_str(&format!("int missing_{index:02}(void);\n"));
        body.push_str(&format!(" + missing_{index:02}()"));
    }
    body.push_str("; }\n");
    source.push_str(&body);
    compile(&dir, "main", &source, &[]);

    // Forked (the default) and not: the two report their status by
    // different routes, and `qld_err` pins the exit code of each.
    for fork in [&[][..], &["--no-fork"][..]] {
        let mut args = fork.to_vec();
        args.extend_from_slice(&["-o", "out", "main.o"]);
        let stderr = qld_err(&dir, &args);
        let count = stderr.matches("undefined symbol: ").count();
        assert_eq!(count, 20, "lld's default limit is 20:\n{stderr}");
        assert!(
            stderr.contains(
                "qld: error: too many errors emitted, stopping now (use --error-limit=0 to see \
                 all errors)"
            ),
            "{stderr}"
        );
    }

    let stderr = qld_err(
        &dir,
        &["--no-fork", "--error-limit=3", "-o", "out", "main.o"],
    );
    assert_eq!(stderr.matches("undefined symbol: ").count(), 3, "{stderr}");

    let stderr = qld_err(
        &dir,
        &["--no-fork", "--error-limit=0", "-o", "out", "main.o"],
    );
    assert_eq!(stderr.matches("undefined symbol: ").count(), 30, "{stderr}");
    assert!(!stderr.contains("too many errors"), "{stderr}");
}

/// `--color-diagnostics=always` colours the severity the way lld does, and
/// `never` (the default when stderr is not a terminal) leaves it alone.
#[test]
fn color_diagnostics() {
    require!("cc");
    let dir = scratch("color");
    compile(&dir, "main", MAIN, &[]);
    let plain = qld_err(&dir, &["--no-fork", "-o", "out", "main.o"]);
    assert!(!plain.contains('\x1b'), "{plain}");

    let colored = qld_err(
        &dir,
        &[
            "--no-fork",
            "--color-diagnostics=always",
            "-o",
            "out",
            "main.o",
        ],
    );
    assert!(
        colored.contains("qld: \x1b[0;31merror: \x1b[0mundefined symbol: missing"),
        "{colored:?}"
    );

    let never = qld_err(
        &dir,
        &[
            "--no-fork",
            "--color-diagnostics=never",
            "-o",
            "out",
            "main.o",
        ],
    );
    assert_eq!(never, plain);

    // Forked, so the escapes go through the parent's stderr relay.
    let forked = qld_err(&dir, &["--color-diagnostics=always", "-o", "out", "main.o"]);
    assert_eq!(forked, colored);
    // And `auto` stays off when stderr is a pipe, as it is here.
    let auto = qld_err(&dir, &["--color-diagnostics=auto", "-o", "out", "main.o"]);
    assert!(!auto.contains('\x1b'), "{auto:?}");
}

/// `--fatal-warnings` turns a warning into an error and **fails the link**;
/// `-w` drops warnings and cancels it, as in lld.
///
/// Run both ways round: `qld` links in a child process by default, and the
/// child reports its status from the output-complete hook, a different path
/// from the value `--no-fork` returns. A promoted warning has to fail the
/// link on both, and it did not on the forking one. Both sources of warning
/// are covered too: `-z <unknown>` comes from the option parser before the
/// link starts, the missing entry symbol from the ELF driver during it.
#[test]
fn fatal_warnings_fail_the_link() {
    require!("cc");
    let dir = scratch("fatal-warnings");
    // No `_start`, so the link itself warns about the entry symbol.
    compile(&dir, "main", "int foo(void) { return 0; }\n", &[]);

    for fork in [&[][..], &["--no-fork"][..]] {
        let how = *fork.first().unwrap_or(&"--fork");
        for (source, extra) in [
            ("the option parser", &["-z", "qld-bogus-keyword"][..]),
            ("the link", &[][..]),
        ] {
            let mut args = fork.to_vec();
            args.extend_from_slice(&["-o", "out", "main.o"]);
            args.extend_from_slice(extra);

            let output = qld(&dir, &args);
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            assert!(
                output.status.success(),
                "{how}, {source}: a warning alone does not fail: {stderr}"
            );
            assert!(
                stderr.contains("qld: warning: "),
                "{how}, {source}: a warning is expected: {stderr}"
            );

            let mut fatal = args.clone();
            fatal.push("--fatal-warnings");
            let output = qld(&dir, &fatal);
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            assert_eq!(
                output.status.code(),
                Some(1),
                "{how}, {source}: GNU ld and lld exit 1 here: {stderr}"
            );
            assert!(
                stderr.contains("qld: error: ") && !stderr.contains("qld: warning: "),
                "{how}, {source}: every warning is now an error: {stderr}"
            );

            let mut quiet = fatal.clone();
            quiet.push("-w");
            let output = qld(&dir, &quiet);
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            assert!(
                output.status.success(),
                "{how}, {source}: -w cancels --fatal-warnings: {stderr}"
            );
            assert_eq!(stderr, "", "{how}, {source}: -w prints nothing");
        }
    }
}

/// The five fatal-error messages qld shares with GNU ld: a missing library,
/// an unknown option, a file that is not an object, and a truncated one.
#[test]
fn fatal_errors_are_gnu_shaped() {
    require!("cc");
    let dir = scratch("fatal");
    compile(&dir, "main", "int main(void) { return 0; }\n", &[]);

    let stderr = qld_err(&dir, &["--no-fork", "-o", "out", "main.o", "-lqld-no-such"]);
    assert!(
        stderr.starts_with("qld: error: cannot find -lqld-no-such"),
        "GNU ld says `cannot find -lfoo`: {stderr}"
    );

    let stderr = qld_err(
        &dir,
        &["--no-fork", "-o", "out", "--qld-no-such-option", "main.o"],
    );
    assert_eq!(
        stderr,
        "qld: error: unrecognized option '--qld-no-such-option'\n\
         qld: use the --help option for usage information\n",
        "GNU ld's wording and usage hint"
    );

    // Binary junk: text would be taken for a linker script, as lld does.
    let junk: Vec<u8> = (0..512u32)
        .map(|i| (i.wrapping_mul(97) ^ 0xa5) as u8)
        .collect();
    fs::write(dir.join("junk.o"), &junk).unwrap();
    let stderr = qld_err(&dir, &["--no-fork", "-o", "out", "junk.o"]);
    assert_eq!(
        stderr, "qld: error: junk.o: file not recognized: file format not recognized\n",
        "BFD's wording"
    );

    let mut truncated = fs::read(dir.join("main.o")).unwrap();
    truncated.truncate(48);
    fs::write(dir.join("short.o"), &truncated).unwrap();
    let stderr = qld_err(&dir, &["--no-fork", "-o", "out", "short.o"]);
    assert!(stderr.starts_with("qld: error: "), "{stderr}");
    assert!(stderr.contains("short.o"), "{stderr}");
}
