//! Linker-script-driven layout (workstream W19, roadmap M3).
//!
//! Each test assembles inputs with the system `as`, links them with GNU ld
//! and with the `qld` binary under test using the same script and options,
//! and compares what `readelf` and `nm` report: section names, types,
//! flags, addresses, file offsets, sizes and alignments; program headers;
//! and symbol addresses. Raw outputs (`--oformat binary`, `ihex`, `srec`)
//! are compared byte for byte. Symbol *types* as `nm` prints them are not
//! compared: they follow the symbol table's section indexes, which the
//! symbol table writer chooses.
//!
//! A test prints `SKIPPED:` and passes when a tool it needs is missing.
//! Error tests check messages and that malformed scripts never panic.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;

use qld::args::{InputAttrs, InputKind, LinkOptions, OutputKind};
use qld::diag::Collect;

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("script-link-tests")
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

/// The GNU ld whose layout qld is compared against, when it is recent
/// enough. Layout details changed between binutils releases (2.42 orders
/// some orphan sections differently from 2.46, gives an empty `PT_LOAD` a
/// different file offset, and evaluates `NEXT` against another page size),
/// so comparisons only run against 2.44 or newer. qld's own assertions run
/// either way.
fn comparable_gnu_ld() -> Option<PathBuf> {
    let ld = gnu_ld()?;
    let out = Command::new(&ld).arg("--version").output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let version = text.lines().next()?.split_whitespace().last()?;
    let mut parts = version.split('.');
    let major: u32 = parts.next()?.parse().ok()?;
    let minor: u32 = parts
        .next()?
        .trim_end_matches(|c: char| !c.is_ascii_digit())
        .parse()
        .ok()?;
    ((major, minor) >= (2, 44)).then_some(ld)
}

/// GNU ld, when installed: `ld.bfd`, or `ld` if it says it is GNU ld.
fn gnu_ld() -> Option<PathBuf> {
    if let Some(bfd) = tool("ld.bfd") {
        return Some(bfd);
    }
    let ld = tool("ld")?;
    let out = Command::new(&ld).arg("--version").output().ok()?;
    String::from_utf8_lossy(&out.stdout)
        .starts_with("GNU ld")
        .then_some(ld)
}

fn host_ok() -> bool {
    cfg!(all(target_os = "linux", target_arch = "x86_64"))
}

macro_rules! require_tools {
    () => {
        if !host_ok() {
            println!("SKIPPED: host is not x86-64 Linux");
            return;
        }
        for name in ["as", "readelf", "nm"] {
            if tool(name).is_none() {
                println!("SKIPPED: {name} not found");
                return;
            }
        }
    };
}

fn run(dir: &Path, program: &Path, args: &[&str]) -> Output {
    Command::new(program)
        .args(args)
        .current_dir(dir)
        .env("LC_ALL", "C")
        .output()
        .unwrap_or_else(|e| panic!("cannot run {}: {e}", program.display()))
}

fn stdout_ok(dir: &Path, program: &str, args: &[&str]) -> String {
    let output = run(dir, Path::new(program), args);
    assert!(
        output.status.success(),
        "`{program} {}` failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn qld_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_qld"))
}

fn assemble(dir: &Path, name: &str, source: &str) {
    fs::write(dir.join(format!("{name}.s")), source).unwrap();
    stdout_ok(
        dir,
        "as",
        &["--64", "-o", &format!("{name}.o"), &format!("{name}.s")],
    );
}

/// What the comparison looks at.
#[derive(Debug, PartialEq)]
struct Summary {
    sections: Vec<String>,
    segments: Vec<String>,
    symbols: Vec<String>,
}

/// Sections whose contents are the linkers' own (version strings, symbol
/// tables) and differ by design.
const IGNORED_SECTIONS: &[&str] = &[".comment", ".symtab", ".strtab", ".shstrtab"];

fn summarize(dir: &Path, file: &str) -> Summary {
    let sections_text = stdout_ok(dir, "readelf", &["-SW", file]);
    let mut sections = Vec::new();
    for line in sections_text.lines() {
        let Some(rest) = line.trim_start().strip_prefix('[') else {
            continue;
        };
        let Some((_, rest)) = rest.split_once(']') else {
            continue;
        };
        let fields: Vec<&str> = rest.split_whitespace().collect();
        if fields
            .first()
            .is_none_or(|n| *n == "Nr" || IGNORED_SECTIONS.contains(n))
        {
            continue;
        }
        if fields.len() < 9 {
            continue;
        }
        // name type addr off size es [flags] lk inf al
        let align = fields.last().copied().unwrap_or_default();
        let flags = if fields.len() >= 10 { fields[6] } else { "" };
        sections.push(format!(
            "{} {} addr={} off={} size={} flags={flags} align={align}",
            fields[0], fields[1], fields[2], fields[3], fields[4]
        ));
    }
    let segments_text = stdout_ok(dir, "readelf", &["-lW", file]);
    let segments = segments_text
        .lines()
        .map(str::trim)
        .filter(|l| {
            [
                "LOAD", "NOTE", "TLS", "GNU_", "PHDR", "INTERP", "DYNAMIC", "NULL",
            ]
            .iter()
            .any(|p| l.starts_with(p))
        })
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect();
    let nm = stdout_ok(dir, "nm", &["-n", file]);
    let symbols = nm
        .lines()
        .filter_map(|l| {
            let fields: Vec<&str> = l.split_whitespace().collect();
            (fields.len() == 3).then(|| format!("{} {}", fields[0], fields[2]))
        })
        .collect();
    Summary {
        sections,
        segments,
        symbols,
    }
}

