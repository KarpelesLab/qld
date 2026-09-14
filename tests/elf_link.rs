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
        options.kind = if round % 5 == 4 {
            OutputKind::Relocatable
        } else {
            OutputKind::StaticExecutable
        };
        options.output = Some(dir.join("out"));
        options.gc_sections = round % 2 == 0;
        if options.kind == OutputKind::Relocatable {
            options.undefined.push("_start".into());
        }
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
        #[cfg(unix)]
        std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_qld"), &ld).unwrap();
        #[cfg(not(unix))]
        fs::copy(env!("CARGO_BIN_EXE_qld"), &ld).unwrap();
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
fn undefined_symbols_get_hints_and_demangled_names() {
    require!("cc", "c++");
    let dir = scratch("undefined-hints");
    fs::write(
        dir.join("m.cc"),
        "namespace ns { int helper_qld(int); }\nint main() { return ns::helper_qld(1); }\n",
    )
    .unwrap();
    fs::write(
        dir.join("h.cc"),
        "namespace ns { int helper_qld(long x) { return (int)x; } }\n",
    )
    .unwrap();
    run_ok(&dir, "c++", &["-c", "m.cc", "h.cc"]);
    let shim = shim(&dir);
    let output = run(&dir, "c++", &[&shim, "-o", "out", "m.o", "h.o"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("undefined symbol: ns::helper_qld(int)"),
        "{stderr}"
    );
    assert!(
        stderr.contains("did you mean: ns::helper_qld(long)?"),
        "{stderr}"
    );
    let output = run(
        &dir,
        "c++",
        &[&shim, "-o", "out", "m.o", "h.o", "-Wl,--no-demangle"],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("undefined symbol: _ZN2ns10helper_qldEi"),
        "{stderr}"
    );

    // A library in the search path that defines the symbol.
    compile_with(
        &dir,
        "lib",
        "int in_library_qld(void) { return 3; }\n",
        &["-fPIC"],
    );
    cc_link_ok(&dir, &["-shared", "-o", "libhintqld.so", "lib.o"]);
    compile_with(
        &dir,
        "main",
        "int in_library_qld(void);\nint main(void) { return in_library_qld(); }\n",
        &["-fPIE"],
    );
    let output = cc_link(&dir, &["-o", "out", "main.o", "-L."]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("-lhintqld"), "{stderr}");
}

#[test]
fn compressed_debug_sections_hold_the_same_dwarf() {
    require!("cc", "readelf");
    let dir = scratch("compress-debug");
    compile_with(
        &dir,
        "main",
        "#include <stdio.h>\nstruct point_qld { int x, y; };\n\
         static int add_qld(struct point_qld p) { return p.x + p.y; }\n\
         int main(void) { struct point_qld p = {1, 2}; printf(\"%d\\n\", add_qld(p)); return 0; }\n",
        &["-g", "-O1", "-fPIE"],
    );
    cc_link_ok(&dir, &["-o", "plain", "main.o"]);
    let plain = readelf(&dir, &["--debug-dump=info", "plain"]);
    for (format, flag) in [("zlib", "C"), ("zstd", "C"), ("zlib-gnu", "")] {
        let out = format!("out-{format}");
        cc_link_ok(
            &dir,
            &[
                "-o",
                &out,
                "main.o",
                &format!("-Wl,--compress-debug-sections={format}"),
                "-Wl,--threads=3",
            ],
        );
        assert_eq!(stdout_of(&dir, &out), "3\n");
        let headers = readelf(&dir, &["-S", &out]);
        let info = headers
            .lines()
            .find(|l| l.contains("debug_info"))
            .unwrap_or_else(|| panic!("{headers}"));
        if format == "zlib-gnu" {
            assert!(info.contains(".zdebug_info"), "{headers}");
        } else {
            assert!(
                info.split_whitespace().any(|f| f.contains(flag)),
                "{headers}"
            );
        }
        if format != "zstd" || readelf(&dir, &["--help"]).contains("zstd") {
            let dump = readelf(&dir, &["--debug-dump=info", &out]);
            let strip = |text: &str| -> Vec<String> {
                text.lines()
                    .filter(|l| l.contains("DW_AT_name") || l.contains("DW_TAG"))
                    .map(|l| l.split_whitespace().skip(1).collect::<Vec<_>>().join(" "))
                    .collect()
            };
            assert_eq!(strip(&dump), strip(&plain), "{format}");
        }
        // The output does not depend on the thread count.
        let again = format!("again-{format}");
        cc_link_ok(
            &dir,
            &[
                "-o",
                &again,
                "main.o",
                &format!("-Wl,--compress-debug-sections={format}"),
                "-Wl,--threads=1",
            ],
        );
        assert_eq!(
            fs::read(dir.join(&out)).unwrap(),
            fs::read(dir.join(&again)).unwrap(),
            "{format}"
        );
    }
}

#[test]
fn gc_sections_drops_imports_only_dead_code_uses() {
    require!("cc", "readelf");
    let dir = scratch("gc-imports");
    compile_with(
        &dir,
        "main",
        "#include <stdio.h>\n#include <unistd.h>\n\
         int dead_qld(void) { return chdir(\"/\"); }\n\
         int main(void) { puts(\"live\"); return 0; }\n",
        &["-fPIE", "-ffunction-sections"],
    );
    cc_link_ok(&dir, &["-o", "out", "main.o", "-Wl,--gc-sections"]);
    assert_eq!(stdout_of(&dir, "out"), "live\n");
    let symbols = readelf(&dir, &["--dyn-syms", "-s", "out"]);
    assert!(symbols.contains("puts@GLIBC"), "{symbols}");
    assert!(!symbols.contains("chdir"), "{symbols}");
    cc_link_ok(&dir, &["-o", "all", "main.o"]);
    let symbols = readelf(&dir, &["--dyn-syms", "all"]);
    assert!(symbols.contains("chdir"), "{symbols}");
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

    // A program that calls the dependency itself must name it.
    compile_with(
        &dir,
        "direct",
        "int base_value(void); int middle_value(void);\nint main(void) { return base_value() + middle_value(); }\n",
        &["-fPIE"],
    );
    let output = cc_link(
        &dir,
        &[
            "-o",
            "direct",
            "direct.o",
            "-L.",
            "-lmiddle",
            "-Wl,-rpath-link,deps",
        ],
    );
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("undefined symbol: base_value"), "{stderr}");
    assert!(stderr.contains("DSO missing from command line"), "{stderr}");
    assert!(stderr.contains("libbase.so, which"), "{stderr}");
}

