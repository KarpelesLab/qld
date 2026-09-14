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

// ---------------------------------------------------------------------------
// Dynamic linking (workstream W11).
// ---------------------------------------------------------------------------

/// A `-B` directory whose `ld` is the qld under test, for `cc`.
fn shim(dir: &Path) -> String {
    let shim = dir.join("shim");
    fs::create_dir_all(&shim).unwrap();
    let ld = shim.join("ld");
    if !ld.exists() {
        std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_qld"), &ld).unwrap();
    }
    format!("-B{}/", shim.display())
}

/// Runs `cc` with the qld shim first; returns stderr.
fn cc_link(dir: &Path, args: &[&str]) -> Output {
    let shim = shim(dir);
    let mut all = vec![shim.as_str()];
    all.extend_from_slice(args);
    run(dir, "cc", &all)
}

fn cc_link_ok(dir: &Path, args: &[&str]) -> String {
    let output = cc_link(dir, args);
    assert!(
        output.status.success(),
        "cc {} failed:\n{}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Compiles `source` into `name.o` with exactly `flags`.
fn compile_with(dir: &Path, name: &str, source: &str, flags: &[&str]) {
    fs::write(dir.join(format!("{name}.c")), source).unwrap();
    let src = format!("{name}.c");
    let obj = format!("{name}.o");
    let mut args = vec!["-c", "-O2"];
    args.extend_from_slice(flags);
    args.extend_from_slice(&[src.as_str(), "-o", obj.as_str()]);
    run_ok(dir, "cc", &args);
}

fn stdout_of(dir: &Path, binary: &str) -> String {
    let output = Command::new(dir.join(binary))
        .current_dir(dir)
        .env("LD_LIBRARY_PATH", dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{binary} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn lazy_binding_without_ibt() {
    require!("cc", "readelf");
    let dir = scratch("lazy-binding");
    compile_with(
        &dir,
        "main",
        "#include <stdio.h>\nint main(void) { puts(\"lazy\"); return 0; }\n",
        &["-fPIE", "-fcf-protection=none"],
    );
    cc_link_ok(&dir, &["-pie", "-Wl,-z,lazy", "-o", "out", "main.o"]);
    assert_eq!(stdout_of(&dir, "out"), "lazy\n");
    let info = readelf(&dir, &["-S", "-d", "out"]);
    assert!(info.contains(".got.plt"), "{info}");
    assert!(!info.contains(".plt.sec"), "{info}");
    assert!(!info.contains("BIND_NOW"), "{info}");
    assert!(info.contains("(JMPREL)"), "{info}");
    // Resolved at load time instead, the program behaves the same.
    let output = Command::new(dir.join("out"))
        .env("LD_BIND_NOW", "1")
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&output.stdout), "lazy\n");
}

#[test]
fn copy_relocation_defines_library_aliases() {
    require!("cc", "readelf");
    let dir = scratch("copy-aliases");
    compile_with(
        &dir,
        "main",
        "
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
extern char **environ;
int main(void) {
    setenv(\"QLD_ALIAS_TEST\", \"1\", 1);
    for (char **e = environ; *e; e++)
        if (strncmp(*e, \"QLD_ALIAS_TEST=\", 15) == 0) {
            puts(\"found\");
            return 0;
        }
    puts(\"missing\");
    return 0;
}
",
        &["-fno-pie"],
    );
    cc_link_ok(&dir, &["-no-pie", "-o", "out", "main.o"]);
    assert_eq!(stdout_of(&dir, "out"), "found\n");
    let symbols = readelf(&dir, &["--dyn-syms", "-r", "out"]);
    assert!(symbols.contains("R_X86_64_COPY"), "{symbols}");
    assert!(symbols.contains(" __environ@"), "{symbols}");
    assert_eq!(symbols.matches("R_X86_64_COPY").count(), 1, "{symbols}");
}

#[test]
fn pack_relative_relocations() {
    require!("cc", "readelf");
    let dir = scratch("relr");
    compile_with(
        &dir,
        "main",
        "
#include <stdio.h>
static int a = 1, b = 2, c = 3;
static int *const table[] = {&a, &b, &c, &a, &b, &c};
int main(void) {
    int sum = 0;
    for (unsigned i = 0; i < sizeof table / sizeof table[0]; i++)
        sum += *table[i];
    printf(\"%d\\n\", sum);
    return 0;
}
",
        &["-fPIE"],
    );
    cc_link_ok(
        &dir,
        &["-pie", "-Wl,-z,pack-relative-relocs", "-o", "out", "main.o"],
    );
    assert_eq!(stdout_of(&dir, "out"), "12\n");
    let info = readelf(&dir, &["-S", "-d", "-V", "-r", "out"]);
    assert!(info.contains(".relr.dyn"), "{info}");
    assert!(info.contains("(RELR)"), "{info}");
    assert!(info.contains("GLIBC_ABI_DT_RELR"), "{info}");
    assert!(!info.contains("R_X86_64_RELATIVE"), "{info}");
}

#[test]
fn shared_library_undefined_symbols() {
    require!("cc");
    let dir = scratch("shlib-undefined");
    compile_with(
        &dir,
        "lib",
        "int missing_qld(void); int call_missing(void) { return missing_qld(); }\n",
        &["-fPIC"],
    );
    compile_with(
        &dir,
        "main",
        "int call_missing(void); int main(void) { return call_missing(); }\n",
        &["-fPIE"],
    );
    // A shared object may leave symbols undefined...
    cc_link_ok(&dir, &["-shared", "-o", "libmissing.so", "lib.o"]);
    // ...unless -z defs or --no-undefined.
    for flag in ["-Wl,-z,defs", "-Wl,--no-undefined"] {
        let output = cc_link(&dir, &["-shared", flag, "-o", "libstrict.so", "lib.o"]);
        assert!(!output.status.success(), "{flag}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("undefined symbol: missing_qld"), "{stderr}");
    }
    // An executable must not use a library with undefined symbols...
    let output = cc_link(&dir, &["-o", "out", "main.o", "-L.", "-lmissing"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("undefined reference: missing_qld"),
        "{stderr}"
    );
    assert!(stderr.contains("libmissing.so"), "{stderr}");
    // ...unless allowed.
    cc_link_ok(
        &dir,
        &[
            "-Wl,--allow-shlib-undefined",
            "-o",
            "out",
            "main.o",
            "-L.",
            "-lmissing",
        ],
    );
}

#[test]
fn transitive_dependencies_are_found_through_rpath_link() {
    require!("cc");
    let dir = scratch("rpath-link");
    fs::create_dir_all(dir.join("deps")).unwrap();
    compile_with(
        &dir,
        "base",
        "int base_value(void) { return 7; }\n",
        &["-fPIC"],
    );
    compile_with(
        &dir,
        "middle",
        "int base_value(void); int middle_value(void) { return base_value() * 6; }\n",
        &["-fPIC"],
    );
    compile_with(
        &dir,
        "main",
        "#include <stdio.h>\nint middle_value(void);\nint main(void) { printf(\"%d\\n\", middle_value()); return 0; }\n",
        &["-fPIE"],
    );
    cc_link_ok(
        &dir,
        &[
            "-shared",
            "-Wl,-soname,libbase.so",
            "-o",
            "deps/libbase.so",
            "base.o",
        ],
    );
    cc_link_ok(
        &dir,
        &[
            "-shared",
            "-o",
            "libmiddle.so",
            "middle.o",
            "-Ldeps",
            "-lbase",
        ],
    );
    // Without -rpath-link, the dependency cannot be found to check
    // libmiddle.so's undefined symbols: a warning, not an error.
    let stderr = cc_link_ok(&dir, &["-o", "out", "main.o", "-L.", "-lmiddle"]);
    assert!(stderr.contains("libbase.so, needed by"), "{stderr}");
    let stderr = cc_link_ok(
        &dir,
        &[
            "-o",
            "out",
            "main.o",
            "-L.",
            "-lmiddle",
            "-Wl,-rpath-link,deps",
        ],
    );
    assert!(!stderr.contains("needed by"), "{stderr}");
    let output = Command::new(dir.join("out"))
        .env(
            "LD_LIBRARY_PATH",
            format!("{}:{}", dir.display(), dir.join("deps").display()),
        )
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&output.stdout), "42\n");
}

#[test]
fn symbolic_binding_and_hash_styles() {
    require!("cc", "readelf");
    let dir = scratch("symbolic");
    compile_with(
        &dir,
        "lib",
        "int inner_qld(void) { return 5; }\nint outer_qld(void) { return inner_qld() + 1; }\n",
        &[
            "-fPIC",
            "-fno-inline",
            "-fno-ipa-icf",
            "-fsemantic-interposition",
        ],
    );
    cc_link_ok(&dir, &["-shared", "-o", "libplain.so", "lib.o"]);
    let plain = readelf(&dir, &["-r", "-d", "libplain.so"]);
    assert!(
        plain.contains("inner_qld"),
        "calls go through the PLT: {plain}"
    );
    cc_link_ok(
        &dir,
        &[
            "-shared",
            "-Wl,-Bsymbolic",
            "-Wl,--hash-style=both",
            "-o",
            "libsym.so",
            "lib.o",
        ],
    );
    let symbolic = readelf(&dir, &["-r", "-d", "-S", "--dyn-syms", "libsym.so"]);
    assert!(!symbolic.contains("R_X86_64_JUMP_SLOT"), "{symbolic}");
    assert!(symbolic.contains("SYMBOLIC"), "{symbolic}");
    assert!(symbolic.contains(".gnu.hash"), "{symbolic}");
    assert!(symbolic.contains(" .hash"), "{symbolic}");
    assert!(symbolic.contains("outer_qld"), "{symbolic}");
}

#[test]
fn text_relocations_are_reported() {
    require!("as", "readelf");
    let dir = scratch("textrel");
    assemble(
        &dir,
        "lib",
        "
    .globl data_qld, _start
    .data
data_qld:
    .quad 1
    .text
_start:
    .quad data_qld
",
    );
    let stderr = qld_ok(&dir, &["-shared", "-o", "lib.so", "lib.o"]);
    assert!(stderr.contains("DT_TEXTREL"), "{stderr}");
    let info = readelf(&dir, &["-d", "lib.so"]);
    assert!(info.contains("TEXTREL"), "{info}");
    let output = qld(&dir, &["-shared", "-z", "text", "-o", "lib2.so", "lib.o"]);
    assert!(!output.status.success());
}

#[test]
fn absolute_addresses_in_pie_need_pic() {
    require!("as");
    let dir = scratch("needs-pic");
    assemble(
        &dir,
        "main",
        "
    .globl _start
    .text
_start:
    movl $_start, %eax
    ret
",
    );
    let output = qld(&dir, &["-pie", "-o", "out", "main.o"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("recompile with -fPIC"), "{stderr}");
}

#[test]
fn exported_symbols_follow_lists_and_excluded_libraries() {
    require!("cc", "ar", "readelf");
    let dir = scratch("exports");
    compile_with(
        &dir,
        "helper",
        "int archived_qld(void) { return 3; }\n",
        &["-fPIC"],
    );
    run_ok(&dir, "ar", &["rcs", "libhelper.a", "helper.o"]);
    compile_with(
        &dir,
        "lib",
        "int archived_qld(void);\nint api_qld(void) { return archived_qld(); }\n",
        &["-fPIC"],
    );
    cc_link_ok(
        &dir,
        &["-shared", "-o", "libwith.so", "lib.o", "libhelper.a"],
    );
    let with = readelf(&dir, &["--dyn-syms", "libwith.so"]);
    assert!(with.contains("archived_qld"), "{with}");
    cc_link_ok(
        &dir,
        &[
            "-shared",
            "-Wl,--exclude-libs,libhelper.a",
            "-o",
            "libwithout.so",
            "lib.o",
            "libhelper.a",
        ],
    );
    let without = readelf(&dir, &["--dyn-syms", "libwithout.so"]);
    assert!(!without.contains("archived_qld"), "{without}");
    assert!(without.contains("api_qld"), "{without}");

    compile_with(
        &dir,
        "main",
        "int listed_qld(void) { return 1; }\nint unlisted_qld(void) { return 2; }\nint main(void) { return listed_qld() + unlisted_qld() - 3; }\n",
        &["-fPIE"],
    );
    fs::write(dir.join("list"), "{ global: listed_qld; };\n").unwrap();
    cc_link_ok(&dir, &["-Wl,--dynamic-list=list", "-o", "out", "main.o"]);
    let exe = readelf(&dir, &["--dyn-syms", "out"]);
    assert!(exe.contains("listed_qld"), "{exe}");
    assert!(!exe.contains("unlisted_qld"), "{exe}");
}

#[test]
fn canonical_plt_entries_keep_function_pointers_equal() {
    require!("cc", "readelf");
    let dir = scratch("canonical-plt");
    compile_with(
        &dir,
        "lib",
        "int target_qld(void) { return 9; }\nint (*library_pointer(void))(void) { return target_qld; }\n",
        &["-fPIC"],
    );
    compile_with(
        &dir,
        "main",
        "
#include <stdio.h>
int target_qld(void);
int (*library_pointer(void))(void);
int (*const exe_pointer)(void) = target_qld;
int main(void) {
    printf(\"%s %d\\n\", exe_pointer == library_pointer() ? \"equal\" : \"different\", exe_pointer());
    return 0;
}
",
        &["-fno-pie"],
    );
    cc_link_ok(&dir, &["-shared", "-o", "libtarget.so", "lib.o"]);
    cc_link_ok(
        &dir,
        &[
            "-no-pie",
            "-o",
            "out",
            "main.o",
            "-L.",
            "-ltarget",
            "-Wl,-rpath,$ORIGIN",
        ],
    );
    assert_eq!(stdout_of(&dir, "out"), "equal 9\n");
    let symbols = readelf(&dir, &["--dyn-syms", "out"]);
    let line = symbols
        .lines()
        .find(|l| l.contains("target_qld"))
        .unwrap_or_default();
    assert!(line.contains("UND"), "{symbols}");
    assert!(
        !line.contains(" 0000000000000000 "),
        "canonical address: {symbols}"
    );
}

#[test]
fn tls_models_in_shared_objects() {
    require!("cc", "readelf");
    let dir = scratch("tls-models");
    compile_with(
        &dir,
        "lib",
        "
static __thread int local_counter = 40;
__thread int exported_counter = 1;
int bump_local(void) { return ++local_counter; }
int bump_exported(void) { return ++exported_counter; }
",
        &["-fPIC", "-ftls-model=local-dynamic"],
    );
    compile_with(
        &dir,
        "ie",
        "__thread int ie_counter = 5; int read_ie(void) { return ie_counter; }\n",
        &["-fPIC", "-ftls-model=initial-exec"],
    );
    compile_with(
        &dir,
        "main",
        "
#include <stdio.h>
extern __thread int exported_counter;
int bump_local(void);
int bump_exported(void);
int read_ie(void);
int main(void) {
    bump_exported();
    int local = bump_local();
    int seen = exported_counter;
    int ie = read_ie();
    int again = bump_exported();
    printf(\"%d %d %d %d\\n\", local, seen, ie, again);
    return 0;
}
",
        &["-fPIE"],
    );
    cc_link_ok(&dir, &["-shared", "-o", "libtls.so", "lib.o", "ie.o"]);
    let info = readelf(&dir, &["-r", "-d", "libtls.so"]);
    assert!(info.contains("R_X86_64_DTPMOD64"), "{info}");
    assert!(info.contains("R_X86_64_TPOFF64"), "{info}");
    assert!(info.contains("STATIC_TLS"), "{info}");
    cc_link_ok(
        &dir,
        &["-o", "out", "main.o", "-L.", "-ltls", "-Wl,-rpath,$ORIGIN"],
    );
    assert_eq!(stdout_of(&dir, "out"), "41 2 5 3\n");
}

#[test]
fn ifuncs_in_position_independent_outputs() {
    require!("cc");
    let dir = scratch("ifunc-pic");
    let source = "
#include <stdio.h>
static int impl_one(void) { return 1; }
static int (*resolve_pick(void))(void) { return impl_one; }
int pick(void) __attribute__((ifunc(\"resolve_pick\")));
int (*const pick_pointer)(void) = pick;
int use_pick(void) { return pick() + pick_pointer(); }
";
    compile_with(&dir, "lib", source, &["-fPIC"]);
    compile_with(
        &dir,
        "main",
        "#include <stdio.h>\nint use_pick(void);\nint main(void) { printf(\"%d\\n\", use_pick()); return 0; }\n",
        &["-fPIE"],
    );
    cc_link_ok(&dir, &["-shared", "-o", "libifunc.so", "lib.o"]);
    cc_link_ok(
        &dir,
        &[
            "-o",
            "shared",
            "main.o",
            "-L.",
            "-lifunc",
            "-Wl,-rpath,$ORIGIN",
        ],
    );
    assert_eq!(stdout_of(&dir, "shared"), "2\n");
    cc_link_ok(&dir, &["-pie", "-o", "pie", "main.o", "lib.o"]);
    assert_eq!(stdout_of(&dir, "pie"), "2\n");
}

/// Corrupting a shared object input must give errors (or a link), never a
/// panic.
#[test]
fn corrupted_shared_objects_never_panic() {
    require!("cc");
    let dir = scratch("corrupt-shared");
    compile_with(
        &dir,
        "lib",
        "__thread int t_qld; int f_qld(void) { return t_qld; }\nint v_qld = 3;\n",
        &["-fPIC"],
    );
    cc_link_ok(
        &dir,
        &[
            "-shared",
            "-Wl,-soname,libc_qld.so",
            "-o",
            "lib.so",
            "lib.o",
        ],
    );
    compile_with(
        &dir,
        "main",
        "int f_qld(void); extern int v_qld; int _start(void) { return f_qld() + v_qld; }\n",
        &["-fPIE", "-fno-stack-protector"],
    );
    let original = fs::read(dir.join("lib.so")).unwrap();
    let object = fs::read(dir.join("main.o")).unwrap();
    let mut state = 0x2545_f491_4f6c_dd1du64;
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
    for round in 0..400 {
        let mut data = original.clone();
        match round % 3 {
            0 => data.truncate((next() as usize) % original.len()),
            1 => {
                for _ in 0..8 {
                    let at = (next() as usize) % data.len();
                    data[at] ^= (next() as u8) | 1;
                }
            }
            _ => {
                let at = (next() as usize) % data.len().clamp(1, 4096);
                data[at] = next() as u8;
            }
        }
        let mut options = LinkOptions::new();
        options.kind = OutputKind::Pie;
        options.output = Some(dir.join("out"));
        options.no_dynamic_linker = round % 2 == 0;
        options.push_input(
            InputKind::Bytes {
                name: "main.o".into(),
                data: Arc::from(object.clone()),
            },
            InputAttrs::default(),
        );
        options.push_input(
            InputKind::Bytes {
                name: format!("corrupt{round}.so"),
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