/// Links `args` with GNU ld and qld into `gnu.out`/`qld.out` and asserts
/// the layouts are the same. Returns false (after printing `SKIPPED`) when
/// GNU ld is not installed.
fn compare(dir: &Path, args: &[&str]) -> bool {
    let Some(ld) = comparable_gnu_ld() else {
        println!("SKIPPED: no GNU ld 2.44 or newer to compare against");
        return false;
    };
    let mut gnu_args = args.to_vec();
    gnu_args.extend(["-o", "gnu.out"]);
    let gnu = run(dir, &ld, &gnu_args);
    assert!(
        gnu.status.success(),
        "GNU ld failed: {}",
        String::from_utf8_lossy(&gnu.stderr)
    );
    let mut qld_args = args.to_vec();
    qld_args.extend(["-o", "qld.out"]);
    let qld = run(dir, &qld_path(), &qld_args);
    assert!(
        qld.status.success(),
        "qld {} failed: {}",
        qld_args.join(" "),
        String::from_utf8_lossy(&qld.stderr)
    );
    let expected = summarize(dir, "gnu.out");
    let actual = summarize(dir, "qld.out");
    assert_eq!(
        expected.sections,
        actual.sections,
        "sections differ in {}",
        dir.display()
    );
    assert_eq!(
        expected.segments,
        actual.segments,
        "segments differ in {}",
        dir.display()
    );
    assert_eq!(
        expected.symbols,
        actual.symbols,
        "symbols differ in {}",
        dir.display()
    );
    true
}

/// Links with both linkers in each raw format and compares the bytes.
fn compare_raw(dir: &Path, args: &[&str]) {
    let Some(ld) = gnu_ld() else {
        println!("SKIPPED: GNU ld not found");
        return;
    };
    for format in ["binary", "ihex", "srec"] {
        // Same output name for both: srec records it.
        let name = format!("image.{format}");
        let mut full = args.to_vec();
        full.extend(["--oformat", format, "-o", &name]);
        let gnu = run(dir, &ld, &full);
        assert!(gnu.status.success(), "GNU ld --oformat {format} failed");
        let expected = fs::read(dir.join(&name)).unwrap();
        let qld = run(dir, &qld_path(), &full);
        assert!(
            qld.status.success(),
            "qld --oformat {format} failed: {}",
            String::from_utf8_lossy(&qld.stderr)
        );
        let actual = fs::read(dir.join(&name)).unwrap();
        assert!(
            expected == actual,
            "{format} output differs ({} vs {} bytes) in {}",
            expected.len(),
            actual.len(),
            dir.display()
        );
    }
}

/// Runs qld alone; returns (success, stderr).
fn qld_only(dir: &Path, args: &[&str]) -> (bool, String) {
    let out = run(dir, &qld_path(), args);
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(!stderr.contains("panicked"), "qld panicked: {stderr}");
    (out.status.success(), stderr)
}

// ---------------------------------------------------------------------------
// Inputs.
// ---------------------------------------------------------------------------

const START: &str = r#"
.section .vectors,"a"
.globl vectors
vectors:
.quad reset_handler
.quad nmi_handler
.quad 0

.text
.globl reset_handler
reset_handler:
  mov $_sidata, %rsi
  mov $_sdata, %rdi
  mov $_edata, %rcx
  sub %rdi, %rcx
  rep movsb
  mov $_sbss, %rdi
  mov $_ebss, %rcx
  sub %rdi, %rcx
  xor %eax, %eax
  rep stosb
  call main
  hlt

.section .text.nmi,"ax"
.p2align 4
.globl nmi_handler
nmi_handler:
  iretq

.section .rodata
.globl message
message:
.asciz "hello, bare metal"

.data
.globl counter
.p2align 3
counter:
.quad 42

.bss
.globl buffer
buffer:
.zero 100
"#;

const MAIN: &str = r#"
.section .text.main,"ax"
.globl main
main:
  mov counter(%rip), %rax
  lea message(%rip), %rdx
  ret

.section .data.table,"aw"
.p2align 4
table:
.quad main, counter

.section .init_array.100,"aw"
.quad main
.section .init_array.5,"aw"
.quad main

.section .bss.big,"aw",@nobits
.p2align 6
big:
.zero 256
"#;

