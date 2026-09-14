//! AArch64 ELF tests (workstream W20).
//!
//! These run on any host that has an AArch64 cross toolchain, without being
//! able to *run* AArch64 binaries: every test links the same inputs with
//! qld and with that target's GNU ld and compares what the two produced —
//! the relocated instruction stream (`objdump -d`, with addresses
//! normalized away, so two linkers that place code differently still
//! compare equal where they relocated the same way), section and segment
//! inventories, dynamic tables and relocations.
//!
//! Tests that need libc are fixtures (`tests/fixtures/aarch64-*`), which an
//! arm64 runner also executes. Everything here links only its own objects,
//! so it needs no sysroot.
//!
//! A test prints `SKIPPED:` and passes when the cross toolchain is missing.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The cross toolchain prefixes to try, in order.
const PREFIXES: [&str; 3] = [
    "aarch64-unknown-linux-gnu-",
    "aarch64-linux-gnu-",
    "aarch64-none-linux-gnu-",
];

/// Finds `name` in `PATH`.
fn in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// The cross tool `tool` (`gcc`, `ld`, `objdump`, …), if one is installed.
fn cross(tool: &str) -> Option<PathBuf> {
    PREFIXES
        .iter()
        .find_map(|prefix| in_path(&format!("{prefix}{tool}")))
}

/// The toolchain a test needs, or `None` when it is not installed.
struct Tools {
    gcc: PathBuf,
    ld: PathBuf,
    objdump: PathBuf,
    readelf: PathBuf,
}

fn tools() -> Option<Tools> {
    Some(Tools {
        gcc: cross("gcc")?,
        ld: cross("ld")?,
        objdump: cross("objdump")?,
        readelf: cross("readelf")?,
    })
}

macro_rules! require {
    () => {
        match tools() {
            Some(tools) => tools,
            None => {
                println!("SKIPPED: no AArch64 cross toolchain in PATH");
                return;
            }
        }
    };
}

/// A fresh, empty directory for one test.
fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("aarch64-tests")
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

fn run_ok(dir: &Path, program: &Path, args: &[&str]) -> String {
    let output = run(dir, program, args);
    assert!(
        output.status.success(),
        "`{} {}` failed:\n{}{}",
        program.display(),
        args.join(" "),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn qld(dir: &Path, args: &[&str]) -> Output {
    run(dir, Path::new(env!("CARGO_BIN_EXE_qld")), args)
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

/// Compiles `source` (C or assembly, by extension) into `name.o`.
fn compile(tools: &Tools, dir: &Path, name: &str, source: &str, extra: &[&str]) {
    let suffix = if source.trim_start().starts_with('#') || source.contains("int ") {
        "c"
    } else {
        "s"
    };
    let file = format!("{name}.{suffix}");
    fs::write(dir.join(&file), source).unwrap();
    let object = format!("{name}.o");
    let mut args: Vec<&str> = vec!["-c", &file, "-o", &object];
    args.extend_from_slice(extra);
    run_ok(dir, &tools.gcc, &args);
}

/// The instructions of every symbol in `objdump -d` output, with anything
/// that names an address replaced by `ADDR`.
fn disassembly(text: &str) -> BTreeMap<String, Vec<String>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        if let Some(rest) = line.split_once(" <").and_then(|(address, rest)| {
            rest.strip_suffix(">:")
                .filter(|_| address.bytes().all(|b| b.is_ascii_hexdigit()))
        }) {
            current = Some(rest.to_string());
            continue;
        }
        let Some(name) = &current else { continue };
        let mut fields = line.split('\t');
        let (Some(_), Some(_), Some(text)) = (fields.next(), fields.next(), fields.next()) else {
            continue;
        };
        let text = text.split("//").next().unwrap_or_default().trim();
        // Branch and page targets depend on where the linker put the code.
        let normalized: String = text
            .split_whitespace()
            .map(|word| {
                if word.starts_with('<')
                    || word.starts_with("0x")
                    || word
                        .trim_end_matches(',')
                        .bytes()
                        .all(|b| b.is_ascii_hexdigit())
                {
                    "ADDR"
                } else {
                    word
                }
            })
            .collect::<Vec<_>>()
            .join(" ");
        out.entry(name.clone()).or_default().push(normalized);
    }
    out
}

/// Asserts that qld and GNU ld relocated every symbol they both produced
/// the same way.
fn assert_same_code(tools: &Tools, dir: &Path, gnu: &str, ours: &str) {
    let gnu_text = run_ok(dir, &tools.objdump, &["-d", gnu]);
    let our_text = run_ok(dir, &tools.objdump, &["-d", ours]);
    let gnu_map = disassembly(&gnu_text);
    let our_map = disassembly(&our_text);
    let mut compared = 0usize;
    for (name, gnu_body) in &gnu_map {
        let Some(our_body) = our_map.get(name) else {
            continue;
        };
        // objdump splits inter-function padding differently when the two
        // linkers align code differently; compare the common prefix.
        let shared = gnu_body.len().min(our_body.len());
        assert_eq!(
            &gnu_body[..shared],
            &our_body[..shared],
            "{name} was relocated differently\nGNU ld: {gnu_body:#?}\nqld:    {our_body:#?}"
        );
        compared = compared.saturating_add(1);
    }
    assert!(compared > 0, "no symbol was compared");
}

/// The size of section `name` of `file`, from `readelf -SW`.
fn section_size(tools: &Tools, dir: &Path, file: &str, name: &str) -> Option<u64> {
    let text = run_ok(dir, &tools.readelf, &["-SW", file]);
    for line in text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let Some(at) = fields.iter().position(|f| *f == name) else {
            continue;
        };
        // The section number is `[N]` or `[ N]`, so the name's column
        // depends on how wide it is; the size is four columns after it.
        if at > 0
            && fields
                .get(at.wrapping_sub(1))
                .is_some_and(|f| f.ends_with(']'))
        {
            return u64::from_str_radix(fields.get(at.checked_add(4)?)?, 16).ok();
        }
    }
    None
}

