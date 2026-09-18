//! Section ordering tests (workstream W29): `--symbol-ordering-file` and
//! `--call-graph-profile-sort`.
//!
//! Inputs are assembled from source with the host C compiler driver, so the
//! expected orders are exact. When lld is installed (`QLD_TEST_LLD`), the
//! same links run through it and the resulting symbol address order must
//! match. Tests print `SKIPPED:` when the compiler is missing, and fail
//! then instead when `QLD_REQUIRE_TOOLS` is set.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use common::tools::{skip, tools, tools_required};
use qld::elf::read::{Elf64Le, ElfFile, SectionIndex, Source};

/// A fresh, empty directory for one test.
fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("ordering-tests")
        .join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn run(dir: &Path, program: &Path, args: &[&str]) -> Output {
    Command::new(program)
        .args(args)
        .current_dir(dir)
        .env("LC_ALL", "C")
        .output()
        .unwrap_or_else(|e| panic!("cannot run {}: {e}", program.display()))
}

fn run_ok(dir: &Path, program: &Path, args: &[&str]) -> Output {
    let output = run(dir, program, args);
    assert!(
        output.status.success(),
        "`{} {}` failed:\n{}{}",
        program.display(),
        args.join(" "),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn qld(dir: &Path, args: &[&str]) -> Output {
    run_ok(dir, Path::new(env!("CARGO_BIN_EXE_qld")), args)
}

/// The C compiler, or `None` (after printing why) when the test must skip.
fn compiler() -> Option<PathBuf> {
    match &tools().cc {
        Some(cc) if tools().host.as_ref().is_some_and(|h| h.arch == "x86_64") => Some(cc.clone()),
        _ => {
            assert!(!tools_required(), "no x86-64 C compiler");
            skip("no x86-64 C compiler");
            None
        }
    }
}

/// Assembles `source` into `name.o`.
fn assemble(cc: &Path, dir: &Path, name: &str, source: &str) {
    let file = format!("{name}.s");
    fs::write(dir.join(&file), source).unwrap();
    run_ok(dir, cc, &["-c", &file, "-o", &format!("{name}.o")]);
}

/// The defined symbols of an output with their addresses, in address order
/// (then name order).
fn symbols_by_address(path: &Path) -> Vec<(u64, String)> {
    let data = fs::read(path).unwrap();
    let elf = ElfFile::<Elf64Le>::parse(&data, Source::new(path)).unwrap();
    let (index, _) = elf.section_by_name(b".symtab").expect("no .symtab");
    let table = elf.symbol_table(index).unwrap();
    let mut out: Vec<(u64, String)> = table
        .iter()
        .filter_map(|s| s.ok())
        .filter(|s| matches!(s.section, SectionIndex::Section(_)) && !s.name.is_empty())
        .filter(|s| s.kind() == 1 || s.kind() == 2)
        .map(|s| (s.value, String::from_utf8_lossy(s.name).into_owned()))
        .collect();
    out.sort();
    out
}

fn names(symbols: &[(u64, String)]) -> Vec<&str> {
    symbols.iter().map(|(_, n)| n.as_str()).collect()
}

/// Links with lld too, when it is installed, and checks that it put the
/// symbols in the same order.
fn compare_with_lld(dir: &Path, args: &[&str], ours: &[(u64, String)]) {
    let Some(lld) = &tools().lld else {
        return;
    };
    let mut lld_args: Vec<&str> = args.to_vec();
    lld_args.extend(["-o", "lld.out"]);
    run_ok(dir, lld, &lld_args);
    let theirs = symbols_by_address(&dir.join("lld.out"));
    assert_eq!(names(ours), names(&theirs), "lld orders differently");
}

const FUNCTIONS: &str = r#"
    .section .text.fa,"ax",@progbits
    .globl fa
    .type fa,@function
fa: ret
    .size fa, 1
    .section .text.fb,"ax",@progbits
    .globl fb
    .type fb,@function
fb: ret
    .size fb, 1
    .section .text.local,"ax",@progbits
    .type loc,@function
loc: ret
    .size loc, 1
    .section .text.fc,"ax",@progbits
    .globl fc
    .type fc,@function
fc: call loc
    ret
    .size fc, 6
    .section .text.fd,"ax",@progbits
    .globl fd
    .type fd,@function
fd: ret
    .size fd, 1
    .section .data.d1,"aw",@progbits
    .globl d1
    .type d1,@object
d1: .quad 1
    .size d1, 8
    .section .data.d2,"aw",@progbits
    .globl d2
    .type d2,@object
d2: .quad 2
    .size d2, 8
"#;

const START: &str = r#"
    .section .text._start,"ax",@progbits
    .globl _start
    .type _start,@function
_start: call fa
    call undefined_fn
    ret
    .size _start, 11
    .section .text.fe,"ax",@progbits
    .globl fe
    .type fe,@function
fe: ret
    .size fe, 1
    .globl absolute_sym
    absolute_sym = 0x1234
"#;

#[test]
fn symbol_ordering_file_orders_sections_and_warns() {
    let Some(cc) = compiler() else { return };
    let dir = scratch("symbol-ordering");
    assemble(&cc, &dir, "a", FUNCTIONS);
    assemble(&cc, &dir, "b", START);
    fs::write(
        dir.join("order.txt"),
        "fe\n  loc  \n# a comment\n\nfd\nmissing\nfe\nd2\nundefined_fn\nabsolute_sym\n",
    )
    .unwrap();
    let args = [
        "a.o",
        "b.o",
        "--symbol-ordering-file",
        "order.txt",
        "--warn-unresolved-symbols",
    ];
    let mut ours: Vec<&str> = args.to_vec();
    ours.extend(["-o", "qld.out"]);
    let output = qld(&dir, &ours);
    let symbols = symbols_by_address(&dir.join("qld.out"));
    assert_eq!(
        names(&symbols),
        ["fe", "loc", "fd", "fa", "fb", "fc", "_start", "d2", "d1"]
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    for expected in [
        "warning: order.txt: duplicate ordered symbol: fe",
        "warning: b.o: unable to order undefined symbol: undefined_fn",
        "warning: b.o: unable to order absolute symbol: absolute_sym",
        "warning: symbol ordering file: no such symbol: missing",
    ] {
        assert!(
            stderr.contains(expected),
            "missing `{expected}` in:\n{stderr}"
        );
    }
    compare_with_lld(&dir, &args, &symbols);

    // --no-warn-symbol-ordering silences all of it, and changes nothing else.
    ours.push("--no-warn-symbol-ordering");
    let output = qld(&dir, &ours);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("order"), "unexpected warnings:\n{stderr}");
    assert_eq!(symbols_by_address(&dir.join("qld.out")), symbols);
}

#[test]
fn symbol_ordering_reports_discarded_sections() {
    let Some(cc) = compiler() else { return };
    let dir = scratch("symbol-ordering-gc");
    assemble(&cc, &dir, "a", FUNCTIONS);
    assemble(&cc, &dir, "b", START);
    fs::write(dir.join("order.txt"), "fd\nfa\n").unwrap();
    let output = qld(
        &dir,
        &[
            "a.o",
            "b.o",
            "--gc-sections",
            "--symbol-ordering-file=order.txt",
            "--warn-unresolved-symbols",
            "-o",
            "qld.out",
        ],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("warning: a.o: unable to order discarded symbol: fd"),
        "{stderr}"
    );
    let symbols = symbols_by_address(&dir.join("qld.out"));
    assert_eq!(names(&symbols), ["fa", "_start"]);
}

#[test]
fn symbol_ordering_is_deterministic_across_threads() {
    let Some(cc) = compiler() else { return };
    let dir = scratch("symbol-ordering-threads");
    assemble(&cc, &dir, "a", FUNCTIONS);
    assemble(&cc, &dir, "b", START);
    fs::write(dir.join("order.txt"), "fc\nd2\nfb\n").unwrap();
    let mut first: Option<Vec<u8>> = None;
    for threads in ["1", "2", "8"] {
        let out = format!("t{threads}.out");
        qld(
            &dir,
            &[
                "a.o",
                "b.o",
                "--symbol-ordering-file",
                "order.txt",
                "--warn-unresolved-symbols",
                &format!("--threads={threads}"),
                "-o",
                &out,
            ],
        );
        let bytes = fs::read(dir.join(&out)).unwrap();
        match &first {
            None => first = Some(bytes),
            Some(first) => assert!(*first == bytes, "output differs with {threads} threads"),
        }
    }
}