/// A Cortex-M style layout on x86-64: a vector table at the start of flash,
/// code and read-only data in flash, `.data` running in RAM but loaded from
/// flash, and `.bss` in RAM.
const FLASH_SCRIPT: &str = r#"
ENTRY(reset_handler)
MEMORY
{
  FLASH (rx)  : ORIGIN = 0x08000000, LENGTH = 64K
  RAM   (rwx) : ORIGIN = 0x20000000, LENGTH = 16K
}
_estack = ORIGIN(RAM) + LENGTH(RAM);
SECTIONS
{
  .isr_vector :
  {
    . = ALIGN(8);
    KEEP(*(.vectors))
    . = ALIGN(8);
  } >FLASH
  .text :
  {
    *(.text)
    *(.text*)
    . = ALIGN(8);
    _etext = .;
  } >FLASH =0x90909090
  .rodata : { *(.rodata) *(.rodata*) BYTE(0x11) SHORT(0x2233) LONG(0x44556677) QUAD(0x8899aabbccddeeff) . = ALIGN(8); } >FLASH
  .init_array :
  {
    PROVIDE_HIDDEN (__init_array_start = .);
    KEEP (*(SORT_BY_INIT_PRIORITY(.init_array.*)))
    PROVIDE_HIDDEN (__init_array_end = .);
  } >FLASH
  _sidata = LOADADDR(.data);
  .data :
  {
    . = ALIGN(8);
    _sdata = .;
    *(.data)
    *(.data*)
    . = ALIGN(8);
    _edata = .;
  } >RAM AT> FLASH
  .bss (NOLOAD) :
  {
    _sbss = .;
    *(.bss)
    *(.bss*)
    *(COMMON)
    . = ALIGN(8);
    _ebss = .;
  } >RAM
  ASSERT(SIZEOF(.data) + SIZEOF(.bss) <= LENGTH(RAM), "RAM overflow")
  /DISCARD/ : { *(.note.gnu.property) }
}
"#;

fn bare_metal(dir: &Path) {
    assemble(dir, "start", START);
    assemble(dir, "main", MAIN);
}

// ---------------------------------------------------------------------------
// Layout tests against GNU ld.
// ---------------------------------------------------------------------------

#[test]
fn memory_regions_load_addresses_and_data_commands() {
    require_tools!();
    let dir = scratch("flash");
    bare_metal(&dir);
    fs::write(dir.join("flash.ld"), FLASH_SCRIPT).unwrap();
    if compare(&dir, &["-T", "flash.ld", "start.o", "main.o"]) {
        compare_raw(&dir, &["-T", "flash.ld", "start.o", "main.o"]);
    }
}

#[test]
fn program_headers_from_phdrs() {
    require_tools!();
    let dir = scratch("phdrs");
    bare_metal(&dir);
    fs::write(
        dir.join("phdrs.ld"),
        r#"
ENTRY(reset_handler)
PHDRS
{
  headers PT_PHDR PHDRS;
  text PT_LOAD FILEHDR PHDRS FLAGS(5);
  data PT_LOAD AT(0x800000) FLAGS(6);
  note PT_NOTE;
}
SECTIONS
{
  . = 0x400000 + SIZEOF_HEADERS;
  .text : { *(.vectors) *(.text*) } :text
  .rodata : { *(.rodata*) } :text
  .note.gnu.property : { *(.note.gnu.property) } :text :note
  . = ALIGN(0x1000);
  .data : { *(.data*) *(.init_array*) } :data
  .bss : { *(.bss*) } :data
  _sidata = LOADADDR(.data); _sdata = ADDR(.data); _edata = _sdata + SIZEOF(.data);
  _sbss = ADDR(.bss); _ebss = _sbss + SIZEOF(.bss);
}
"#,
    )
    .unwrap();
    compare(&dir, &["-T", "phdrs.ld", "start.o", "main.o"]);
}

#[test]
fn orphans_follow_sections_with_similar_flags() {
    require_tools!();
    let dir = scratch("orphans");
    bare_metal(&dir);
    assemble(
        &dir,
        "extra",
        r#"
.section .fast_code,"ax"
.globl fast
fast: ret
.section .config,"a"
.quad 1
.section .state,"aw"
.quad 2
.section .scratch,"aw",@nobits
.zero 32
.section .info_only,""
.asciz "not allocated"
.section .note.custom,"a",@note
.long 4, 4, 1
.asciz "abc"
"#,
    );
    fs::write(
        dir.join("orphans.ld"),
        r#"
ENTRY(reset_handler)
SECTIONS
{
  . = 0x100000;
  .text : { *(.text*) }
  _etext = .;
  . = ALIGN(0x1000);
  .rodata : { *(.rodata*) *(.vectors) }
  .data : { *(.data*) *(.init_array*) }
  .bss : { *(.bss*) }
  _sidata = .; _sdata = .; _edata = .; _sbss = ADDR(.bss); _ebss = .;
}
"#,
    )
    .unwrap();
    compare(&dir, &["-T", "orphans.ld", "start.o", "main.o", "extra.o"]);
    // Without SECTIONS every section is an orphan.
    fs::write(
        dir.join("none.ld"),
        "_sidata = 0; _sdata = 0; _edata = 0; _sbss = 0; _ebss = 0;\n",
    )
    .unwrap();
    compare(&dir, &["-T", "none.ld", "start.o", "main.o", "extra.o"]);
}

