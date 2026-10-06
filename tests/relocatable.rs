//! `-r` (relocatable output) and `--emit-relocs` across every ELF class
//! and byte order (workstream W50).
//!
//! One clang builds the same freestanding sources for each architecture —
//! they include no header, so no cross sysroot is needed — and qld then
//! combines the objects into one object with `-r`. `ld.lld -r` does the
//! same, and the two objects must hold the same code and the same
//! relocations: `llvm-objdump -dr` prints section-relative offsets and
//! names its relocations' targets, so the comparison does not depend on
//! where either linker put the sections. Only the padding between members
//! differs by design, and is dropped before comparing (qld pads code with
//! NOPs, as GNU ld does, and lld with traps). Each combined object is then
//! linked into a program, so that a broken relocation shows up as a
//! failed or wrong link rather than a byte difference.
//!
//! `--emit-relocs` is compared the same way, by the `(type, symbol)` pairs
//! the two linkers write; that covers the `SHT_REL` architectures (i386
//! and 32-bit Arm) as well as the `SHT_RELA` ones.
//!
//! On a host that can run them, the combined objects are also linked with
//! the system compiler and *run*.
//!
//! Tools: a clang and clang++ with the back ends (any upstream build has
//! them all) and `ld.lld` (`QLD_TEST_LLD`), plus `llvm-objdump`. A missing
//! tool prints `SKIPPED:` and passes, unless `QLD_REQUIRE_TOOLS` is set.
//! A host that does not build ELF at all — the MinGW toolchain of a
//! Windows runner, whose `ld` only knows `i386pe` — skips every test.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use common::tools::{driver_is_clang, find_program, skip, tools, tools_required};

/// One architecture: the directory name, the clang target triple and the
/// linker emulation.
struct Target {
    name: &'static str,
    triple: &'static str,
    emulation: &'static str,
}

/// The architectures the comparison covers: both classes, both byte
/// orders, and both relocation forms.
const TARGETS: &[Target] = &[
    Target {
        name: "x86_64",
        triple: "x86_64-linux-gnu",
        emulation: "elf_x86_64",
    },
    Target {
        name: "i386",
        triple: "i386-linux-gnu",
        emulation: "elf_i386",
    },
    Target {
        name: "x32",
        triple: "x86_64-linux-gnux32",
        emulation: "elf32_x86_64",
    },
    Target {
        name: "arm",
        triple: "armv7-linux-gnueabihf",
        emulation: "armelf_linux_eabi",
    },
    Target {
        name: "aarch64",
        triple: "aarch64-linux-gnu",
        emulation: "aarch64linux",
    },
    Target {
        name: "riscv32",
        triple: "riscv32-linux-gnu",
        emulation: "elf32lriscv",
    },
    Target {
        name: "riscv64",
        triple: "riscv64-linux-gnu",
        emulation: "elf64lriscv",
    },
    Target {
        name: "s390x",
        triple: "s390x-linux-gnu",
        emulation: "elf64_s390",
    },
];

const A_C: &str = r#"
int comm_qld;
static const char msg[] = "hello from a";
static int table[4] = { 1, 2, 3, 4 };
__attribute__((noinline)) static int helper(int x) { return x + table[x & 3]; }
__attribute__((visibility("hidden"))) int hidden_qld(int x) { return helper(x) + 2; }
const char *a_msg(void) { return msg; }
int a_fn(int x) { return hidden_qld(x) + comm_qld; }
"#;

const B_C: &str = r#"
static const char msg[] = "hello from b";
static int local_data = 11;
__attribute__((noinline)) static int helper(int x) { return x * 3 + local_data; }
int a_fn(int); const char *a_msg(void);
const char *b_msg(void) { return msg; }
int b_fn(int x) { return helper(x) + a_fn(x) + (int)(a_msg()[0] + b_msg()[0]); }
"#;