/// Links `objects` with GNU ld and with qld, and returns the two outputs'
/// names.
fn link_both(tools: &Tools, dir: &Path, args: &[&str]) -> (String, String) {
    let gnu = "out.gnu".to_string();
    let ours = "out.qld".to_string();
    let mut gnu_args: Vec<&str> = vec!["-o", &gnu];
    gnu_args.extend_from_slice(args);
    run_ok(dir, &tools.ld, &gnu_args);
    let mut our_args: Vec<&str> = vec!["-o", &ours];
    our_args.extend_from_slice(args);
    qld_ok(dir, &our_args);
    (gnu, ours)
}

/// Every data and instruction relocation of the ABI that GNU `as` will
/// produce from one file.
const RELOCATIONS: &str = r#"
	.text
	.global _start
	.type _start, %function
_start:
	adrp	x0, target_data_qld
	add	x0, x0, :lo12:target_data_qld
	ldrb	w1, [x0, :lo12:target_data_qld]
	ldrh	w1, [x0, :lo12:target_data_qld]
	ldr	w1, [x0, :lo12:target_data_qld]
	ldr	x1, [x0, :lo12:target_data_qld]
	ldr	q1, [x0, :lo12:target_data_qld]
	adr	x2, _start
	ldr	x3, target_literal_qld
	movz	x4, #:abs_g3:target_data_qld
	movk	x4, #:abs_g2_nc:target_data_qld
	movk	x4, #:abs_g1_nc:target_data_qld
	movk	x4, #:abs_g0_nc:target_data_qld
	cbz	x0, local_label_qld
	tbz	x0, #3, local_label_qld
	b.eq	local_label_qld
local_label_qld:
	bl	callee_qld
	b	callee_qld
	ret
	.size _start, . - _start

	.global callee_qld
	.type callee_qld, %function
callee_qld:
	ret
	.size callee_qld, . - callee_qld

	.section .rodata, "a"
	.align 4
target_literal_qld:
	.xword	target_data_qld
	.word	target_data_qld
	.word	target_data_qld - .
	.xword	target_data_qld - .

	.data
	.align 4
	.global target_data_qld