#[test]
fn overlays_and_expressions() {
    require_tools!();
    let dir = scratch("overlay");
    bare_metal(&dir);
    assemble(
        &dir,
        "ovl",
        r#"
.section .ov1,"ax"
.globl in_ov1
in_ov1: .fill 20, 1, 0xc3
.section .ov2,"ax"
.globl in_ov2
in_ov2: .fill 40, 1, 0xc3
"#,
    );
    fs::write(
        dir.join("ovl.ld"),
        r#"
ENTRY(reset_handler)
SECTIONS
{
  . = 0x10000;
  .text ALIGN(0x100) : SUBALIGN(32) { *(.vectors) *(.text*) }
  text_end = .;
  OVERLAY 0x20000 : AT (0x30000)
  {
    .ov1 { *(.ov1) }
    .ov2 { *(.ov2) }
  }
  after_overlay = .;
  .rodata MAX(., 0x28000) : { *(.rodata*) }
  sizes = SIZEOF(.ov1) + SIZEOF(.ov2) + ALIGNOF(.text);
  next_align = ALIGNOF(NEXT_SECTION);
  .data : ALIGN(0x40) { *(.data*) *(.init_array*) LONG(DEFINED(text_end) ? 1 : 2) LONG(DEFINED(nosuch) ? 3 : 4) }
  .bss : { *(.bss*) }
  _sidata = ABSOLUTE(LOADADDR(.data)); _sdata = ADDR(.data); _edata = . ;
  _sbss = ADDR(.bss); _ebss = _sbss + SIZEOF(.bss);
  page = CONSTANT(MAXPAGESIZE);
  log = LOG2CEIL(sizes);
}
"#,
    )
    .unwrap();
    if compare(&dir, &["-T", "ovl.ld", "start.o", "main.o", "ovl.o"]) {
        compare_raw(&dir, &["-T", "ovl.ld", "start.o", "main.o", "ovl.o"]);
    }
}

#[test]
fn input_section_selection_and_sorting() {
    require_tools!();
    let dir = scratch("select");
    bare_metal(&dir);
    assemble(
        &dir,
        "sorted",
        r#"
.section .table.zeta,"a"
.byte 1
.section .table.alpha,"a"
.p2align 3
.quad 2
.section .table.mid,"a"
.p2align 1
.short 3
.section .ctors.65000,"aw"
.quad 4
.section .ctors.10,"aw"
.quad 5
"#,
    );
    stdout_ok(&dir, "ar", &["rcs", "libsorted.a", "sorted.o"]);
    fs::write(
        dir.join("select.ld"),
        r#"
ENTRY(reset_handler)
SECTIONS
{
  . = 0x200000;
  .text : { *(EXCLUDE_FILE(*main.o) .text*) main.o(.text*) *(.vectors) }
  .by_name : { KEEP(*(SORT_BY_NAME(.table.*))) }
  .by_align : { libsorted.a:sorted.o(SORT_BY_ALIGNMENT(.table.*)) }
  .ctors : { KEEP(*(SORT_BY_INIT_PRIORITY(.ctors.*))) }
  .rodata : { INPUT_SECTION_FLAGS (!SHF_WRITE) *(.rodata*) }
  .data : { *(.data*) *(.init_array*) }
  .bss : { *(.bss*) *(COMMON) }
  _sidata = 0; _sdata = ADDR(.data); _edata = .; _sbss = ADDR(.bss); _ebss = .;
  /DISCARD/ : { *(.note.*) }
}
"#,
    )
    .unwrap();
    compare(
        &dir,
        &[
            "-T",
            "select.ld",
            "start.o",
            "main.o",
            "--whole-archive",
            "libsorted.a",
        ],
    );
}

#[test]
fn region_aliases_and_attribute_placement() {
    require_tools!();
    let dir = scratch("regions");
    bare_metal(&dir);
    fs::write(
        dir.join("regions.ld"),
        r#"
ENTRY(reset_handler)
MEMORY
{
  rom (rx) : ORIGIN = 0x1000, LENGTH = 0x4000
  ram (rw!x) : ORIGIN = 0x80000, LENGTH = 0x8000
}
REGION_ALIAS("REGION_TEXT", rom);
SECTIONS
{
  .text : { *(.vectors) *(.text*) } > REGION_TEXT
  .rodata : { *(.rodata*) }
  .data : { *(.data*) *(.init_array*) }
  .bss : { *(.bss*) }
  _sidata = LOADADDR(.data); _sdata = ADDR(.data); _edata = _sdata + SIZEOF(.data);
  _sbss = ADDR(.bss); _ebss = _sbss + SIZEOF(.bss);
  /DISCARD/ : { *(.note.*) }
}
"#,
    )
    .unwrap();
    compare(&dir, &["-T", "regions.ld", "start.o", "main.o"]);
}