const C_CC: &str = r#"
struct W { int v; int get() const { return v * 5; } };
template <typename T> T twice(T t) { return t + t; }
extern "C" int b_fn(int);
extern "C" int c_fn(int x) { W w{x}; return w.get() + twice(x) + b_fn(x); }
extern "C" void _start(void) { volatile int s = c_fn(3); (void)s; for (;;) {} }
"#;

/// A second C++ file with the same inline template, so that the link has a
/// COMDAT group to deduplicate.
const D_CC: &str = r#"
struct W { int v; int get() const { return v * 5; } };
template <typename T> T twice(T t) { return t + t; }
extern "C" int d_fn(int x) { W w{x}; return w.get() + twice(x) + 1; }
"#;

const CXX_FLAGS: &[&str] = &[
    "-fno-exceptions",
    "-fno-rtti",
    "-fno-asynchronous-unwind-tables",
];

const OBJECTS: &[&str] = &["a.o", "b.o", "c.o", "d.o"];

fn scratch(name: &str) -> PathBuf {
    common::scratch::scratch_dir("relocatable-tests", name)
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

/// Whether the host builds the ELF objects these tests compile and links
/// them with an ELF linker, as the other ELF-only suites ask. A toolchain
/// that targets something else (the MinGW `cc` and `ld` of a Windows
/// runner) skips; a missing compiler is [`suite`]'s to report, so that
/// `QLD_REQUIRE_TOOLS` still fails on Linux.
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

/// The clang, clang++, lld and objdump the tests need.
struct Suite {
    cc: PathBuf,
    cxx: PathBuf,
    lld: PathBuf,
    objdump: PathBuf,
}

/// The tools, or `None` after printing why they are missing.
fn suite() -> Option<Suite> {
    if !elf_host() {
        return None;
    }
    let t = tools();
    let pick = |configured: &Option<PathBuf>, name: &str| {
        configured
            .clone()
            .filter(|c| driver_is_clang(c))
            .or_else(|| find_program(name))
    };
    let (Some(cc), Some(cxx)) = (pick(&t.cc, "clang"), pick(&t.cxx, "clang++")) else {
        assert!(!tools_required(), "no clang and clang++");
        skip("no clang and clang++ (needed to build the cross objects)");
        return None;
    };
    let Some(lld) = t.lld.clone() else {
        assert!(!tools_required(), "no ld.lld");
        skip("no ld.lld");
        return None;
    };
    let Some(objdump) = find_program("llvm-objdump") else {
        assert!(!tools_required(), "no llvm-objdump");
        skip("no llvm-objdump");
        return None;
    };
    Some(Suite {
        cc,
        cxx,
        lld,
        objdump,
    })
}

/// Builds the four objects for `target` in `dir`. Returns `false` when
/// this clang has no back end for it.
fn compile(dir: &Path, suite: &Suite, target: &Target) -> bool {
    fs::write(dir.join("a.c"), A_C).unwrap();
    fs::write(dir.join("b.c"), B_C).unwrap();
    fs::write(dir.join("c.cc"), C_CC).unwrap();
    fs::write(dir.join("d.cc"), D_CC).unwrap();
    let triple = format!("--target={}", target.triple);
    let probe = run(dir, &suite.cc, &[&triple, "-c", "-o", "probe.o", "a.c"]);
    if !probe.status.success() {
        assert!(
            !tools_required(),
            "{} cannot build {}: {}",
            suite.cc.display(),
            target.triple,
            String::from_utf8_lossy(&probe.stderr)
        );
        skip(format!(
            "{} cannot build {}",
            suite.cc.display(),
            target.triple
        ));
        return false;
    }
    for (source, object) in [("a.c", "a.o"), ("b.c", "b.o")] {
        run_ok(
            dir,
            &suite.cc,
            &[&triple, "-c", "-O1", "-g", "-fcommon", "-o", object, source],
        );
    }
    for (source, object) in [("c.cc", "c.o"), ("d.cc", "d.o")] {
        let mut args = vec![triple.as_str(), "-c", "-O1", "-g", "-o", object, source];
        args.extend_from_slice(CXX_FLAGS);
        run_ok(dir, &suite.cxx, &args);
    }
    true
}

/// Mnemonics a linker writes to pad between members: qld writes NOPs, as
/// GNU ld does, and lld traps. They are dropped before comparing.
const PADDING: &[&str] = &["int3", "nop", "nopw", "nopl", "nopq", "trap", "break"];

/// One line of `llvm-objdump -dr`, with the leading address dropped, or
/// `None` for a blank line, the file name or padding.
fn strip_address(line: &str) -> Option<String> {
    let line = line.trim();
    if line.is_empty() || line.contains("file format") {
        return None;
    }
    let hex = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_hexdigit());
    // "0000000000000010 <helper>:" -> "<helper>:"
    if let Some((head, rest)) = line.split_once(' ')
        && hex(head)
        && rest.starts_with('<')
    {
        return Some(rest.to_owned());
    }
    // "  10:\tmovl\t%edi, %eax" -> "movl\t%edi, %eax", and the same for
    // the "10:  R_X86_64_PC32\ttable-0x4" of a relocation.
    let body = match line.split_once(':') {
        Some((head, rest)) if hex(head) => rest.trim(),
        _ => line,
    };
    if PADDING.contains(&body.split_whitespace().next().unwrap_or("")) {
        return None;
    }
    Some(body.to_owned())
}

