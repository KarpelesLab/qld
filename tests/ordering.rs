//! Section ordering tests (workstream W29): `--symbol-ordering-file` and
//! `--call-graph-profile-sort`.
//!
//! Inputs are assembled from source with the host C compiler driver, so the
//! expected orders are exact. When lld is installed (`QLD_TEST_LLD`), the
//! same links run through it and the resulting symbol address order must
//! match. Tests print `SKIPPED:` on a host whose toolchain does not build
//! x86-64 ELF objects, and when the compiler is missing; a missing
//! compiler fails instead when `QLD_REQUIRE_TOOLS` is set.

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
/// Whether the host builds and runs the x86-64 ELF objects these tests
/// assemble, as the other ELF-only suites ask. A compiler that targets
/// something else (the MinGW `cc` of a Windows runner) skips; a missing
/// compiler is the caller's to report, so that `QLD_REQUIRE_TOOLS` still
/// fails on Linux.
fn elf_host() -> bool {
    if !cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        skip("host is not x86-64 Linux");
        return false;
    }
    match tools().host.as_ref() {
        Some(host) if !(host.arch == "x86_64" && host.is_linux()) => {
            skip(format!(
                "the C compiler builds {host} objects, not x86-64 ELF"
            ));
            false
        }
        _ => true,
    }
}

