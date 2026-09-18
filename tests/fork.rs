//! `--fork` (the default on Unix) against `--no-fork`: the `qld` binary
//! links in a child process and returns once the output is complete. These
//! tests check what a caller sees: the exit status, stdout and stderr
//! through pipes (in order when they are one pipe), and a complete output
//! file as soon as `qld` returns.
//!
//! Inputs are assembled with the system `as`; a test prints `SKIPPED:` and
//! passes when it is missing or the host is not x86-64 Linux.

#![cfg(unix)]

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const QLD: &str = env!("CARGO_BIN_EXE_qld");

const EXIT_42: &str = "
    .globl _start
    .text
_start:
    mov $60, %eax
    mov $42, %edi
    syscall
";

/// A directory with `start.o` assembled from [`EXIT_42`], or `None` (after
/// printing why) when the host cannot run the test.
fn setup(name: &str) -> Option<PathBuf> {
    if !cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        println!("SKIPPED: host is not x86-64 Linux");
        return None;
    }
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("fork-tests")
        .join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("start.s"), EXIT_42).unwrap();
    match Command::new("as")
        .args(["--64", "-o", "start.o", "start.s"])
        .current_dir(&dir)
        .status()
    {
        Ok(status) if status.success() => Some(dir),
        _ => {
            println!("SKIPPED: `as` not found or failed");
            None
        }
    }
}

fn qld(dir: &Path, args: &[&str]) -> Output {
    Command::new(QLD)
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap()
}

/// Runs qld with stdout and stderr on one pipe, as `2>&1 | ...` does.
fn qld_combined(dir: &Path, args: &[&str]) -> (bool, String) {
    let (mut reader, writer) = std::io::pipe().unwrap();
    let mut child = Command::new(QLD)
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(writer.try_clone().unwrap())
        .stderr(writer)
        .spawn()
        .unwrap();
    let status = child.wait().unwrap();
    // The command above holds the last copies of the write end: reading to
    // the end must finish as soon as qld has returned, even though its
    // child may still be cleaning up.
    let mut text = String::new();
    reader.read_to_string(&mut text).unwrap();
    (status.success(), text)
}

fn exit_code(path: &Path) -> Option<i32> {
    Command::new(path).status().unwrap().code()
}

#[test]
fn output_is_complete_when_qld_returns() {
    let Some(dir) = setup("complete") else {
        return;
    };
    for round in 0..3 {
        let _ = fs::remove_file(dir.join("out"));
        let output = qld(&dir, &["-o", "out", "start.o", "--fork"]);
        assert!(output.status.success(), "round {round}: {output:?}");
        assert_eq!(exit_code(&dir.join("out")), Some(42), "round {round}");
    }
}

#[test]
fn stdout_and_stderr_match_no_fork() {
    let Some(dir) = setup("streams") else {
        return;
    };
    let args = ["-o", "out", "start.o", "--print-map", "-z", "qld-bogus"];
    let forked = qld(&dir, &args);
    let mut no_fork_args = args.to_vec();
    no_fork_args.push("--no-fork");
    let direct = qld(&dir, &no_fork_args);
    assert!(forked.status.success() && direct.status.success());
    assert!(
        String::from_utf8_lossy(&forked.stdout).contains("_start"),
        "the map is on stdout: {forked:?}"
    );
    assert_eq!(forked.stdout, direct.stdout);
    assert_eq!(forked.stderr, direct.stderr);
    let stderr = String::from_utf8_lossy(&forked.stderr);
    assert_eq!(
        stderr.matches("qld-bogus").count(),
        1,
        "one warning: {stderr}"
    );
}

#[test]
fn one_pipe_for_both_streams_keeps_their_order() {
    let Some(dir) = setup("combined") else {
        return;
    };
    let args = ["-o", "out", "start.o", "-z", "qld-bogus", "--print-map"];
    let (ok, forked) = qld_combined(&dir, &args);
    assert!(ok, "{forked}");
    let mut no_fork_args = args.to_vec();
    no_fork_args.push("--no-fork");
    let (ok, direct) = qld_combined(&dir, &no_fork_args);
    assert!(ok, "{direct}");
    assert_eq!(forked, direct);
    let warning = forked.find("qld-bogus").unwrap();
    let map = forked.find("_start").unwrap();
    assert!(warning < map, "the warning comes before the map: {forked}");
}

#[test]
fn failures_keep_their_status_and_messages() {
    let Some(dir) = setup("failure") else {
        return;
    };
    fs::write(
        dir.join("call.s"),
        ".globl _start\n.text\n_start:\n    call missing_function_qld\n",
    )
    .unwrap();
    assert!(
        Command::new("as")
            .args(["--64", "-o", "call.o", "call.s"])
            .current_dir(&dir)
            .status()
            .unwrap()
            .success()
    );
    for extra in [None, Some("--no-fork")] {
        let mut args = vec!["-o", "bad", "call.o"];
        args.extend(extra);
        let output = qld(&dir, &args);
        assert_eq!(output.status.code(), Some(1), "{extra:?}: {output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("undefined symbol: missing_function_qld"),
            "{extra:?}: {stderr}"
        );
        assert!(!dir.join("bad").exists());

        let mut args = vec!["-o", "bad", "no-such-input.o"];
        args.extend(extra);
        let output = qld(&dir, &args);
        assert_eq!(output.status.code(), Some(1), "{extra:?}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("no-such-input.o"),
            "{extra:?}: {output:?}"
        );
    }
}

#[test]
fn output_to_dev_stdout_links_in_process() {
    let Some(dir) = setup("dev-stdout") else {
        return;
    };
    let output = qld(&dir, &["-o", "/dev/stdout", "start.o"]);
    assert!(output.status.success(), "{output:?}");
    assert!(output.stdout.starts_with(b"\x7fELF"), "{output:?}");
}

#[test]
fn an_inherited_child_marker_is_ignored() {
    let Some(dir) = setup("marker") else {
        return;
    };
    // As if a plugin subprocess of a forked link ran qld: the variable names
    // a process that is not this qld's parent.
    let output = Command::new(QLD)
        .args(["-o", "out", "start.o"])
        .current_dir(&dir)
        .env("QLD_FORK_CHILD", "1:oe")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(exit_code(&dir.join("out")), Some(42));
}

#[test]
fn help_and_version_do_not_fork() {
    let output = Command::new(QLD).arg("--version").output().unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("GNU"));
    let output = Command::new(QLD).arg("--help").output().unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("--no-fork"));
}