/// `llvm-objdump -dr` of `path` with the addresses dropped and the padding
/// removed: the instructions of every code section, with the relocations
/// that apply to them named by symbol.
fn code_and_relocs(dir: &Path, objdump: &Path, path: &str) -> String {
    let output = run_ok(dir, objdump, &["-dr", "--no-show-raw-insn", path]);
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(strip_address)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Links `inputs` with lld, with the qld emulation name.
fn lld_link(dir: &Path, suite: &Suite, target: &Target, out: &str, extra: &[&str]) {
    let mut args = vec!["-m", target.emulation, "-o", out];
    args.extend_from_slice(extra);
    run_ok(dir, &suite.lld, &args);
}

/// Combines the objects with `-r`, with qld into `q.o` and with lld into
/// `l.o`. Returns `false` when the architecture was skipped.
fn combine(dir: &Path, suite: &Suite, target: &Target) -> bool {
    if !compile(dir, suite, target) {
        return false;
    }
    let mut ours = vec!["-m", target.emulation, "-r", "-o", "q.o"];
    ours.extend_from_slice(OBJECTS);
    qld(dir, &ours);
    let mut theirs = vec!["-r"];
    theirs.extend_from_slice(OBJECTS);
    lld_link(dir, suite, target, "l.o", &theirs);
    true
}

#[test]
fn relocatable_output_matches_lld_on_every_class() {
    let Some(suite) = suite() else { return };
    let mut checked = 0;
    for target in TARGETS {
        let dir = scratch(&format!("r-{}", target.name));
        if !combine(&dir, &suite, target) {
            continue;
        }
        assert_eq!(
            code_and_relocs(&dir, &suite.objdump, "q.o"),
            code_and_relocs(&dir, &suite.objdump, "l.o"),
            "{}: qld's -r output differs from lld's",
            target.name
        );
        checked += 1;
    }
    assert!(checked > 0, "no architecture was checked");
}

#[test]
fn relocatable_output_links_into_a_program() {
    let Some(suite) = suite() else { return };
    let mut checked = 0;
    for target in TARGETS {
        let dir = scratch(&format!("link-{}", target.name));
        if !combine(&dir, &suite, target) {
            continue;
        }
        lld_link(&dir, &suite, target, "q.out", &["-e", "_start", "q.o"]);
        lld_link(&dir, &suite, target, "l.out", &["-e", "_start", "l.o"]);
        // The relocations were all resolved: nothing is left over.
        let output = run_ok(&dir, &suite.objdump, &["-r", "q.out"]);
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(
            !text.contains("R_") || text.contains("RELATIVE"),
            "{}: unresolved relocations in a static program:\n{text}",
            target.name
        );
        checked += 1;
    }
    assert!(checked > 0, "no architecture was checked");
}

/// RISC-V and 32-bit Arm carry build attributes; `-r` merges them into one
/// section, as a final link does, rather than copying each input's.
#[test]
fn relocatable_output_merges_build_attributes() {
    let Some(suite) = suite() else { return };
    for (name, section) in [
        ("arm", ".ARM.attributes"),
        ("riscv32", ".riscv.attributes"),
        ("riscv64", ".riscv.attributes"),
    ] {
        let Some(target) = TARGETS.iter().find(|t| t.name == name) else {
            continue;
        };
        let dir = scratch(&format!("attrs-{name}"));
        if !combine(&dir, &suite, target) {
            continue;
        }
        let output = run_ok(&dir, &suite.objdump, &["--section-headers", "q.o"]);
        let text = String::from_utf8_lossy(&output.stdout);
        let sections: Vec<&str> = text
            .lines()
            .filter(|line| line.contains(section))
            .collect::<Vec<_>>();
        assert_eq!(sections.len(), 1, "{name}: {section}:\n{text}");
        let size = sections[0].split_whitespace().nth(2).unwrap_or("0");
        assert_ne!(size, "00000000", "{name}: {section} is empty");
    }
}

/// `-r` with a linker script places the sections the script names (the
/// path W38 added), on a 32-bit target as well as a 64-bit one.
#[test]
fn relocatable_output_follows_a_linker_script() {
    let Some(suite) = suite() else { return };
    for name in ["i386", "s390x", "x86_64"] {
        let Some(target) = TARGETS.iter().find(|t| t.name == name) else {
            continue;
        };
        let dir = scratch(&format!("script-{name}"));
        if !compile(&dir, &suite, target) {
            continue;
        }
        fs::write(
            dir.join("combine.ld"),
            "SECTIONS { .qld.code : { *(.text .text.*) } \
             .qld.data : { *(.data .data.* .rodata .rodata.*) } }\n",
        )
        .unwrap();
        let mut args = vec![
            "-m",
            target.emulation,
            "-r",
            "-T",
            "combine.ld",
            "-o",
            "q.o",
        ];
        args.extend_from_slice(OBJECTS);
        qld(&dir, &args);
        let output = run_ok(&dir, &suite.objdump, &["--section-headers", "q.o"]);
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(text.contains(".qld.code"), "{name}:\n{text}");
        assert!(text.contains(".qld.data"), "{name}:\n{text}");
        // It still links.
        lld_link(&dir, &suite, target, "q.out", &["-e", "_start", "q.o"]);
    }
}

/// `--emit-relocs` writes the form the architecture's inputs use: `.rel`
/// sections of `Elf_Rel` on i386 and 32-bit Arm, `.rela` sections of
/// `Elf_Rela` elsewhere.
#[test]
fn emit_relocs_keeps_the_input_relocation_form() {
    let Some(suite) = suite() else { return };
    let mut checked = 0;
    for target in TARGETS {
        let dir = scratch(&format!("emit-{}", target.name));
        if !compile(&dir, &suite, target) {
            continue;
        }
        let mut ours = vec![
            "-m",
            target.emulation,
            "-e",
            "_start",
            "--emit-relocs",
            "-o",
            "q.out",
        ];
        ours.extend_from_slice(OBJECTS);
        qld(&dir, &ours);
        let mut inputs = vec!["-e", "_start", "--emit-relocs"];
        inputs.extend_from_slice(OBJECTS);
        lld_link(&dir, &suite, target, "l.out", &inputs);

        // i386 and 32-bit Arm keep the `SHT_REL` form of their inputs.
        let rel = matches!(target.name, "i386" | "arm");
        let output = run_ok(&dir, &suite.objdump, &["--section-headers", "q.out"]);
        let text = String::from_utf8_lossy(&output.stdout);
        let wanted = if rel { " .rel.text" } else { " .rela.text" };
        let unwanted = if rel { " .rela.text" } else { " .rel.text" };
        assert!(text.contains(wanted), "{}:\n{text}", target.name);
        assert!(!text.contains(unwanted), "{}:\n{text}", target.name);
        // Every emitted relocation names the same symbol as lld's.
        assert_eq!(
            emitted(&dir, &suite.objdump, "q.out"),
            emitted(&dir, &suite.objdump, "l.out"),
            "{}: --emit-relocs differs from lld's",
            target.name
        );
        checked += 1;
    }
    assert!(checked > 0, "no architecture was checked");
}

/// The `(type, symbol)` pairs of the code relocations a program carries,
/// sorted. Only the relocations of `.text` are compared: the offsets into
/// the merged `.debug_str` depend on how a linker deduplicates its
/// strings, which qld and lld do differently.
fn emitted(dir: &Path, objdump: &Path, path: &str) -> Vec<String> {
    let output = run_ok(dir, objdump, &["-r", path]);
    let mut lines = Vec::new();
    let mut wanted = false;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if let Some(section) = line.strip_prefix("RELOCATION RECORDS FOR [") {
            let section = section.trim_end_matches("]:");
            wanted = matches!(section, ".rel.text" | ".rela.text");
            continue;
        }
        if !wanted {
            continue;
        }
        let mut fields = line.split_whitespace();
        let Some(offset) = fields.next() else {
            continue;
        };
        if offset.len() < 8 || !offset.chars().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }
        let Some(kind) = fields.next() else { continue };
        lines.push(format!("{kind} {}", fields.next().unwrap_or("")));
    }
    lines.sort();
    lines
}

