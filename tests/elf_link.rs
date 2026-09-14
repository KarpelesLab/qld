//! Focused integration tests for the ELF link driver (workstream W8).
//!
//! Inputs are assembled with the system `as` or compiled with `cc -c`, linked
//! with the `qld` binary under test, run, and inspected with `readelf`. A
//! test prints `SKIPPED:` and passes when a tool it needs is missing.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;

use qld::args::{InputAttrs, InputKind, LinkOptions, OutputKind};
use qld::diag::Collect;

/// A fresh, empty directory for one test.
fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("elf-link-tests")
        .join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Finds `name` in `PATH`.
fn tool(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// Whether the host can run x86-64 Linux executables.
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
        .unwrap_or_else(|e| panic!("cannot run {program}: {e}"))
}

fn run_ok(dir: &Path, program: &str, args: &[&str]) -> String {
    let output = run(dir, program, args);
    assert!(
        output.status.success(),
        "`{program} {}` failed: {}{}",
        args.join(" "),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn qld(dir: &Path, args: &[&str]) -> Output {
    run(dir, env!("CARGO_BIN_EXE_qld"), args)
}

fn qld_ok(dir: &Path, args: &[&str]) -> String {
    let output = qld(dir, args);
    assert!(
        output.status.success(),
        "qld {} failed:\n{}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn assemble(dir: &Path, name: &str, source: &str) {
    let path = dir.join(format!("{name}.s"));
    fs::write(&path, source).unwrap();
    run_ok(
        dir,
        "as",
        &["--64", "-o", &format!("{name}.o"), &format!("{name}.s")],
    );
}

fn compile(dir: &Path, name: &str, source: &str, flags: &[&str]) {
    fs::write(dir.join(format!("{name}.c")), source).unwrap();
    let mut args = vec!["-c", "-O2", "-fno-pie"];
    args.extend_from_slice(flags);
    let src = format!("{name}.c");
    let obj = format!("{name}.o");
    args.extend_from_slice(&[src.as_str(), "-o", obj.as_str()]);
    run_ok(dir, "cc", &args);
}

fn readelf(dir: &Path, args: &[&str]) -> String {
    let mut all = vec!["-W"];
    all.extend_from_slice(args);
    run_ok(dir, "readelf", &all)
}

/// Exit code of running `./out` in `dir`.
fn exit_code(dir: &Path, binary: &str) -> i32 {
    Command::new(dir.join(binary))
        .current_dir(dir)
        .status()
        .unwrap()
        .code()
        .unwrap_or(-1)
}

const EXIT_42: &str = "
    .globl _start
    .text
_start:
    mov $60, %eax
    mov $42, %edi
    syscall
";

#[test]
fn minimal_executable_runs_and_is_well_formed() {
    require!("as", "readelf");
    let dir = scratch("minimal");
    assemble(&dir, "start", EXIT_42);
    qld_ok(&dir, &["-o", "out", "start.o"]);
    assert_eq!(exit_code(&dir, "out"), 42);
    let headers = readelf(&dir, &["-h", "-l", "-s", "out"]);
    assert!(headers.contains("EXEC (Executable file)"), "{headers}");
    assert!(headers.contains("GNU_STACK"), "{headers}");
    assert!(!headers.contains("INTERP"), "{headers}");
    let entry = headers
        .lines()
        .find(|l| l.contains("Entry point address:"))
        .and_then(|l| l.split_whitespace().last())
        .map(|v| u64::from_str_radix(v.trim_start_matches("0x"), 16).unwrap())
        .unwrap();
    let start = headers
        .lines()
        .find(|l| l.ends_with(" _start"))
        .and_then(|l| l.split_whitespace().nth(1))
        .map(|v| u64::from_str_radix(v, 16).unwrap())
        .unwrap();
    assert_eq!(entry, start);
    assert!(entry >= 0x401000, "text starts on its own page: {entry:#x}");
}

#[test]
fn undefined_symbols_are_reported_with_locations() {
    require!("as");
    let dir = scratch("undefined");
    assemble(
        &dir,
        "main",
        "
    .globl _start
    .text
_start:
    nop
    call missing_function_qld
    call missing_function_qld
    ret
",
    );
    let output = qld(&dir, &["-o", "out", "main.o"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("error: undefined symbol: missing_function_qld"),
        "{stderr}"
    );
    assert!(
        stderr.contains(">>> referenced by main.o:(.text+0x2)"),
        "{stderr}"
    );
    assert!(
        stderr.contains(">>> referenced by main.o:(.text+0x7)"),
        "{stderr}"
    );
    assert!(!dir.join("out").exists(), "no output after errors");
}

#[test]
fn weak_undefined_symbols_resolve_to_zero() {
    require!("as");
    let dir = scratch("weak-undefined");
    assemble(
        &dir,
        "main",
        "
    .weak optional_qld
    .globl _start
    .text
_start:
    lea optional_qld(%rip), %rdi
    xor %eax, %eax
    test %rdi, %rdi
    setne %al
    mov %eax, %edi
    mov $60, %eax
    syscall
",
    );
    qld_ok(&dir, &["-o", "out", "main.o"]);
    assert_eq!(exit_code(&dir, "out"), 0);
}

#[test]
fn duplicate_symbols_are_reported() {
    require!("as");
    let dir = scratch("duplicate");
    let source = "
    .globl dup_qld, _start
    .text
_start:
dup_qld:
    ret
";
    assemble(&dir, "a", source);
    assemble(
        &dir,
        "b",
        &source.replace("_start, ", "").replace("_start:\n", ""),
    );
    let output = qld(&dir, &["-o", "out", "a.o", "b.o"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("duplicate symbol: dup_qld"), "{stderr}");
    assert!(stderr.contains("defined at a.o"), "{stderr}");
    assert!(stderr.contains("defined at b.o"), "{stderr}");
    qld_ok(
        &dir,
        &["--allow-multiple-definition", "-o", "out", "a.o", "b.o"],
    );
}

#[test]
fn linker_defined_symbols_mark_the_image() {
    require!("as", "readelf");
    let dir = scratch("defined");
    assemble(
        &dir,
        "main",
        "
    .globl _start
    .text
_start:
    lea __ehdr_start(%rip), %rdi
    cmpl $0x464c457f, (%rdi)
    jne fail
    lea __executable_start(%rip), %rsi
    cmp %rdi, %rsi
    jne fail
    lea _end(%rip), %rax
    lea __bss_start(%rip), %rcx
    cmp %rcx, %rax
    jb fail
    lea buffer(%rip), %rdx
    cmp %rcx, %rdx
    jb fail
    cmp %rax, %rdx
    jae fail
    mov $60, %eax
    xor %edi, %edi
    syscall
fail:
    mov $60, %eax
    mov $1, %edi
    syscall
    .bss
buffer:
    .zero 4096
",
    );
    qld_ok(&dir, &["-o", "out", "main.o"]);
    assert_eq!(exit_code(&dir, "out"), 0);
}

#[test]
fn gotpcrelx_is_relaxed_and_gotpcrel_uses_the_got() {
    require!("as", "readelf");
    let dir = scratch("gotpcrel");
    assemble(
        &dir,
        "main",
        "
    .globl _start
    .text
_start:
    movq value_qld@GOTPCREL(%rip), %rax
    movl (%rax), %edi
    mov $60, %eax
    syscall
    .data
value_qld:
    .long 17
",
    );
    qld_ok(&dir, &["-o", "out", "main.o"]);
    assert_eq!(exit_code(&dir, "out"), 17);
    let sections = readelf(&dir, &["-S", "out"]);
    assert!(!sections.contains(".got"), "relaxed away: {sections}");

    // With --no-relax the GOT entry stays.
    qld_ok(&dir, &["--no-relax", "-o", "out2", "main.o"]);
    assert_eq!(exit_code(&dir, "out2"), 17);
    let sections = readelf(&dir, &["-S", "out2"]);
    assert!(sections.contains(".got"), "{sections}");
}

#[test]
fn tls_local_exec_uses_the_thread_pointer_layout() {
    require!("as", "readelf");
    let dir = scratch("tls");
    // No libc: the TLS block is never set up, so only check the offsets
    // qld computes, through the addresses readelf reports.
    assemble(
        &dir,
        "main",
        "
    .globl _start
    .text
_start:
    movq %fs:x_qld@tpoff, %rax
    mov $60, %eax
    xor %edi, %edi
    syscall
    .section .tdata,\"awT\",@progbits
    .p2align 3
x_qld:
    .quad 1
    .section .tbss,\"awT\",@nobits
    .p2align 3
y_qld:
    .zero 16
",
    );
    qld_ok(&dir, &["-o", "out", "main.o"]);
    let all = readelf(&dir, &["-l", "-S", "out"]);
    assert!(all.contains("TLS "), "{all}");
    // mov %fs:0xffffffffffffffe8,%rax: x is 24 bytes below the end of the
    // 24-byte TLS block (8 bytes of .tdata, 16 of .tbss).
    let text = readelf(&dir, &["-x", ".text", "out"]);
    let bytes: String = text
        .lines()
        .filter_map(|line| line.trim().strip_prefix("0x"))
        .flat_map(|line| line.split_whitespace().skip(1).take(4))
        .collect();
    assert!(bytes.starts_with("64488b0425e8ffffff"), "{text}");
}

#[test]
fn gc_sections_reports_and_explains() {
    require!("cc");
    let dir = scratch("gc");
    compile(
        &dir,
        "main",
        "
volatile int sink_qld;
__attribute__((noipa)) int used_function_qld(void) { return 3; }
int unused_function_qld(void) { return 4; }
void _start(void) {
    sink_qld = used_function_qld();
    for (;;) {
        __builtin_trap();
    }
}
",
        &[
            "-ffunction-sections",
            "-fno-asynchronous-unwind-tables",
            "-fno-stack-protector",
        ],
    );
    let stderr = qld_ok(
        &dir,
        &[
            "--gc-sections",
            "--print-gc-sections",
            "--why-live=used_function_qld",
            "--why-live=unused_function_qld",
            "-o",
            "out",
            "main.o",
        ],
    );
    assert!(
        stderr.contains("removing unused section '.text.unused_function_qld' in file 'main.o'"),
        "{stderr}"
    );
    assert!(
        stderr.contains("symbol unused_function_qld is removed by --gc-sections"),
        "{stderr}"
    );
    assert!(
        stderr.contains("live symbol: used_function_qld"),
        "{stderr}"
    );
}

#[test]
fn identical_code_folding() {
    require!("cc", "readelf");
    let dir = scratch("icf");
    compile(
        &dir,
        "a",
        "int fold_a_qld(int x) { return x * 13 + 5; }",
        &["-ffunction-sections"],
    );
    compile(
        &dir,
        "b",
        "int fold_b_qld(int x) { return x * 13 + 5; }",
        &["-ffunction-sections"],
    );
    assemble(
        &dir,
        "start",
        "
    .globl _start
    .text
_start:
    lea fold_a_qld(%rip), %rax
    lea fold_b_qld(%rip), %rcx
    xor %edi, %edi
    cmp %rax, %rcx
    sete %dil
    mov $60, %eax
    syscall
",
    );
    qld_ok(&dir, &["-o", "none", "start.o", "a.o", "b.o"]);
    assert_eq!(exit_code(&dir, "none"), 0, "not folded by default");
    // The addresses are compared, so safe mode must not fold.
    qld_ok(&dir, &["--icf=safe", "-o", "safe", "start.o", "a.o", "b.o"]);
    assert_eq!(exit_code(&dir, "safe"), 0);
    let stderr = qld_ok(
        &dir,
        &[
            "--icf=all",
            "--print-icf-sections",
            "-o",
            "all",
            "start.o",
            "a.o",
            "b.o",
        ],
    );
    assert_eq!(exit_code(&dir, "all"), 1, "folded: {stderr}");
    assert!(
        stderr.contains("selected section a.o:(.text.fold_a_qld)"),
        "{stderr}"
    );
    assert!(
        stderr.contains("removing identical section b.o:(.text.fold_b_qld)"),
        "{stderr}"
    );
    let symbols = readelf(&dir, &["-s", "all"]);
    let address = |name: &str| {
        symbols
            .lines()
            .find(|l| l.ends_with(name))
            .and_then(|l| l.split_whitespace().nth(1))
            .map(str::to_owned)
    };
    assert_eq!(address(" fold_a_qld"), address(" fold_b_qld"), "{symbols}");
}

#[test]
fn map_file_lists_sections_and_symbols() {
    require!("as");
    let dir = scratch("map");
    assemble(&dir, "start", EXIT_42);
    qld_ok(&dir, &["-Map", "out.map", "-o", "out", "start.o"]);
    let map = fs::read_to_string(dir.join("out.map")).unwrap();
    assert!(map.contains("VMA"), "{map}");
    assert!(map.contains(" .text"), "{map}");
    assert!(map.contains("start.o:(.text)"), "{map}");
    assert!(map.contains(" _start"), "{map}");
}

#[test]
fn input_scripts_expand_groups() {
    require!("as");
    let dir = scratch("script");
    assemble(
        &dir,
        "a",
        "
    .globl _start
    .text
_start:
    call exit_with_42_qld
",
    );
    assemble(
        &dir,
        "b",
        "
    .globl exit_with_42_qld
    .text
exit_with_42_qld:
    mov $60, %eax
    mov $42, %edi
    syscall
",
    );
    run_ok(&dir, "ar", &["rcs", "libb.a", "b.o"]);
    fs::write(
        dir.join("libgroup.so"),
        "/* script */ GROUP ( a.o libb.a )\n",
    )
    .unwrap();
    qld_ok(&dir, &["-o", "out", "libgroup.so"]);
    assert_eq!(exit_code(&dir, "out"), 42);
}

#[test]
fn position_independent_outputs_link() {
    require!("as", "readelf");
    let dir = scratch("pic-outputs");
    assemble(&dir, "start", EXIT_42);
    qld_ok(
        &dir,
        &["-pie", "--no-dynamic-linker", "-o", "pie", "start.o"],
    );
    assert_eq!(exit_code(&dir, "pie"), 42);
    let headers = readelf(&dir, &["-h", "-l", "-d", "pie"]);
    assert!(headers.contains("DYN (Position-Independent"), "{headers}");
    assert!(headers.contains("(FLAGS_1)"), "{headers}");
    assert!(!headers.contains("INTERP"), "{headers}");

    qld_ok(
        &dir,
        &[
            "-shared",
            "-soname",
            "libstart.so",
            "-o",
            "lib.so",
            "start.o",
        ],
    );
    let dynamic = readelf(&dir, &["-h", "-d", "--dyn-syms", "lib.so"]);
    assert!(dynamic.contains("DYN (Shared object file)"), "{dynamic}");
    assert!(
        dynamic.contains("Library soname: [libstart.so]"),
        "{dynamic}"
    );
    assert!(dynamic.contains(" _start"), "{dynamic}");
}

#[test]
fn output_is_identical_across_thread_counts() {
    require!("cc");
    let dir = scratch("threads");
    compile(
        &dir,
        "main",
        "
static const char text[] = \"determinism\";
int _start(void) { return text[3]; }
",
        &["-ffunction-sections", "-fdata-sections"],
    );
    let mut images = Vec::new();
    for threads in ["1", "2", "8"] {
        let out = format!("out{threads}");
        qld_ok(
            &dir,
            &[
                &format!("--threads={threads}"),
                "--build-id=sha1",
                "-o",
                &out,
                "main.o",
            ],
        );
        images.push(fs::read(dir.join(&out)).unwrap());
    }
    assert_eq!(images[0], images[1]);
    assert_eq!(images[0], images[2]);
}

/// Corrupting an object in many ways must give errors (or a link), never a
/// panic. Uses the library API with in-memory inputs, so each attempt is
/// cheap.
#[test]
fn corrupted_objects_never_panic() {
    require!("cc");
    let dir = scratch("corrupt");
    compile(
        &dir,
        "main",
        "
#include <stddef.h>
static const char *const table[] = {\"alpha\", \"beta\"};
__thread int counter_qld = 3;
int helper(int x) { return x + counter_qld; }
int _start(void) { return helper((int)(size_t)table[1][0]); }
",
        &[
            "-ffunction-sections",
            "-fdata-sections",
            "-fno-stack-protector",
        ],
    );
    let original = fs::read(dir.join("main.o")).unwrap();
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(2)
        .build()
        .unwrap();
    for round in 0..1000 {
        let mut data = original.clone();
        match round % 3 {
            0 => data.truncate((next() as usize) % original.len()),
            1 => {
                for _ in 0..4 {
                    let at = (next() as usize) % data.len();
                    data[at] ^= (next() as u8) | 1;
                }
            }
            _ => {
                // Corrupt within the headers and tables, where damage matters.
                let at = (next() as usize) % data.len().clamp(1, 512);
                let len = data.len();
                data[at % len] = next() as u8;
                let table = (next() as usize) % len;
                data[table] = 0xff;
            }
        }
        let mut options = LinkOptions::new();
        options.kind = OutputKind::StaticExecutable;
        options.output = Some(dir.join("out"));
        options.gc_sections = round % 2 == 0;
        options.push_input(
            InputKind::Bytes {
                name: format!("corrupt{round}.o"),
                data: Arc::from(data),
            },
            InputAttrs::default(),
        );
        let sink = Collect::new();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            pool.install(|| qld::elf::link(&options, &sink))
        }));
        assert!(result.is_ok(), "panic on corruption round {round}");
    }
}
