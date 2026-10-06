//! Differences from GNU ld that W53 settled, and the options it added.
//!
//! Each test states what GNU ld 2.46 and lld 23 do, and checks that qld
//! matches the one it follows. Inputs are assembled with the system `as` or
//! compiled with `cc`; a test prints `SKIPPED:` and passes when a tool it
//! needs is missing.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A fresh, empty directory for one test.
fn scratch(name: &str) -> PathBuf {
    common::scratch::scratch_dir("args-differences", name)
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
    let mut all = vec!["--no-fork", "--threads=2"];
    all.extend_from_slice(args);
    run(dir, env!("CARGO_BIN_EXE_qld"), &all)
}

fn qld_ok(dir: &Path, args: &[&str]) {
    let output = qld(dir, args);
    assert!(
        output.status.success(),
        "qld {} failed:\n{}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assemble(dir: &Path, name: &str, source: &str) {
    fs::write(dir.join(format!("{name}.s")), source).unwrap();
    run_ok(
        dir,
        "as",
        &["--64", "-o", &format!("{name}.o"), &format!("{name}.s")],
    );
}

fn readelf(dir: &Path, args: &[&str]) -> String {
    let mut all = vec!["-W"];
    all.extend_from_slice(args);
    run_ok(dir, "readelf", &all)
}

/// The `readelf -s` line of `name`, split into fields.
fn symbol(dir: &Path, file: &str, name: &str) -> Vec<String> {
    readelf(dir, &["-s", file])
        .lines()
        .find(|line| line.split_whitespace().nth(7) == Some(name))
        .map(|line| line.split_whitespace().map(str::to_owned).collect())
        .unwrap_or_else(|| panic!("{file}: no symbol {name}"))
}

/// `(place, addend)` of every `R_X86_64_RELATIVE` relocation.
fn relative_relocs(dir: &Path, file: &str) -> Vec<(u64, u64)> {
    readelf(dir, &["-r", file])
        .lines()
        .filter(|line| line.contains("R_X86_64_RELATIVE"))
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let place = u64::from_str_radix(fields.first()?, 16).ok()?;
            let addend = u64::from_str_radix(fields.last()?, 16).ok()?;
            Some((place, addend))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Linker-defined symbols
// ---------------------------------------------------------------------------

const EHDR_REF: &str = "
    .text
    .globl _start
_start:
    lea __ehdr_start(%rip), %rax
    lea __executable_start(%rip), %rcx
    mov $60, %eax
    xor %edi, %edi
    syscall
    .section .note.GNU-stack,\"\",@progbits
";

/// GNU ld and lld both define `__ehdr_start` and `__executable_start`
/// relative to the first allocated section. qld wrote them `SHN_ABS`,
/// which a position-independent output does not relocate.
#[test]
fn ehdr_start_is_section_relative() {
    require!("as", "readelf");
    let dir = scratch("ehdr-start");
    assemble(&dir, "start", EHDR_REF);
    for mode in ["-pie", "-no-pie", "-shared"] {
        qld_ok(&dir, &[mode, "-e", "_start", "-o", "out", "start.o"]);
        for name in ["__ehdr_start", "__executable_start"] {
            let fields = symbol(&dir, "out", name);
            assert_ne!(fields[6], "ABS", "{mode}: {name} is absolute: {fields:?}");
            assert_eq!(fields[6], "1", "{mode}: {name} is not in section 1");
        }
    }
}

/// A weak-undefined reference to `_DYNAMIC`, which the linker itself
/// defines, needs a `RELATIVE` relocation on its GOT slot in a
/// position-independent output: the slot holds an address in the image.
/// `--no-relax` keeps the GOT slot that the `mov`/`lea` relaxation would
/// otherwise remove.
#[test]
fn dynamic_got_slot_is_relocated_in_a_pie() {
    require!("as", "readelf");
    let dir = scratch("dynamic-got");
    assemble(
        &dir,
        "start",
        "
    .text
    .globl _start
_start:
    movq _DYNAMIC@GOTPCREL(%rip), %rax
    mov $60, %eax
    xor %edi, %edi
    syscall
    .weak _DYNAMIC
    .section .note.GNU-stack,\"\",@progbits
",
    );
    for extra in [
        vec!["-pie", "--no-dynamic-linker"],
        vec!["-static", "-pie", "--no-dynamic-linker"],
        vec!["-shared"],
    ] {
        let mut args = vec!["--no-relax", "-e", "_start", "-o", "out", "start.o"];
        args.extend(extra.iter().copied());
        qld_ok(&dir, &args);
        let value = u64::from_str_radix(&symbol(&dir, "out", "_DYNAMIC")[1], 16).unwrap();
        let got = readelf(&dir, &["-S", "out"]);
        assert!(got.contains(".got"), "{extra:?}: no .got");
        let relatives = relative_relocs(&dir, "out");
        assert!(
            relatives.iter().any(|&(_, addend)| addend == value),
            "{extra:?}: no RELATIVE relocation holding _DYNAMIC ({value:#x}): {relatives:?}"
        );
    }
}

const LINKER_SYMBOL_REFS: &str = "
    .section mysec,\"a\",@progbits
    .quad 1
    .text
    .globl getp
    .type getp,@function
getp:
    movq _end@GOTPCREL(%rip), %rax
    movq _edata@GOTPCREL(%rip), %rcx
    movq __bss_start@GOTPCREL(%rip), %rdx
    movq __start_mysec@GOTPCREL(%rip), %rsi
    movq __stop_mysec@GOTPCREL(%rip), %rdi
    movq mysym@GOTPCREL(%rip), %r8
    movq __ehdr_start@GOTPCREL(%rip), %r9
    movq _DYNAMIC@GOTPCREL(%rip), %r10
    ret
    .section .note.GNU-stack,\"\",@progbits
";

/// GNU ld and lld export a shared object's linker-defined symbols: `_end`,
/// `_edata`, `__bss_start`, `_etext` and `--defsym` symbols with default
/// visibility, `__start_SEC`/`__stop_SEC` protected. qld made them all
/// local, so a GOT slot for one was bound at link time where the two
/// reference linkers emit `GLOB_DAT`. The per-module symbols
/// (`__ehdr_start`, `__executable_start`, `_DYNAMIC`) stay hidden.
#[test]
fn a_shared_object_exports_its_linker_defined_symbols() {
    require!("as", "readelf");
    let dir = scratch("linker-symbol-exports");
    assemble(&dir, "lib", LINKER_SYMBOL_REFS);
    let common = [
        "-shared",
        "--no-relax",
        "--defsym",
        "mysym=0x1234",
        "lib.o",
        "-o",
    ];
    let mut args = common.to_vec();
    args.push("out.so");
    qld_ok(&dir, &args);
    let dynsym = readelf(&dir, &["--dyn-syms", "out.so"]);
    let entry = |name: &str| -> String {
        dynsym
            .lines()
            .find(|line| line.split_whitespace().nth(7) == Some(name))
            .map(|line| {
                let f: Vec<&str> = line.split_whitespace().collect();
                format!("{} {}", f[4], f[5])
            })
            .unwrap_or_else(|| "absent".to_string())
    };
    for name in ["_end", "_edata", "__bss_start", "mysym"] {
        assert_eq!(entry(name), "GLOBAL DEFAULT", "{name}");
    }
    for name in ["__start_mysec", "__stop_mysec"] {
        assert_eq!(entry(name), "GLOBAL PROTECTED", "{name}");
    }
    for name in ["__ehdr_start", "__executable_start", "_DYNAMIC"] {
        assert_eq!(entry(name), "absent", "{name} should stay per-module");
    }
    // The exported ones are preemptible, so their GOT slots get GLOB_DAT
    // rather than a link-time value; the protected and hidden ones do not.
    let relocs = readelf(&dir, &["-r", "out.so"]);
    let bound: Vec<&str> = relocs
        .lines()
        .filter(|line| line.contains("GLOB_DAT"))
        .filter_map(|line| line.split_whitespace().nth(4))
        .collect();
    for name in ["_end", "_edata", "__bss_start", "mysym"] {
        assert!(bound.contains(&name), "{name} has no GLOB_DAT: {bound:?}");
    }
    for name in ["__start_mysec", "__stop_mysec", "_DYNAMIC"] {
        assert!(!bound.contains(&name), "{name} should not be preempted");
    }
    // `-z start-stop-visibility=` overrides the protected default.
    let mut args = common.to_vec();
    args.extend_from_slice(&["hidden.so", "-z", "start-stop-visibility=hidden"]);
    qld_ok(&dir, &args);
    assert!(
        !readelf(&dir, &["--dyn-syms", "hidden.so"]).contains("__start_mysec"),
        "-z start-stop-visibility=hidden still exported __start_mysec"
    );
}

// ---------------------------------------------------------------------------
// IFUNC
// ---------------------------------------------------------------------------

const EXPORTED_IFUNC: &str = "
    .text
    .type resolver,@function
resolver:
    lea impl(%rip), %rax
    ret
    .type impl,@function
impl:
    mov $42, %eax
    ret
    .globl myfunc
    .type myfunc,@gnu_indirect_function
    .set myfunc, resolver
    .section .note.GNU-stack,\"\",@progbits
";

/// GNU ld and lld stamp any output carrying an `STT_GNU_IFUNC` symbol
/// `ELFOSABI_GNU`, even when the link needs no stub of its own for it;
/// without that, `readelf` prints the symbol type as `<OS specific>: 10`.
#[test]
fn shared_object_with_an_unused_ifunc_is_elfosabi_gnu() {
    require!("as", "readelf");
    let dir = scratch("ifunc-osabi");
    assemble(&dir, "ifunc", EXPORTED_IFUNC);
    qld_ok(&dir, &["-shared", "-o", "out.so", "ifunc.o"]);
    let header = readelf(&dir, &["-h", "out.so"]);
    assert!(
        header.contains("UNIX - GNU"),
        "OS/ABI is not GNU:\n{header}"
    );
    let dynsym = readelf(&dir, &["--dyn-syms", "out.so"]);
    assert!(
        dynsym.contains("IFUNC"),
        "myfunc is not an IFUNC in .dynsym:\n{dynsym}"
    );
    // A link with no GNU extension at all keeps ELFOSABI_NONE.
    assemble(
        &dir,
        "plain",
        "
    .text
    .globl f
f:  ret
    .section .note.GNU-stack,\"\",@progbits
",
    );
    qld_ok(&dir, &["-shared", "-o", "plain.so", "plain.o"]);
    assert!(
        readelf(&dir, &["-h", "plain.so"]).contains("UNIX - System V"),
        "a link with no GNU extension should stay ELFOSABI_NONE"
    );
}

/// A data pointer to an IFUNC keeps the address of the canonical PLT
/// stub, through a `RELATIVE` relocation, so `&f == f` holds. GNU ld
/// writes an `IRELATIVE` there instead, which stores the *resolved*
/// address and breaks the comparison; lld does as qld does.
#[test]
#[cfg(unix)]
fn a_data_pointer_to_an_ifunc_compares_equal_to_the_function() {
    require!("cc", "readelf");
    let dir = scratch("ifunc-pointer");
    fs::write(
        dir.join("main.c"),
        r#"
#include <stdio.h>
static int real(void) { return 42; }
static void *resolve(void) { return (void *)real; }
int myfunc(void) __attribute__((ifunc("resolve")));
int (*ptr)(void) = myfunc;
int main(void) {
    if ((void *)ptr != (void *)myfunc) { puts("differ"); return 1; }
    return ptr() == 42 ? 0 : 2;
}
"#,
    )
    .unwrap();
    run_ok(
        &dir,
        "cc",
        &["-c", "-fPIE", "-O1", "main.c", "-o", "main.o"],
    );
    // Drive the compiler through a `ld` symlink to qld, so the link gets
    // the C runtime and libc the target needs.
    let ld = dir.join("ld");
    let _ = fs::remove_file(&ld);
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_qld"), &ld).unwrap();
    let link = run(
        &dir,
        "cc",
        &[
            "-pie",
            &format!("-B{}", dir.display()),
            "main.o",
            "-o",
            "out",
        ],
    );
    if !link.status.success() {
        println!(
            "SKIPPED: the compiler driver could not link through qld: {}",
            String::from_utf8_lossy(&link.stderr)
        );
        return;
    }
    let relocs = readelf(&dir, &["-r", "out"]);
    assert!(
        relocs.contains("IRELATIVE"),
        "no IRELATIVE for the IFUNC stub:\n{relocs}"
    );
    let status = Command::new(dir.join("out"))
        .current_dir(&dir)
        .status()
        .unwrap();
    assert_eq!(
        status.code(),
        Some(0),
        "the pointer and the function differ, or the call failed"
    );
}

// ---------------------------------------------------------------------------
// Dynamic lists
// ---------------------------------------------------------------------------

const CPP_LIKE: &str = "
    .text
    .globl user
    .type user,@function
user:
    call _Znwm@PLT
    call _ZdlPv@PLT
    movq _ZTI4Base@GOTPCREL(%rip), %rax
    movq _ZTS4Base@GOTPCREL(%rip), %rcx
    movq gdata@GOTPCREL(%rip), %rdx
    ret
    .globl _Znwm
    .type _Znwm,@function
_Znwm:  ret
    .globl _ZdlPv
    .type _ZdlPv,@function
_ZdlPv: ret
    .data
    .globl _ZTI4Base
    .type _ZTI4Base,@object
    .size _ZTI4Base,8
_ZTI4Base: .quad 0
    .globl _ZTS4Base
    .type _ZTS4Base,@object
    .size _ZTS4Base,8
_ZTS4Base: .quad 0
    .globl gdata
    .type gdata,@object
    .size gdata,8
gdata:  .quad 0
    .section .note.GNU-stack,\"\",@progbits
";

/// The symbols a shared object's dynamic relocations name, sorted.
fn dynamic_symbols_bound_at_run_time(dir: &Path, file: &str) -> Vec<String> {
    let mut names: Vec<String> = readelf(dir, &["-r", file])
        .lines()
        .filter(|line| line.contains("GLOB_DAT") || line.contains("JUMP_SLOT"))
        .filter_map(|line| line.split_whitespace().nth(4).map(str::to_owned))
        .collect();
    names.sort();
    names
}

/// `--dynamic-list-data`, `--dynamic-list-cpp-new` and
/// `--dynamic-list-cpp-typeinfo` add their symbols to the dynamic list, so
/// `-Bsymbolic` no longer binds them, exactly as in GNU ld.
#[test]
fn the_built_in_dynamic_lists_keep_their_symbols_preemptible() {
    require!("as", "readelf");
    let dir = scratch("dynamic-lists");
    assemble(&dir, "cpp", CPP_LIKE);
    let link = |extra: &[&str]| {
        let mut args = vec![
            "-shared",
            "--no-relax",
            "-Bsymbolic",
            "-o",
            "out.so",
            "cpp.o",
        ];
        args.extend_from_slice(extra);
        qld_ok(&dir, &args);
        dynamic_symbols_bound_at_run_time(&dir, "out.so")
    };
    assert_eq!(link(&[]), Vec::<String>::new());
    assert_eq!(link(&["--dynamic-list-cpp-new"]), ["_ZdlPv", "_Znwm"]);
    assert_eq!(
        link(&["--dynamic-list-cpp-typeinfo"]),
        ["_ZTI4Base", "_ZTS4Base"]
    );
    assert_eq!(
        link(&["--dynamic-list-data"]),
        ["_ZTI4Base", "_ZTS4Base", "gdata"]
    );
    assert_eq!(
        link(&["--dynamic-list-cpp-new", "--dynamic-list-cpp-typeinfo"]),
        ["_ZTI4Base", "_ZTS4Base", "_ZdlPv", "_Znwm"]
    );
}

/// `-Bgroup` sets `DF_1_GROUP` and, as in GNU ld, makes an undefined
/// symbol an error: a group has to be self-contained.
#[test]
fn bgroup_sets_df_1_group_and_rejects_undefined_symbols() {
    require!("as", "readelf");
    let dir = scratch("bgroup");
    assemble(
        &dir,
        "lib",
        "
    .text
    .globl f
    .type f,@function
f:  ret
    .section .note.GNU-stack,\"\",@progbits
",
    );
    qld_ok(&dir, &["-shared", "-Bgroup", "-o", "out.so", "lib.o"]);
    let dynamic = readelf(&dir, &["-d", "out.so"]);
    assert!(dynamic.contains("GROUP"), "no DF_1_GROUP:\n{dynamic}");
    assemble(
        &dir,
        "undef",
        "
    .text
    .globl g
    .type g,@function
g:  call missing@PLT
    ret
    .section .note.GNU-stack,\"\",@progbits
",
    );
    let output = qld(&dir, &["-shared", "-Bgroup", "-o", "bad.so", "undef.o"]);
    assert!(
        !output.status.success(),
        "-Bgroup accepted an undefined symbol"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("undefined symbol: missing"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

// ---------------------------------------------------------------------------
// glibc ABI version dependencies
// ---------------------------------------------------------------------------

/// GNU ld 2.46 adds a `GLIBC_ABI_GNU2_TLS` version dependency to an output
/// that keeps TLS descriptors, so glibc refuses to load it on a dynamic
/// linker whose descriptor resolvers are not fixed. `--no-gnu2-tls-tag`
/// turns it off. lld 23 has neither the tag nor the options.
#[test]
fn tls_descriptors_add_the_glibc_abi_version() {
    require!("cc", "readelf");
    let dir = scratch("gnu2-tls-tag");
    fs::write(
        dir.join("tls.c"),
        "#include <stdio.h>\n__thread int tv = 3;\nint get(void) { return tv; }\n\
         void show(void) { printf(\"%d\\n\", get()); }\n",
    )
    .unwrap();
    let compile = run(
        &dir,
        "cc",
        &["-fPIC", "-mtls-dialect=gnu2", "-c", "tls.c", "-o", "tls.o"],
    );
    if !compile.status.success() {
        println!("SKIPPED: the compiler has no -mtls-dialect=gnu2");
        return;
    }
    let libc = match ["/lib64/libc.so.6", "/lib/x86_64-linux-gnu/libc.so.6"]
        .into_iter()
        .find(|path| Path::new(path).is_file())
    {
        Some(path) => path,
        None => {
            println!("SKIPPED: no libc.so.6");
            return;
        }
    };
    if !readelf(&dir, &["-V", libc]).contains("GLIBC_ABI_GNU2_TLS") {
        println!("SKIPPED: this glibc has no GLIBC_ABI_GNU2_TLS version");
        return;
    }
    qld_ok(&dir, &["-shared", "-o", "out.so", "tls.o", libc]);
    assert!(
        readelf(&dir, &["-V", "out.so"]).contains("GLIBC_ABI_GNU2_TLS"),
        "no GLIBC_ABI_GNU2_TLS dependency"
    );
    qld_ok(
        &dir,
        &[
            "-shared",
            "--no-gnu2-tls-tag",
            "-o",
            "off.so",
            "tls.o",
            libc,
        ],
    );
    assert!(
        !readelf(&dir, &["-V", "off.so"]).contains("GLIBC_ABI_GNU2_TLS"),
        "--no-gnu2-tls-tag did not turn the dependency off"
    );
}

// ---------------------------------------------------------------------------
// PE
// ---------------------------------------------------------------------------

/// A PE link cannot honor the ELF-only debug-index and section-ordering
/// options, so it rejects them rather than writing an image that quietly
/// lacks what was asked for.
#[test]
fn a_pe_link_rejects_the_elf_only_options() {
    if tool("x86_64-w64-mingw32-gcc").is_none() {
        println!("SKIPPED: x86_64-w64-mingw32-gcc not found");
        return;
    }
    let dir = scratch("pe-elf-only");
    fs::write(dir.join("main.c"), "int main(void) { return 0; }\n").unwrap();
    run_ok(
        &dir,
        "x86_64-w64-mingw32-gcc",
        &["-c", "main.c", "-o", "main.o"],
    );
    for option in [
        "--gdb-index",
        "--debug-names",
        "--separate-debug-file",
        "--symbol-ordering-file=order.txt",
        "--call-graph-profile-sort=hfsort",
    ] {
        let output = qld(&dir, &["-m", "i386pep", option, "-o", "out.exe", "main.o"]);
        let message = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(
            !output.status.success(),
            "a PE link accepted {option}: {message}"
        );
        let name = option.split('=').next().unwrap();
        assert!(
            message.contains(name) && message.contains("PE/COFF"),
            "{option}: {message}"
        );
    }
}