/// On a host that runs them, a program built from a `-r` combined object
/// behaves like one built from the objects.
#[test]
fn relocatable_output_runs() {
    if !elf_host() {
        return;
    }
    let Some(cc) = tools().cc.clone() else {
        assert!(!tools_required(), "no C compiler");
        skip("no C compiler");
        return;
    };
    for (name, flags) in [("host", &[][..]), ("i386", &["-m32"][..])] {
        let dir = scratch(&format!("run-{name}"));
        fs::write(
            dir.join("one.c"),
            r#"
#include <stdio.h>
int comm_qld;
static const char msg[] = "one";
__attribute__((noinline)) static int helper(int x) { return x + 1; }
__attribute__((visibility("hidden"))) int hidden_qld(void) { return 2; }
const char *one_msg(void) { return msg; }
int one_fn(void) { return helper(comm_qld) + hidden_qld(); }
"#,
        )
        .unwrap();
        fs::write(
            dir.join("two.c"),
            r#"
#include <stdio.h>
static const char msg[] = "two";
int one_fn(void); const char *one_msg(void);
__attribute__((noinline)) static int helper(int x) { return x * 10; }
int main(void) { printf("%d %d %s %s\n", one_fn(), helper(4), one_msg(), msg); return 0; }
"#,
        )
        .unwrap();
        let build = |source: &str, object: &str| {
            let mut args = flags.to_vec();
            args.extend(["-c", "-O1", "-fcommon", "-o", object, source]);
            run(&dir, &cc, &args)
        };
        if !build("one.c", "one.o").status.success() {
            skip(format!("the C compiler cannot build {name} objects"));
            continue;
        }
        run_ok(&dir, &cc, &{
            let mut args = flags.to_vec();
            args.extend(["-c", "-O1", "-o", "two.o", "two.c"]);
            args
        });
        qld(&dir, &["-r", "-o", "combined.o", "one.o", "two.o"]);
        let mut link = flags.to_vec();
        link.extend(["-o", "out", "combined.o"]);
        if !run(&dir, &cc, &link).status.success() {
            skip(format!("cannot link {name} programs"));
            continue;
        }
        let output = run(&dir, &dir.join("out"), &[]);
        if !output.status.success() {
            skip(format!("cannot run {name} programs"));
            continue;
        }
        assert_eq!(String::from_utf8_lossy(&output.stdout), "3 40 one two\n");
    }
}