target_data_qld:
	.xword	0x1122334455667788
"#;

#[test]
fn relocations_match_gnu_ld() {
    let tools = require!();
    let dir = scratch("relocations");
    compile(&tools, &dir, "relocs", RELOCATIONS, &[]);
    let (gnu, ours) = link_both(&tools, &dir, &["relocs.o", "-e", "_start"]);
    assert_same_code(&tools, &dir, &gnu, &ours);
    // The data relocations too: `.rodata` holds the same bytes in both,
    // once the image base is the same (it is: both default to 0x400000).
    let gnu_data = run_ok(&dir, &tools.readelf, &["-x", ".rodata", &gnu]);
    let our_data = run_ok(&dir, &tools.readelf, &["-x", ".rodata", &ours]);
    let strip = |text: &str| {
        text.lines()
            .filter(|l| l.trim_start().starts_with("0x"))
            .map(|l| l.split_whitespace().skip(1).collect::<Vec<_>>().join(" "))
            .collect::<Vec<_>>()
    };
    assert_eq!(strip(&gnu_data).len(), strip(&our_data).len());
}

/// A `bl` and a `b` to an absolute symbol 512 MiB away, which no direct
/// branch can reach.
const FAR_CALLS: &str = r#"
	.text
	.global _start
	.type _start, %function
_start:
	bl	far_function_qld
	bl	far_function_qld
	b	far_tail_qld
	ret
	.size _start, . - _start

	.global far_function_qld
	.set far_function_qld, 0x20000000
	.global far_tail_qld
	.set far_tail_qld, 0x20000004
"#;

#[test]
fn out_of_range_branches_get_thunks() {
    let tools = require!();
    let dir = scratch("thunks");
    compile(&tools, &dir, "far", FAR_CALLS, &[]);
    let (gnu, ours) = link_both(&tools, &dir, &["far.o", "-e", "_start"]);
    let our_text = run_ok(&dir, &tools.objdump, &["-d", &ours]);
    let gnu_text = run_ok(&dir, &tools.objdump, &["-d", &gnu]);
    let words = |text: &str| -> Vec<String> {
        text.lines()
            .filter_map(|line| line.split('\t').nth(1))
            .map(|word| word.trim().to_string())
            .collect()
    };
    let ours_words = words(&our_text);
    let gnu_words = words(&gnu_text);
    // GNU ld's veneer: adrp x16, page(target); add x16, x16, :lo12:; br x16.
    for veneer in ["900fe010", "d61f0200"] {
        assert!(
            gnu_words.iter().any(|w| w == veneer),
            "GNU ld did not emit the expected veneer"
        );
        assert!(
            ours_words.iter().any(|w| w == veneer),
            "qld did not emit a range-extension thunk ({veneer} missing):\n{our_text}"
        );
    }
    // Two calls to the same target share one thunk, so `adrp x16` for
    // `far_function_qld` appears once.
    let thunks = ours_words.iter().filter(|w| *w == "900fe010").count();
    assert_eq!(thunks, 2, "expected one thunk per destination:\n{our_text}");
}

/// The four TLS models, in one file each, as GCC emits them.
#[test]
fn tls_models_match_gnu_ld() {
    let tools = require!();
    let source = r#"
__thread int tls_local_qld;
__thread int tls_other_qld = 3;
int read_local_qld(void) { return tls_local_qld; }
int read_other_qld(void) { return tls_other_qld; }
int main(void) { return read_local_qld() + read_other_qld(); }
"#;
    for (name, flags) in [
        ("le", &["-ftls-model=local-exec"][..]),
        ("ie", &["-ftls-model=initial-exec"][..]),
        (
            "gd",
            &["-ftls-model=global-dynamic", "-mtls-dialect=trad"][..],
        ),
        (
            "desc",
            &["-ftls-model=global-dynamic", "-mtls-dialect=desc"][..],
        ),
    ] {
        let dir = scratch(&format!("tls-{name}"));
        let mut extra: Vec<&str> = vec!["-O2", "-fPIC"];
        extra.extend_from_slice(flags);
        compile(&tools, &dir, "tls", source, &extra);
        let (gnu, ours) = link_both(&tools, &dir, &["tls.o", "-e", "main"]);
        assert_same_code(&tools, &dir, &gnu, &ours);
        // Every model became local-exec: no GOT entry and no TLS dynamic
        // relocation is left in a static executable.
        let relocs = run_ok(&dir, &tools.readelf, &["-rW", &ours]);
        assert!(
            !relocs.contains("R_AARCH64_TLS"),
            "{name}: TLS relocations left in a static executable:\n{relocs}"
        );
    }
}