#[test]
fn trace_symbols_common_warnings_and_cross_references() {
    require!("cc");
    let dir = scratch("xref");
    compile_with(
        &dir,
        "main",
        "int comm_qld;\nint lib_qld(void);\nint main(void) { return lib_qld() + comm_qld; }\n",
        &["-fcommon", "-fPIE"],
    );
    compile_with(
        &dir,
        "lib",
        "int comm_qld;\nint lib_qld(void) { return 0; }\n",
        &["-fcommon", "-fPIE"],
    );
    compile_with(&dir, "big", "long comm_qld;\n", &["-fcommon", "-fPIE"]);
    run_ok(&dir, "ar", &["rcs", "liblib.a", "lib.o"]);
    let stderr = cc_link_ok(
        &dir,
        &[
            "-o",
            "out",
            "main.o",
            "-L.",
            "-llib",
            "-Wl,-y,lib_qld",
            "-Wl,--trace-symbol=comm_qld",
        ],
    );
    let main_ref = stderr
        .find("main.o: reference to lib_qld")
        .unwrap_or(usize::MAX);
    let lib_def = stderr
        .find("liblib.a(lib.o): definition of lib_qld")
        .unwrap_or(usize::MAX);
    assert!(main_ref < lib_def && lib_def != usize::MAX, "{stderr}");
    assert!(
        stderr.contains("main.o: definition of comm_qld"),
        "{stderr}"
    );

    let stderr = cc_link_ok(
        &dir,
        &["-o", "out", "main.o", "lib.o", "big.o", "-Wl,--warn-common"],
    );
    assert!(
        stderr.contains("lib.o and main.o: multiple common of `comm_qld'"),
        "{stderr}"
    );
    assert!(
        stderr.contains("big.o: common of `comm_qld' overriding smaller common from main.o"),
        "{stderr}"
    );

    let output = cc_link(&dir, &["-o", "out", "main.o", "-L.", "-llib", "-Wl,--cref"]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Cross Reference Table"), "{stdout}");
    let row = stdout
        .lines()
        .position(|l| l.starts_with("lib_qld "))
        .unwrap_or_else(|| panic!("{stdout}"));
    let lines: Vec<&str> = stdout.lines().collect();
    assert!(lines[row].ends_with("liblib.a(lib.o)"), "{stdout}");
    assert!(lines[row + 1].trim() == "main.o", "{stdout}");
    cc_link_ok(
        &dir,
        &[
            "-o",
            "out",
            "main.o",
            "lib.o",
            "-Wl,--cref",
            "-Wl,-Map,out.map",
        ],
    );
    let map = fs::read_to_string(dir.join("out.map")).unwrap();
    assert!(map.contains("Cross Reference Table"), "{map}");
    assert!(map.contains(".text"), "{map}");
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

// ---------------------------------------------------------------------------
// Relocatable output.
// ---------------------------------------------------------------------------

#[test]
fn relocatable_output_combines_objects() {
    require!("cc", "readelf");
    let dir = scratch("relocatable");
    compile_with(
        &dir,
        "one",
        "
int comm_qld;
extern int weak_missing_qld(void) __attribute__((weak));
__attribute__((noinline)) static int helper(int x) { return x + 1; }
__attribute__((visibility(\"hidden\"))) int hidden_qld(void) { return 2; }
int one_qld(void) { return helper(comm_qld) + hidden_qld() + (weak_missing_qld ? 100 : 0); }
",
        &["-fcommon", "-ffunction-sections", "-fPIE"],
    );
    compile_with(
        &dir,
        "two",
        "
#include <stdio.h>
__attribute__((noinline)) static int helper(int x) { return x * 10; }
int one_qld(void);
int main(void) { printf(\"%d %d\\n\", one_qld(), helper(4)); return 0; }
",
        &["-fPIE"],
    );
    qld_ok(
        &dir,
        &[
            "-r",
            "-o",
            "combined.o",
            "--defsym",
            "abs_qld=0x1234",
            "one.o",
            "two.o",
        ],
    );
    let header = readelf(&dir, &["-h", "-S", "-s", "-r", "combined.o"]);
    assert!(header.contains("REL (Relocatable file)"), "{header}");
    assert!(header.contains("There are no program headers") || !header.contains("LOAD"));
    assert!(header.contains("COM comm_qld"), "{header}");
    assert!(
        header.contains("WEAK   DEFAULT  UND weak_missing_qld"),
        "{header}"
    );
    assert!(header.contains("HIDDEN     "), "{header}");
    assert!(header.contains("ABS abs_qld"), "{header}");
    assert_eq!(header.matches(" helper").count(), 2, "{header}");
    assert!(header.contains(".rela.text.one_qld"), "{header}");
    cc_link_ok(&dir, &["-o", "out", "combined.o"]);
    assert_eq!(stdout_of(&dir, "out"), "3 40\n");

    // -d allocates the common symbol; -x drops the unreferenced locals.
    qld_ok(
        &dir,
        &["-r", "-d", "-x", "-o", "defined.o", "one.o", "two.o"],
    );
    let symbols = readelf(&dir, &["-s", "defined.o"]);
    assert!(!symbols.contains("COM comm_qld"), "{symbols}");
    assert!(symbols.contains("OBJECT  GLOBAL DEFAULT    "), "{symbols}");
    assert!(!symbols.contains("FILE"), "{symbols}");
    cc_link_ok(&dir, &["-o", "out2", "defined.o"]);
    assert_eq!(stdout_of(&dir, "out2"), "3 40\n");

    // --gc-sections needs roots with -r, as in GNU ld.
    let output = qld(&dir, &["-r", "--gc-sections", "-o", "gc.o", "one.o"]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("requires a defined symbol root"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    qld_ok(
        &dir,
        &[
            "-r",
            "--gc-sections",
            "-u",
            "hidden_qld",
            "-o",
            "gc.o",
            "one.o",
        ],
    );
    let kept = readelf(&dir, &["-S", "gc.o"]);
    assert!(kept.contains(".text.hidden_qld"), "{kept}");
    assert!(!kept.contains(".text.one_qld"), "{kept}");
}

#[test]
fn relocatable_output_uses_extended_section_numbering() {
    require!("as", "readelf");
    let dir = scratch("relocatable-many-sections");
    let mut source = String::from(
        ".globl _start\n.text\n_start:\n call f65999\n mov %eax, %edi\n mov $60, %eax\n syscall\n",
    );
    for i in 0..66000 {
        source.push_str(&format!(
            ".section .text.f{i},\"ax\",@progbits\n.globl f{i}\nf{i}:\n mov ${}, %eax\n ret\n",
            i % 100
        ));
    }
    assemble(&dir, "many", &source);
    qld_ok(&dir, &["-r", "-o", "combined.o", "many.o"]);
    let header = readelf(&dir, &["-h", "combined.o"]);
    assert!(
        header.contains("Number of section headers:         0 ("),
        "{header}"
    );
    let symbols = readelf(&dir, &["-s", "combined.o"]);
    assert!(symbols.contains("GLOBAL DEFAULT 66004 f65999"), "{symbols}");
    qld_ok(&dir, &["-o", "out", "combined.o"]);
    assert_eq!(exit_code(&dir, "out"), 99);
}

#[test]
fn gnu_property_notes_merge_used_and_needed_bits() {
    require!("as", "readelf");
    let dir = scratch("property-notes");
    fs::write(dir.join("start.s"), EXIT_42).unwrap();
    fs::write(dir.join("other.s"), ".globl other\n.text\nother:\n ret\n").unwrap();
    for (object, source, used) in [
        ("start.o", "start.s", "yes"),
        ("used.o", "other.s", "yes"),
        ("unused.o", "other.s", "no"),
    ] {
        run_ok(
            &dir,
            "as",
            &[
                "--64",
                &format!("-mx86-used-note={used}"),
                "-o",
                object,
                source,
            ],
        );
    }
    qld_ok(
        &dir,
        &["-o", "both", "start.o", "used.o", "-z", "x86-64-v2"],
    );
    let notes = readelf(&dir, &["-n", "both"]);
    assert!(notes.contains("x86 ISA used"), "{notes}");
    assert!(notes.contains("x86 feature used"), "{notes}");
    assert!(notes.contains("x86 ISA needed: x86-64-v2"), "{notes}");
    assert_eq!(exit_code(&dir, "both"), 42);

    // "Used" bits survive only when every input has them.
    qld_ok(&dir, &["-o", "mixed", "start.o", "unused.o"]);
    let notes = readelf(&dir, &["-n", "mixed"]);
    assert!(!notes.contains("x86 ISA used"), "{notes}");
    qld_ok(&dir, &["-r", "-o", "combined.o", "start.o", "used.o"]);
    let notes = readelf(&dir, &["-n", "combined.o"]);
    assert!(notes.contains("x86 ISA used"), "{notes}");
}

// ---------------------------------------------------------------------------
// Real-project regressions (workstream W16).
// ---------------------------------------------------------------------------

/// An archive member beats a shared library that comes after its archive,
/// as in GNU ld (gcc's `-lgcc --as-needed -lgcc_s` relies on it for
/// `__popcountdi2`); weak references still bind to the shared library.
#[test]
fn archive_before_shared_library_is_extracted() {
    require!("cc", "ar", "readelf");
    let dir = scratch("archive-before-shared");
    compile_with(
        &dir,
        "shared",
        "int dup_qld(void) { return 1; }\nint weakonly_qld(void) { return 5; }\n",
        &["-fPIC"],
    );
    compile_with(
        &dir,
        "helper",
        "int dup_qld(void) { return 1; }\n",
        &["-fPIC"],
    );
    compile_with(
        &dir,
        "member_a",
        "int dup_qld(void) { return 2; }\n",
        &["-fPIC"],
    );
    compile_with(
        &dir,
        "member_b",
        "int weakonly_qld(void) { return 6; }\n",
        &["-fPIC"],
    );
    compile_with(
        &dir,
        "main",
        "#include <stdio.h>\nint dup_qld(void);\nint weakonly_qld(void) __attribute__((weak));\n\
         int main(void) { printf(\"%d %d\\n\", dup_qld(), weakonly_qld ? weakonly_qld() : 0); return 0; }\n",
        &["-fPIE"],
    );
    compile_with(
        &dir,
        "main2",
        "#include <stdio.h>\nint dup_qld(void);\nint main(void) { printf(\"%d\\n\", dup_qld()); return 0; }\n",
        &["-fPIE"],
    );
    run_ok(&dir, "ar", &["rcs", "libdup.a", "member_a.o", "member_b.o"]);
    cc_link_ok(&dir, &["-shared", "-o", "libdup.so", "shared.o"]);
    cc_link_ok(&dir, &["-shared", "-o", "libhelper.so", "helper.o"]);

    // Archive first: the member defining dup_qld is extracted; the weak
    // reference does not extract the other one and binds to the library.
    cc_link_ok(&dir, &["-o", "first", "main.o", "libdup.a", "-L.", "-ldup"]);
    assert_eq!(stdout_of(&dir, "first"), "2 5\n");
    let dynsym = readelf(&dir, &["--dyn-syms", "first"]);
    assert!(!dynsym.contains("UND dup_qld"), "{dynsym}");
    assert!(dynsym.contains("UND weakonly_qld"), "{dynsym}");

    // Library first: it wins.
    cc_link_ok(
        &dir,
        &["-o", "second", "main.o", "-L.", "-ldup", "libdup.a"],
    );
    assert_eq!(stdout_of(&dir, "second"), "1 5\n");

    // An --as-needed library that only duplicates the archive is not needed.
    cc_link_ok(
        &dir,
        &[
            "-o",
            "third",
            "main2.o",
            "libdup.a",
            "-L.",
            "-Wl,--as-needed",
            "-lhelper",
        ],
    );
    assert_eq!(stdout_of(&dir, "third"), "2\n");
    let dynamic = readelf(&dir, &["-d", "third"]);
    assert!(!dynamic.contains("libhelper.so"), "{dynamic}");
}

/// `PT_TLS` starts on its alignment even when `.tdata` is less aligned than
/// `.tbss`: glibc places the block by `p_vaddr % p_align`, so local-exec
/// offsets were 4 bytes off (LLVM's unit tests crashed in
/// `timeTraceProfilerBegin`).
#[test]
fn tls_segment_starts_aligned() {
    require!("cc", "readelf");
    let dir = scratch("tls-segment-aligned");
    compile_with(
        &dir,
        "main",
        "#include <stdio.h>\n__thread int small_qld = 7;\n__thread void *ptr_qld;\n\
         __attribute__((noinline)) static int *addr(void) { return &small_qld; }\n\
         __attribute__((noinline)) static void **paddr(void) { return &ptr_qld; }\n\
         int main(void) { *paddr() = addr(); printf(\"%d %d\\n\", *addr(), *(int *)*paddr()); return 0; }\n",
        &["-fPIE"],
    );
    cc_link_ok(&dir, &["-pie", "-o", "out", "main.o"]);
    assert_eq!(stdout_of(&dir, "out"), "7 7\n");
    let segments = readelf(&dir, &["-l", "out"]);
    let tls = segments
        .lines()
        .find(|l| l.trim_start().starts_with("TLS"))
        .unwrap_or_else(|| panic!("no PT_TLS: {segments}"));
    let fields: Vec<&str> = tls.split_whitespace().collect();
    let vaddr = u64::from_str_radix(fields[2].trim_start_matches("0x"), 16).unwrap();
    let align = u64::from_str_radix(fields[7].trim_start_matches("0x"), 16).unwrap();
    assert_eq!(vaddr % align, 0, "{tls}");
}

/// Importing a weak data symbol also imports the strong symbol its library
/// defines at the same address, as GNU ld does (glibc's `environ` brings
/// `__environ`, `timezone` brings `__timezone`).
#[test]
fn weak_data_imports_bring_their_strong_alias() {
    require!("cc", "readelf");
    let dir = scratch("weak-data-alias");
    compile_with(
        &dir,
        "lib",
        "int strong_qld = 3;\nextern int weak_qld __attribute__((weak, alias(\"strong_qld\")));\n\
         int func_qld(void) { return 1; }\n\
         extern int weakfunc_qld(void) __attribute__((weak, alias(\"func_qld\")));\n",
        &["-fPIC"],
    );
    compile_with(
        &dir,
        "main",
        "#include <stdio.h>\nextern int weak_qld;\nint weakfunc_qld(void);\n\
         int main(void) { printf(\"%d %d\\n\", weak_qld, weakfunc_qld()); return 0; }\n",
        &["-fPIC"],
    );
    cc_link_ok(&dir, &["-shared", "-o", "libalias.so", "lib.o"]);
    for (output, kind) in [("out", "-pie"), ("libuser.so", "-shared")] {
        cc_link_ok(&dir, &[kind, "-o", output, "main.o", "-L.", "-lalias"]);
        let dynsym = readelf(&dir, &["--dyn-syms", output]);
        assert!(
            dynsym.contains("GLOBAL DEFAULT  UND strong_qld"),
            "{dynsym}"
        );
        // Functions have no such aliases.
        assert!(
            !dynsym.lines().any(|l| l.ends_with(" func_qld")),
            "{dynsym}"
        );
    }
    assert_eq!(stdout_of(&dir, "out"), "3 1\n");
}

/// An exported symbol in a non-allocated section keeps that section's index
/// in `.dynsym` (rustc's `rust_metadata_*` symbols in `.rustc`), as with GNU
/// ld, instead of becoming absolute.
#[test]
fn dynamic_symbols_in_non_allocated_sections_keep_their_section() {
    require!("as", "readelf");
    let dir = scratch("dynsym-nonalloc");
    assemble(
        &dir,
        "meta",
        "
    .section .meta_qld,\"\",@progbits
    .globl metadata_qld
    .type metadata_qld, @object
    .size metadata_qld, 4
metadata_qld:
    .long 1
    .text
    .globl code_qld
code_qld:
    ret
",
    );
    qld_ok(&dir, &["-shared", "-o", "libmeta.so", "meta.o"]);
    let sections = readelf(&dir, &["-S", "libmeta.so"]);
    let index = sections
        .lines()
        .find(|l| l.contains(" .meta_qld "))
        .and_then(|l| l.split('[').nth(1))
        .and_then(|l| l.split(']').next())
        .map(|n| n.trim().to_string())
        .unwrap_or_else(|| panic!("no .meta_qld: {sections}"));
    let dynsym = readelf(&dir, &["--dyn-syms", "libmeta.so"]);
    let line = dynsym
        .lines()
        .find(|l| l.ends_with(" metadata_qld"))
        .unwrap_or_else(|| panic!("metadata_qld not exported: {dynsym}"));
    let ndx = line.split_whitespace().nth(6).unwrap_or_default();
    assert_eq!(ndx, index, "{line}\n{sections}");
}

/// An executable exports the definitions that a dependency of its libraries
/// (not itself on the command line) defines or references, as GNU ld does:
/// LLVM's BUILD_SHARED_LIBS tools left template instances unexported, and a
/// callback looked up by an indirect dependency was not found at run time.
#[test]
fn transitive_dependencies_see_executable_definitions() {
    require!("cc", "readelf");
    let dir = scratch("transitive-exports");
    compile_with(
        &dir,
        "base",
        "int callback_qld(void);\nint tmpl_qld(void) __attribute__((weak));\n\
         int tmpl_qld(void) { return 1; }\n\
         int base_call(void) { return callback_qld() + tmpl_qld(); }\n",
        &["-fPIC"],
    );
    compile_with(
        &dir,
        "mid",
        "int base_call(void);\nint mid_call(void) { return base_call(); }\n",
        &["-fPIC"],
    );
    compile_with(
        &dir,
        "main",
        "#include <stdio.h>\nint mid_call(void);\nint callback_qld(void) { return 40; }\n\
         int tmpl_qld(void) __attribute__((weak));\nint tmpl_qld(void) { return 2; }\n\
         int main(void) { printf(\"%d\\n\", mid_call() + tmpl_qld() - 2); return 0; }\n",
        &["-fPIE"],
    );
    cc_link_ok(&dir, &["-shared", "-o", "libbase.so", "base.o"]);
    cc_link_ok(
        &dir,
        &["-shared", "-o", "libmid.so", "mid.o", "-L.", "-lbase"],
    );
    cc_link_ok(
        &dir,
        &[
            "-pie",
            "-o",
            "out",
            "main.o",
            "-L.",
            "-lmid",
            "-Wl,-rpath-link,.",
        ],
    );
    let dynsym = readelf(&dir, &["--dyn-syms", "out"]);
    let exported = |name: &str| {
        dynsym
            .lines()
            .any(|l| l.ends_with(&format!(" {name}")) && !l.contains(" UND "))
    };
    assert!(exported("callback_qld"), "{dynsym}");
    assert!(exported("tmpl_qld"), "{dynsym}");
    // libbase's own references bind to the executable's definitions.
    assert_eq!(stdout_of(&dir, "out"), "42\n");
}