#[test]
fn insert_into_the_default_layout() {
    require_tools!();
    let dir = scratch("insert");
    assemble(
        &dir,
        "prog",
        r#"
.globl _start
.text
_start:
  mov $60, %eax
  xor %edi, %edi
  syscall
.section .mydata,"aw"
.globl mydata_item
mydata_item: .quad 7
.data
.quad 1
"#,
    );
    fs::write(
        dir.join("insert.ld"),
        r#"
SECTIONS
{
  .mydata : { __mydata_start = .; KEEP(*(.mydata)) __mydata_end = .; }
}
INSERT AFTER .data;
"#,
    )
    .unwrap();
    compare(&dir, &["-static", "-T", "insert.ld", "prog.o"]);
}

#[test]
fn command_line_section_addresses() {
    require_tools!();
    let dir = scratch("ttext");
    assemble(
        &dir,
        "prog",
        r#"
.globl _start
.text
_start:
  mov $60, %eax
  xor %edi, %edi
  syscall
.data
.globl value
value: .quad 1
.bss
.zero 16
"#,
    );
    compare(
        &dir,
        &["-static", "-Ttext=0x500000", "-Tdata=0x600000", "prog.o"],
    );
    compare(
        &dir,
        &["-static", "--section-start=.bss=0x700000", "prog.o"],
    );
    compare(&dir, &["-static", "-N", "prog.o"]);
    compare(&dir, &["-static", "-n", "prog.o"]);
    compare(
        &dir,
        &["-static", "-Ttext-segment=0x800000", "-N", "prog.o"],
    );
}

#[test]
fn binary_inputs_define_start_end_and_size() {
    require_tools!();
    let dir = scratch("binary-input");
    bare_metal(&dir);
    fs::write(dir.join("blob-1.bin"), b"payload bytes\0\x01\x02").unwrap();
    fs::write(dir.join("flash.ld"), FLASH_SCRIPT).unwrap();
    let args = [
        "-T",
        "flash.ld",
        "start.o",
        "main.o",
        "-b",
        "binary",
        "blob-1.bin",
        "-b",
        "default",
    ];
    if compare(&dir, &args) {
        let nm = stdout_ok(&dir, "nm", &["qld.out"]);
        assert!(nm.contains("_binary_blob_1_bin_start"), "{nm}");
        assert!(nm.contains("A _binary_blob_1_bin_size"), "{nm}");
        compare_raw(&dir, &args);
    }
}

// ---------------------------------------------------------------------------
// Errors.
// ---------------------------------------------------------------------------

#[test]
fn region_overflow_is_reported_like_gnu() {
    require_tools!();
    let dir = scratch("overflow");
    bare_metal(&dir);
    fs::write(
        dir.join("small.ld"),
        FLASH_SCRIPT.replace("LENGTH = 64K", "LENGTH = 16"),
    )
    .unwrap();
    let (ok, stderr) = qld_only(&dir, &["-T", "small.ld", "start.o", "main.o", "-o", "out"]);
    assert!(!ok);
    assert!(
        stderr.contains("will not fit in region `FLASH'"),
        "{stderr}"
    );
    assert!(stderr.contains("region `FLASH' overflowed by"), "{stderr}");
    if let Some(ld) = gnu_ld() {
        let gnu = run(
            &dir,
            &ld,
            &["-T", "small.ld", "start.o", "main.o", "-o", "gnu"],
        );
        let gnu_err = String::from_utf8_lossy(&gnu.stderr);
        let wanted = gnu_err
            .lines()
            .find(|l| l.contains("overflowed by"))
            .and_then(|l| l.split_once("region"))
            .map(|(_, rest)| rest.to_string())
            .unwrap_or_default();
        assert!(stderr.contains(&wanted), "GNU: {gnu_err}\nqld: {stderr}");
    }
}

#[test]
fn failed_assertions_fail_the_link() {
    require_tools!();
    let dir = scratch("assert");
    bare_metal(&dir);
    fs::write(
        dir.join("assert.ld"),
        FLASH_SCRIPT.replace(
            "<= LENGTH(RAM), \"RAM overflow\"",
            "<= 8, \"RAM too small for this image\"",
        ),
    )
    .unwrap();
    let (ok, stderr) = qld_only(&dir, &["-T", "assert.ld", "start.o", "main.o", "-o", "out"]);
    assert!(!ok);
    assert!(stderr.contains("RAM too small for this image"), "{stderr}");
}