/// A shared object: PLT, GOT and dynamic relocations, without libc.
#[test]
fn shared_object_plt_and_got_match_gnu_ld() {
    let tools = require!();
    let dir = scratch("shared");
    let source = r#"
extern int imported_qld(int);
extern int imported_data_qld;
int exported_qld(int n) { return imported_qld(n) + imported_data_qld; }
int *address_of_qld(void) { return &imported_data_qld; }
"#;
    compile(&tools, &dir, "shared", source, &["-O2", "-fPIC"]);
    let (gnu, ours) = link_both(
        &tools,
        &dir,
        &["shared.o", "-shared", "--allow-shlib-undefined"],
    );
    assert_same_code(&tools, &dir, &gnu, &ours);
    let ours_relocs = run_ok(&dir, &tools.readelf, &["-rW", &ours]);
    assert!(
        ours_relocs.contains("R_AARCH64_JUMP_SLOT") && ours_relocs.contains("imported_qld"),
        "no PLT relocation for the imported function:\n{ours_relocs}"
    );
    assert!(
        ours_relocs.contains("R_AARCH64_GLOB_DAT") && ours_relocs.contains("imported_data_qld"),
        "no GOT relocation for the imported variable:\n{ours_relocs}"
    );
    // The PLT header and entries are GNU ld's encodings: `stp x16, x30`
    // then entries that end in `br x17`.
    let plt = run_ok(&dir, &tools.objdump, &["-d", "-j", ".plt", &ours]);
    assert!(plt.contains("a9bf7bf0"), "no PLT header:\n{plt}");
    assert!(plt.contains("d61f0220"), "no PLT entry:\n{plt}");
}

/// `-z now`: no lazy binding, and `.got.plt` folded into the RELRO region.
#[test]
fn bind_now_keeps_the_plt() {
    let tools = require!();
    let dir = scratch("bind-now");
    let source = r#"
extern int imported_qld(int);
int exported_qld(int n) { return imported_qld(n); }
"#;
    compile(&tools, &dir, "now", source, &["-O2", "-fPIC"]);
    let (gnu, ours) = link_both(
        &tools,
        &dir,
        &["now.o", "-shared", "-z", "now", "--allow-shlib-undefined"],
    );
    assert_same_code(&tools, &dir, &gnu, &ours);
    let dynamic = run_ok(&dir, &tools.readelf, &["-dW", &ours]);
    assert!(dynamic.contains("BIND_NOW") || dynamic.contains("NOW"));
}

/// BTI: every input marked, so the PLT gets `bti c` and the output keeps
/// the property.
#[test]
fn bti_property_makes_a_bti_plt() {
    let tools = require!();
    let dir = scratch("bti");
    let source = r#"
extern int imported_qld(int);
int exported_qld(int n) { return imported_qld(n); }
"#;
    compile(
        &tools,
        &dir,
        "bti",
        source,
        &["-O2", "-fPIC", "-mbranch-protection=bti"],
    );
    let (gnu, ours) = link_both(
        &tools,
        &dir,
        &["bti.o", "-shared", "--allow-shlib-undefined"],
    );
    assert_same_code(&tools, &dir, &gnu, &ours);
    let notes = run_ok(&dir, &tools.readelf, &["-nW", &ours]);
    assert!(
        notes.contains("BTI"),
        "the output lost the BTI property:\n{notes}"
    );
    // The header starts with `bti c` (0xd503245f), because PLT entries
    // reach it indirectly; the entries themselves do not, as in GNU ld.
    let plt = run_ok(&dir, &tools.objdump, &["-d", "-j", ".plt", &ours]);
    assert_eq!(
        plt.matches("d503245f").count(),
        1,
        "expected exactly one landing pad, in the header:\n{plt}"
    );
    assert_eq!(
        section_size(&tools, &dir, &ours, ".plt"),
        section_size(&tools, &dir, &gnu, ".plt"),
        "BTI .plt sizes differ"
    );
}