fn compiler() -> Option<PathBuf> {
    if !elf_host() {
        return None;
    }
    match &tools().cc {
        Some(cc) => Some(cc.clone()),
        None => {
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
    for threads in ["1", "2"] {
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

/// A call graph over `f0`..`f7` (sizes vary): `(caller, callee, weight)`.
const GRAPH: [(u32, u32, u64); 9] = [
    (0, 3, 100),
    (3, 5, 400),
    (5, 3, 50),
    (1, 2, 1000),
    (2, 7, 10),
    (6, 1, 30),
    (4, 4, 90),
    (7, 0, 5),
    (1, 6, 300),
];

/// Assembly for `f0`..`f7` with the call graph profile section of `GRAPH`,
/// hand-written so that any ELF assembler takes it.
fn graph_source() -> String {
    let mut s = String::new();
    for (i, size) in [16, 300, 40, 8, 1000, 64, 12, 200].into_iter().enumerate() {
        s.push_str(&format!(
            "\t.section .text.f{i},\"ax\",@progbits\n\t.globl f{i}\n\t.type f{i},@function\nf{i}:\n\t.skip {size}, 0x90\n\t.size f{i}, {size}\n"
        ));
    }
    s.push_str("\t.section .llvm.call-graph-profile,\"eM\",@0x6fff4c09,8\n");
    for (from, to, weight) in GRAPH {
        s.push_str(&format!(
            "\t.reloc ., R_X86_64_NONE, f{from}\n\t.reloc ., R_X86_64_NONE, f{to}\n\t.quad {weight}\n"
        ));
    }
    s.push_str("\t.section .text._start,\"ax\",@progbits\n\t.globl _start\n_start: ret\n");
    s
}

#[test]
fn call_graph_profile_sort_matches_lld() {
    let Some(cc) = compiler() else { return };
    let dir = scratch("call-graph");
    assemble(&cc, &dir, "g", &graph_source());
    for (algorithm, expected) in [
        ("hfsort", ["f1", "f2", "f6", "f7", "f0", "f3", "f5", "f4"]),
        ("cdsort", ["f6", "f1", "f2", "f7", "f0", "f3", "f5", "f4"]),
    ] {
        let option = format!("--call-graph-profile-sort={algorithm}");
        let args = ["g.o", option.as_str()];
        qld(&dir, &["g.o", &option, "-o", "qld.out"]);
        let symbols = symbols_by_address(&dir.join("qld.out"));
        assert_eq!(names(&symbols), expected, "{algorithm}");
        compare_with_lld(&dir, &args, &symbols);
    }
    // Without the option, qld keeps GNU ld's order.
    qld(&dir, &["g.o", "-o", "plain.out"]);
    let symbols = symbols_by_address(&dir.join("plain.out"));
    assert_eq!(
        names(&symbols),
        ["f0", "f1", "f2", "f3", "f4", "f5", "f6", "f7"]
    );
}

#[test]
fn call_graph_ordering_file_and_symbol_order() {
    let Some(cc) = compiler() else { return };
    let dir = scratch("call-graph-file");
    assemble(&cc, &dir, "g", &graph_source());
    fs::write(
        dir.join("graph.txt"),
        "f7 f6 500\nf6 f5 400\nf2 missing 3\nf0 f1 1\n",
    )
    .unwrap();
    let args = [
        "g.o",
        "--call-graph-ordering-file=graph.txt",
        "--call-graph-profile-sort=hfsort",
        "--print-symbol-order=order.txt",
    ];
    let mut ours = args.to_vec();
    ours.extend(["-o", "qld.out"]);
    let output = qld(&dir, &ours);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("graph.txt: no such symbol: missing"),
        "{stderr}"
    );
    let symbols = symbols_by_address(&dir.join("qld.out"));
    let order = fs::read_to_string(dir.join("order.txt")).unwrap();
    let printed: Vec<&str> = order.lines().collect();
    assert_eq!(printed, &names(&symbols)[..printed.len()]);
    compare_with_lld(&dir, &args, &symbols);

    // The symbol ordering file goes first, then the call graph.
    fs::write(dir.join("syms.txt"), "f4\nf2\n").unwrap();
    let args = [
        "g.o",
        "--symbol-ordering-file=syms.txt",
        "--call-graph-profile-sort=hfsort",
    ];
    let mut ours = args.to_vec();
    ours.extend(["-o", "qld.out"]);
    qld(&dir, &ours);
    let symbols = symbols_by_address(&dir.join("qld.out"));
    assert_eq!(&names(&symbols)[..2], ["f4", "f2"]);
    compare_with_lld(&dir, &args, &symbols);
}

const FOLDABLE: &str = r#"
    .section .text.g1,"ax",@progbits
    .globl g1
    .type g1,@function
g1: movl $1, %eax
    ret
    .size g1, 6
    .section .text.g2,"ax",@progbits
    .globl g2
    .type g2,@function
g2: movl $2, %eax
    ret
    .size g2, 6
    .section .text.g3,"ax",@progbits
    .globl g3
    .type g3,@function
g3: movl $1, %eax
    ret
    .size g3, 6
    .section .text._start,"ax",@progbits
    .globl _start
    .type _start,@function
_start: call g1
    call g2
    call g3
    ret
    .size _start, 16
"#;

#[test]
fn symbols_of_folded_sections_order_the_kept_section() {
    let Some(cc) = compiler() else { return };
    let dir = scratch("symbol-ordering-icf");
    assemble(&cc, &dir, "f", FOLDABLE);
    // g3 folds into g1: listing g3 first moves g1's section first.
    fs::write(dir.join("order.txt"), "g3\ng2\n").unwrap();
    let args = [
        "f.o",
        "--icf=all",
        "--symbol-ordering-file=order.txt",
        "--no-warn-symbol-ordering",
    ];
    let mut ours = args.to_vec();
    ours.extend(["-o", "qld.out"]);
    qld(&dir, &ours);
    let symbols = symbols_by_address(&dir.join("qld.out"));
    let first = symbols.first().unwrap();
    let g1 = symbols.iter().find(|(_, n)| n == "g1").unwrap();
    let g3 = symbols.iter().find(|(_, n)| n == "g3").unwrap();
    assert_eq!(g1.0, g3.0, "g3 is folded into g1");
    assert_eq!(first.0, g1.0, "{symbols:?}");
    compare_with_lld(&dir, &args, &symbols);
}