#[test]
fn diverging_scripts_are_errors() {
    require_tools!();
    let dir = scratch("diverge");
    bare_metal(&dir);
    // Each pass moves `.text` past where the previous pass put `.data`.
    fs::write(
        dir.join("diverge.ld"),
        r#"
SECTIONS
{
  .text : { *(.text*) *(.vectors) }
  .gap ADDR(.data) + SIZEOF(.data) + 0x1000 : { LONG(0) }
  .data : { *(.data*) *(.init_array*) }
  _sidata = 0; _sdata = 0; _edata = 0; _sbss = 0; _ebss = 0;
}
"#,
    )
    .unwrap();
    let (ok, stderr) = qld_only(
        &dir,
        &["-T", "diverge.ld", "start.o", "main.o", "-o", "out"],
    );
    assert!(!ok, "{stderr}");
    assert!(stderr.contains("did not converge"), "{stderr}");
}

#[test]
fn undefined_symbols_in_expressions_are_errors() {
    require_tools!();
    let dir = scratch("undefined-expr");
    bare_metal(&dir);
    fs::write(
        dir.join("undef.ld"),
        FLASH_SCRIPT.replace(
            "_estack = ORIGIN(RAM) + LENGTH(RAM);",
            "_estack = no_such_symbol + 1;",
        ),
    )
    .unwrap();
    let (ok, stderr) = qld_only(&dir, &["-T", "undef.ld", "start.o", "main.o", "-o", "out"]);
    assert!(!ok);
    assert!(
        stderr.contains("undefined symbol `no_such_symbol' referenced in expression"),
        "{stderr}"
    );
}

#[test]
fn orphan_handling_error_names_the_section() {
    require_tools!();
    let dir = scratch("orphan-error");
    bare_metal(&dir);
    fs::write(
        dir.join("strict.ld"),
        FLASH_SCRIPT.replace("*(.text*)", "*(.text)"),
    )
    .unwrap();
    let (ok, stderr) = qld_only(
        &dir,
        &[
            "--orphan-handling=error",
            "-T",
            "strict.ld",
            "start.o",
            "main.o",
            "-o",
            "out",
        ],
    );
    assert!(!ok);
    assert!(
        stderr.contains("unplaced orphan section `.text.nmi' from `start.o'"),
        "{stderr}"
    );
}

#[test]
fn nocrossrefs_prohibits_references_between_sections() {
    require_tools!();
    let dir = scratch("nocrossrefs");
    assemble(&dir, "start", START);
    assemble(&dir, "main", MAIN);
    // `main` (in .text) reads `counter` (in .data), and .data holds a
    // pointer to `main`.
    let script = FLASH_SCRIPT.replace(
        "ENTRY(reset_handler)",
        "ENTRY(reset_handler)\nNOCROSSREFS(.text .data)\n",
    );
    fs::write(dir.join("ncr.ld"), &script).unwrap();
    let args = ["-T", "ncr.ld", "start.o", "main.o", "-o", "out"];
    let (ok, stderr) = qld_only(&dir, &args);
    assert!(!ok, "{stderr}");
    assert!(
        stderr.contains("prohibited cross reference from .text to `counter' in .data"),
        "{stderr}"
    );
    assert!(
        stderr.contains("prohibited cross reference from .data to `main' in .text"),
        "{stderr}"
    );
    if let Some(ld) = gnu_ld() {
        let gnu = run(
            &dir,
            &ld,
            &["-T", "ncr.ld", "start.o", "main.o", "-o", "gnu"],
        );
        let gnu_err = String::from_utf8_lossy(&gnu.stderr);
        assert!(!gnu.status.success(), "{gnu_err}");
        for line in gnu_err.lines().filter(|l| l.contains("prohibited")) {
            let (_, message) = line.split_once("prohibited").unwrap_or(("", line));
            assert!(
                stderr.contains(message.trim()),
                "GNU: {line}\nqld: {stderr}"
            );
        }
    }

    // NOCROSSREFS_TO only checks references to the first section, so
    // .data -> .text stays legal while .text -> .data does not.
    let script = script.replace("NOCROSSREFS(.text .data)", "NOCROSSREFS_TO(.data .text)");
    fs::write(dir.join("ncr.ld"), &script).unwrap();
    let (ok, stderr) = qld_only(&dir, &args);
    assert!(!ok, "{stderr}");
    assert!(
        stderr.contains("prohibited cross reference from .text to `counter' in .data"),
        "{stderr}"
    );
    assert!(
        !stderr.contains("from .data to `main'"),
        "references to the first section only: {stderr}"
    );
}