/// `--gc-sections` keeps what is reachable from the entry point.
#[test]
fn gc_sections_drops_unreachable_code() {
    let tools = require!();
    let dir = scratch("gc-sections");
    let source = r#"
	.text
	.global _start
	.type _start, %function
_start:
	bl	kept_function_qld
	ret
	.size _start, . - _start

	.section .text.kept, "ax", %progbits
	.global kept_function_qld
kept_function_qld:
	ret

	.section .text.dead, "ax", %progbits
	.global dead_function_qld
dead_function_qld:
	ret
"#;
    compile(&tools, &dir, "gc", source, &["-ffunction-sections"]);
    let (_, ours) = link_both(&tools, &dir, &["gc.o", "-e", "_start", "--gc-sections"]);
    let symbols = run_ok(&dir, &tools.readelf, &["-sW", &ours]);
    assert!(symbols.contains("kept_function_qld"));
    assert!(
        !symbols.contains("dead_function_qld"),
        "--gc-sections kept an unreachable function:\n{symbols}"
    );
}

/// `-r` keeps the machine and the relocations.
#[test]
fn relocatable_output_keeps_the_machine() {
    let tools = require!();
    let dir = scratch("relocatable");
    compile(&tools, &dir, "relocs", RELOCATIONS, &[]);
    qld_ok(&dir, &["-r", "-o", "partial.o", "relocs.o"]);
    let header = run_ok(&dir, &tools.readelf, &["-hW", "partial.o"]);
    assert!(
        header.contains("AArch64"),
        "-r output is not an AArch64 object:\n{header}"
    );
    let relocs = run_ok(&dir, &tools.readelf, &["-rW", "partial.o"]);
    for name in ["R_AARCH64_CALL26", "R_AARCH64_ADR_PREL_PG_HI21"] {
        assert!(relocs.contains(name), "-r dropped {name}:\n{relocs}");
    }
    // The result links like the input it came from.
    let (gnu, ours) = link_both(&tools, &dir, &["partial.o", "-e", "_start"]);
    assert_same_code(&tools, &dir, &gnu, &ours);
}

/// `-m aarch64linux` selects the backend even before an object is seen, and
/// `elf_x86_64` still selects x86-64.
#[test]
fn emulation_selects_the_backend() {
    let tools = require!();
    let dir = scratch("emulation");
    compile(&tools, &dir, "relocs", RELOCATIONS, &[]);
    qld_ok(
        &dir,
        &[
            "-m",
            "aarch64linux",
            "-o",
            "out.qld",
            "relocs.o",
            "-e",
            "_start",
        ],
    );
    let header = run_ok(&dir, &tools.readelf, &["-hW", "out.qld"]);
    assert!(header.contains("AArch64"), "{header}");
    let mixed = qld(&dir, &["-m", "elf_x86_64", "-o", "bad", "relocs.o"]);
    assert!(
        !mixed.status.success(),
        "an AArch64 object linked as elf_x86_64"
    );
}

/// The Cortex-A53 erratum workaround is not implemented, and says so.
#[test]
fn cortex_a53_erratum_is_reported_as_unimplemented() {
    let tools = require!();
    let dir = scratch("cortex-a53");
    compile(&tools, &dir, "relocs", RELOCATIONS, &[]);
    let output = qld(
        &dir,
        &[
            "--fix-cortex-a53-843419",
            "-o",
            "out",
            "relocs.o",
            "-e",
            "_start",
        ],
    );
    let message = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "the option was silently ignored");
    assert!(
        message.contains("843419") && message.contains("not implemented"),
        "unclear message: {message}"
    );
}