/// Truncated and corrupted scripts must give errors, never panics.
#[test]
fn corrupted_scripts_never_panic() {
    require_tools!();
    let dir = scratch("corrupt-script");
    bare_metal(&dir);
    let objects: Vec<(String, Arc<[u8]>)> = ["start.o", "main.o"]
        .iter()
        .map(|n| (n.to_string(), Arc::from(fs::read(dir.join(n)).unwrap())))
        .collect();
    let original = FLASH_SCRIPT.as_bytes();
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
    let script = dir.join("broken.ld");
    for round in 0..400 {
        let mut data = original.to_vec();
        match round % 3 {
            0 => data.truncate((next() as usize) % original.len()),
            1 => {
                for _ in 0..3 {
                    let at = (next() as usize) % data.len();
                    data[at] = b"(){};=.*+-<>ABCDEFGHILMNOPRSTXYZ0123456789,:!/\"x"
                        [(next() as usize) % 47];
                }
            }
            _ => {
                let at = (next() as usize) % data.len();
                data.insert(at, b"0123456789+-*/()"[(next() as usize) % 16]);
            }
        }
        fs::write(&script, &data).unwrap();
        let mut options = LinkOptions::new();
        options.kind = OutputKind::StaticExecutable;
        options.output = Some(dir.join("out"));
        options.push_input(InputKind::Script(script.clone()), InputAttrs::default());
        for (name, bytes) in &objects {
            options.push_input(
                InputKind::Bytes {
                    name: name.clone(),
                    data: Arc::clone(bytes),
                },
                InputAttrs::default(),
            );
        }
        let sink = Collect::new();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            pool.install(|| qld::elf::link(&options, &sink))
        }));
        assert!(
            result.is_ok(),
            "panic on script corruption round {round}:\n{}",
            String::from_utf8_lossy(&data)
        );
    }
}

// ---------------------------------------------------------------------------
// Relocatable output with a script (`-r -T`), as the kernel links modules.
// ---------------------------------------------------------------------------

/// One section of a relocatable file: name, type, flags, size, alignment,
/// entry size and contents.
type RelSection = (String, u32, u64, u64, u64, u64, Vec<u8>);

/// The sections of a relocatable ELF file in header order. Contents are
/// left empty for `SHT_NOBITS`, groups, relocations (compared through
/// readelf) and the linkers' own symbol and string tables.
fn relocatable_sections(path: &Path) -> Vec<RelSection> {
    let data = fs::read(path).unwrap();
    let word = |at: usize, n: usize| -> u64 {
        let mut bytes = [0u8; 8];
        bytes[..n].copy_from_slice(&data[at..at + n]);
        u64::from_le_bytes(bytes)
    };
    let shoff = word(0x28, 8) as usize;
    let shnum = word(0x3c, 2) as usize;
    let shstrndx = word(0x3e, 2) as usize;
    let header = |i: usize| shoff + i * 64;
    let names = word(header(shstrndx) + 24, 8) as usize;
    let mut out = Vec::new();
    for i in 1..shnum {
        let h = header(i);
        let name_at = names + word(h, 4) as usize;
        let end = data[name_at..].iter().position(|&b| b == 0).unwrap();
        let name = String::from_utf8_lossy(&data[name_at..name_at + end]).into_owned();
        let sh_type = word(h + 4, 4) as u32;
        let (offset, size) = (word(h + 24, 8) as usize, word(h + 32, 8));
        let contents = if matches!(sh_type, 2 | 3 | 4 | 8 | 17) {
            Vec::new()
        } else {
            data[offset..offset + size as usize].to_vec()
        };
        let size = if matches!(sh_type, 2 | 3) { 0 } else { size };
        out.push((
            name,
            sh_type,
            word(h + 8, 8),
            size,
            word(h + 48, 8),
            word(h + 56, 8),
            contents,
        ));
    }
    out
}

/// Symbols as `name value section binding type visibility`, sorted, without
/// section symbols and file symbols.
fn relocatable_symbols(dir: &Path, file: &str) -> Vec<String> {
    let text = stdout_ok(dir, "readelf", &["-sW", file]);
    let sections = stdout_ok(dir, "readelf", &["-SW", file]);
    let section_name = |index: &str| -> String {
        let Ok(index) = index.parse::<usize>() else {
            return index.to_string();
        };
        sections
            .lines()
            .find_map(|l| {
                let rest = l.trim_start().strip_prefix('[')?;
                let (number, rest) = rest.split_once(']')?;
                (number.trim().parse::<usize>().ok()? == index)
                    .then(|| rest.split_whitespace().next().unwrap_or("").to_string())
            })
            .unwrap_or_default()
    };
    let mut symbols: Vec<String> = text
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            if f.len() < 8 || !f[0].ends_with(':') || f[3] == "SECTION" || f[3] == "FILE" {
                return None;
            }
            Some(format!(
                "{} {} {} {} {} {}",
                f[7],
                f[1],
                section_name(f[6]),
                f[4],
                f[3],
                f[5]
            ))
        })
        .collect();
    symbols.sort();
    symbols
}

/// Relocations as `section: offset type symbol+addend`.
fn relocatable_relocations(dir: &Path, file: &str) -> Vec<String> {
    let text = stdout_ok(dir, "readelf", &["-rW", file]);
    let mut section = String::new();
    let mut out = Vec::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("Relocation section '") {
            section = rest.split('\'').next().unwrap_or("").to_string();
            continue;
        }
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() >= 3 && f[0].chars().all(|c| c.is_ascii_hexdigit()) {
            let target = f.get(4..).map(|r| r.join(" ")).unwrap_or_default();
            out.push(format!("{section}: {} {} {target}", f[0], f[2]));
        }
    }
    out
}

/// Links `-r` with both linkers and compares sections (in order, with
/// contents), symbols and relocations. Addresses are not compared:
/// relocatable outputs have none.
fn compare_relocatable(dir: &Path, args: &[&str]) {
    let Some(ld) = comparable_gnu_ld() else {
        println!("SKIPPED: no GNU ld 2.44 or newer to compare against");
        return;
    };
    for (linker, out) in [(ld, "gnu.o"), (qld_path(), "qld.o")] {
        let mut full = vec!["-r"];
        full.extend_from_slice(args);
        full.extend(["-o", out]);
        let result = run(dir, &linker, &full);
        assert!(
            result.status.success(),
            "{} {} failed: {}",
            linker.display(),
            full.join(" "),
            String::from_utf8_lossy(&result.stderr)
        );
    }
    let expected = relocatable_sections(&dir.join("gnu.o"));
    let actual = relocatable_sections(&dir.join("qld.o"));
    let names = |s: &[RelSection]| s.iter().map(|x| x.0.clone()).collect::<Vec<_>>();
    assert_eq!(
        names(&expected),
        names(&actual),
        "section order differs in {}",
        dir.display()
    );
    for (e, a) in expected.iter().zip(&actual) {
        assert_eq!(e, a, "section {} differs in {}", e.0, dir.display());
    }
    assert_eq!(
        relocatable_symbols(dir, "gnu.o"),
        relocatable_symbols(dir, "qld.o"),
        "symbols differ in {}",
        dir.display()
    );
    assert_eq!(
        relocatable_relocations(dir, "gnu.o"),
        relocatable_relocations(dir, "qld.o"),
        "relocations differ in {}",
        dir.display()
    );
}

/// A module-style link: sorted tables, `/DISCARD/`, sections with explicit
/// addresses and `ALIGN`, data commands, `. = ALIGN(8)` (which pads by the
/// addresses GNU ld gives sections while running the script), symbols
/// inside and outside output sections, `PROVIDE`, orphans (same-named ones
/// join the script's section), a COMDAT group (never matched by wildcards
/// in `-r`) and a link-order section.
#[test]
fn relocatable_link_with_a_script() {
    require_tools!();
    let dir = scratch("relocatable-script");
    assemble(
        &dir,
        "a",
        r#"
	.section .rodata,"a"
ra:	.byte 1
	.section .data.bar,"aw"
	.globl bar
bar:	.long 3
	.section .text.a,"ax"
	.globl fa
fa:	ret
	.section .init.text,"ax"
	.globl init_a
init_a:	nop
	ret
	.section .empty,"aw"
	.section alloc_tags,"aw"
	.quad 7
	.section .tbl,"a"
	.globl tbl_a
tbl_a:	.quad ra
	.section "__ksymtab+foo","a"
	.quad fa
	.section "__ksymtab+bar","a"
	.quad bar
	.section .discard.x,"a"
	.quad 1
	.section .text.inl,"axG",@progbits,inl,comdat
	.weak inl
inl:	ret
	.section __patchable_function_entries,"awo",@progbits,.text.a
	.quad fa
"#,
    );
    assemble(
        &dir,
        "b",
        r#"
	.section .rodata,"a"
rb:	.byte 2
	.section .text.b,"ax"
	.globl fb
fb:	nop
	ret
	.section alloc_tags,"aw"
	.quad 8
	.section .tbl,"a"
	.quad rb
	.section .text.inl,"axG",@progbits,inl,comdat
	.weak inl
inl:	ret
	.section .bss,"aw",@nobits
	.zero 16
"#,
    );
    fs::write(
        dir.join("module.ld"),
        r#"
abs_sym = 0x1234;
SECTIONS {
 /DISCARD/ : { *(.discard) *(.discard.*) }
 __ksymtab 0 : ALIGN(8) { *(SORT(__ksymtab+*)) }
 .text : { *(.text.b) *(.text .text.*) }
 .rodata : { a.o(.rodata) }
 .onlysyms : { only_start = .; only_end = .; }
 .dataish : { BYTE(1) SHORT(2) }
 .codetag.alloc_tags : { . = ALIGN(8); __start_alloc_tags = .; KEEP(*(alloc_tags)) __stop_alloc_tags = .; }
 .tbl : { tbl_start = .; *(.tbl) tbl_end = .; PROVIDE(tbl_unused = .); tbl_size = tbl_end - tbl_start; }
 .emptyout : { *(.nothing) }
 .aligned : { . = ALIGN(16); *(.data.bar) . = . + 4; after = .; }
}
alias_fa = fa;
alias_off = fa + 1;
"#,
    )
    .unwrap();
    compare_relocatable(&dir, &["-T", "module.ld", "a.o", "b.o"]);
}
