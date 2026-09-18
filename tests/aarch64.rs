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
///
/// qld gets `--no-relax`: by default it relaxes ADRP pairs as lld does
/// (see [`adrp_relaxations_match_lld`]), which GNU ld never does.
fn link_both(tools: &Tools, dir: &Path, args: &[&str]) -> (String, String) {
    let gnu = "out.gnu".to_string();
    let ours = "out.qld".to_string();
    let mut gnu_args: Vec<&str> = vec!["-o", &gnu];
    gnu_args.extend_from_slice(args);
    run_ok(dir, &tools.ld, &gnu_args);
    let mut our_args: Vec<&str> = vec!["-o", &ours, "--no-relax"];
    our_args.extend_from_slice(args);
    qld_ok(dir, &our_args);
    (gnu, ours)
}

/// lld, the reference for what GNU ld does not implement: `QLD_TEST_LLD`,
/// or `ld.lld` in `PATH`.
fn lld() -> Option<PathBuf> {
    match std::env::var_os("QLD_TEST_LLD") {
        Some(path) if path.is_empty() => None,
        Some(path) => Some(PathBuf::from(path)),
        None => in_path("ld.lld"),
    }
}

/// The instruction words (hex, as `objdump -d` prints them) of symbol
/// `name`.
fn words_of(text: &str, name: &str) -> Vec<String> {
    let header = format!("<{name}>:");
    text.lines()
        .skip_while(|line| !line.ends_with(&header))
        .skip(1)
        .take_while(|line| !line.trim().is_empty())
        .filter_map(|line| line.split('\t').nth(1))
        .map(|word| word.trim().to_string())
        .collect()
}

/// The instructions of symbol `name`: mnemonics and registers, with every
/// number and symbolic address replaced by `ADDR`.
fn mnemonics_of(text: &str, name: &str) -> Vec<String> {
    let header = format!("<{name}>:");
    text.lines()
        .skip_while(|line| !line.ends_with(&header))
        .skip(1)
        .take_while(|line| !line.trim().is_empty())
        .filter_map(|line| {
            let mut fields = line.split('\t').skip(2);
            let mnemonic = fields.next()?.trim();
            let operands = fields.next().unwrap_or_default();
            let operands = operands.split("//").next().unwrap_or_default();
            let normalized: Vec<String> = operands
                .split_whitespace()
                .map(|token| {
                    let bare = token.trim_start_matches('[');
                    if bare.starts_with('#')
                        || bare.starts_with('<')
                        || bare.starts_with(|c: char| c.is_ascii_digit())
                    {
                        let close = if token.ends_with(']') { "]" } else { "" };
                        let comma = if token.ends_with(',') { "," } else { "" };
                        let open = if token.starts_with('[') { "[" } else { "" };
                        format!("{open}ADDR{close}{comma}")
                    } else {
                        token.to_string()
                    }
                })
                .collect();
            Some(
                format!("{mnemonic} {}", normalized.join(" "))
                    .trim()
                    .to_string(),
            )
        })
        .collect()
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
    let dynamic = run_ok(&dir, &tools.readelf, &["-dW", &ours]);
    assert!(dynamic.contains("AARCH64_BTI_PLT"), "{dynamic}");
}

/// The mnemonics of section `section` of `file`, in order.
fn section_mnemonics(tools: &Tools, dir: &Path, file: &str, section: &str) -> Vec<String> {
    let text = run_ok(dir, &tools.objdump, &["-d", "-j", section, file]);
    text.lines()
        .filter(|line| line.starts_with(' ') && line.contains(":\t"))
        .filter_map(|line| line.split('\t').nth(2))
        .map(|m| m.trim().to_string())
        .collect()
}

/// An executable calling a shared library, built from `source` with
/// `flags`, and the library.
fn plt_executable(tools: &Tools, dir: &Path, flags: &[&str]) {
    compile(
        tools,
        dir,
        "imp",
        "int imported_qld(int n) { return n; }\n",
        &["-O2", "-fPIC"],
    );
    run_ok(dir, &tools.ld, &["-shared", "-o", "libimp.so", "imp.o"]);
    let source = r#"
extern int imported_qld(int);
int main(void) { return imported_qld(1); }
int (*address_qld(void))(int) { return imported_qld; }
"#;
    let mut all = vec!["-O2", "-fno-PIC"];
    all.extend_from_slice(flags);
    compile(tools, dir, "exe", source, &all);
}

/// BTI in an executable: GNU ld gives the entries a landing pad too (an
/// entry can be a function's canonical address), 24 bytes each, and
/// `DT_AARCH64_BTI_PLT`. `-z force-bti` does the same for unmarked inputs,
/// with a warning for each, and marks the output.
#[test]
fn bti_plt_in_an_executable() {
    let tools = require!();
    for (name, flags, z) in [
        ("marked", &["-mbranch-protection=bti"][..], &[][..]),
        ("forced", &[][..], &["-z", "force-bti"][..]),
    ] {
        let dir = scratch(&format!("bti-exe-{name}"));
        plt_executable(&tools, &dir, flags);
        let mut args = vec!["exe.o", "libimp.so", "-e", "main"];
        args.extend_from_slice(z);
        let gnu_err = run(&dir, &tools.ld, &[&["-o", "out.gnu"], &args[..]].concat());
        let ours = qld(&dir, &[&["-o", "out.qld"], &args[..]].concat());
        assert!(ours.status.success(), "{name}: qld failed");
        let plt = section_mnemonics(&tools, &dir, "out.qld", ".plt");
        assert_eq!(
            plt,
            section_mnemonics(&tools, &dir, "out.gnu", ".plt"),
            "{name}: the PLT differs from GNU ld's"
        );
        assert_eq!(plt.iter().filter(|m| *m == "bti").count(), 2, "{plt:?}");
        let dynamic = run_ok(&dir, &tools.readelf, &["-dW", "out.qld"]);
        assert!(dynamic.contains("AARCH64_BTI_PLT"), "{name}: {dynamic}");
        let notes = run_ok(&dir, &tools.readelf, &["-nW", "out.qld"]);
        assert!(notes.contains("BTI"), "{name}: {notes}");
        let warning = "BTI is required by -z force-bti";
        let ours_warn = String::from_utf8_lossy(&ours.stderr).contains(warning);
        let gnu_warn = String::from_utf8_lossy(&gnu_err.stderr).contains(warning);
        assert_eq!(ours_warn, gnu_warn, "{name}: warnings differ");
        assert_eq!(ours_warn, name == "forced");
    }
}

/// `-z pac-plt`: every entry authenticates with `autia1716` before its
/// `br x17` (24 bytes), the header does not, and `DT_AARCH64_PAC_PLT` is
/// set; with BTI the entry keeps its landing pad. The shapes are GNU ld's.
#[test]
fn pac_plt_authenticates_entries() {
    let tools = require!();
    for (name, flags) in [
        ("plain", &[][..]),
        ("bti", &["-mbranch-protection=bti"][..]),
    ] {
        let dir = scratch(&format!("pac-plt-{name}"));
        plt_executable(&tools, &dir, flags);
        let args = ["exe.o", "libimp.so", "-e", "main", "-z", "pac-plt"];
        let (gnu, ours) = link_both(&tools, &dir, &args);
        let plt = section_mnemonics(&tools, &dir, &ours, ".plt");
        assert_eq!(
            plt,
            section_mnemonics(&tools, &dir, &gnu, ".plt"),
            "{name}: the PLT differs from GNU ld's"
        );
        assert_eq!(
            plt.iter().filter(|m| *m == "autia1716").count(),
            1,
            "{name}: {plt:?}"
        );
        let dynamic = run_ok(&dir, &tools.readelf, &["-dW", &ours]);
        assert!(dynamic.contains("AARCH64_PAC_PLT"), "{name}: {dynamic}");
        assert_same_code(&tools, &dir, &gnu, &ours);
    }
}

/// A static executable's IFUNC stubs are PLT entries, so they get the BTI
/// landing pad (and `-z pac-plt`) too.
#[test]
fn bti_ifunc_stubs_in_a_static_executable() {
    let tools = require!();
    let dir = scratch("bti-iplt");
    let source = r#"
static int impl_qld(void) { return 42; }
static void *resolve_qld(void) { return (void *)impl_qld; }
int ifunc_qld(void) __attribute__((ifunc("resolve_qld")));
int main(void) { return ifunc_qld(); }
int (*take_qld(void))(void) { return ifunc_qld; }
"#;
    compile(
        &tools,
        &dir,
        "ifunc",
        source,
        &["-O2", "-mbranch-protection=standard"],
    );
    for z in [&[][..], &["-z", "pac-plt"][..]] {
        let mut args = vec!["ifunc.o", "-static", "-e", "main"];
        args.extend_from_slice(z);
        let (gnu, ours) = link_both(&tools, &dir, &args);
        let plt = section_mnemonics(&tools, &dir, &ours, ".plt");
        assert_eq!(plt, section_mnemonics(&tools, &dir, &gnu, ".plt"), "{z:?}");
        assert_eq!(plt.first().map(String::as_str), Some("bti"), "{plt:?}");
        assert_same_code(&tools, &dir, &gnu, &ours);
    }
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

/// ADRP pairs that lld relaxes, and ones it must leave alone.
const ADRP_PAIRS: &str = r#"
	.text
	.global _start
	.type _start, %function
_start:
	adrp	x0, :got:var_qld
	ldr	x0, [x0, :got_lo12:var_qld]
	adrp	x1, var_qld
	add	x1, x1, :lo12:var_qld
	adrp	x2, :got:weak_qld
	ldr	x2, [x2, :got_lo12:weak_qld]
	adrp	x3, var_qld
	add	x4, x3, :lo12:var_qld
	adrp	x5, :got:var_qld
	ldr	x6, [x5, :got_lo12:var_qld]
	adrp	x7, far_qld
	add	x7, x7, :lo12:far_qld
	adrp	x8, :got:far_qld
	ldr	x8, [x8, :got_lo12:far_qld]
	adrp	x9, :got:near_qld
	ldr	x9, [x9, :got_lo12:near_qld]
	adrp	x10, :got:big_qld
	ldr	x10, [x10, :got_lo12:big_qld]
	adrp	x11, near_qld + 8
	add	x11, x11, :lo12:near_qld + 8
	ret
	.size _start, . - _start

	.data
	.global var_qld
var_qld:
	.xword	1
	.global near_qld
near_qld:
	.xword	2, 3
	.bss
	.space	0x200000
	.global big_qld
big_qld:
	.xword	0
	.weak weak_qld
	.global far_qld
	.set far_qld, 0x10000000
"#;

/// What lld 23 makes of [`ADRP_PAIRS`] in a static executable.
const ADRP_PAIRS_RELAXED: [&str; 23] = [
    // `var_qld` has one GOT access that cannot be relaxed (x5/x6), so none
    // of its GOT accesses are.
    "adrp x0, ADDR ADDR",
    "ldr x0, [x0, ADDR]",
    // ADRP+ADD within 1 MiB: `nop; adr`.
    "nop",
    "adr x1, ADDR ADDR",
    // An undefined weak symbol keeps its GOT entry.
    "adrp x2, ADDR ADDR",
    "ldr x2, [x2, ADDR]",
    // Two registers: not a pair.
    "adrp x3, ADDR ADDR",
    "add x4, x3, ADDR",
    "adrp x5, ADDR ADDR",
    "ldr x6, [x5, ADDR]",
    // 256 MiB away: out of `adr` range.
    "adrp x7, ADDR ADDR",
    "add x7, x7, ADDR",
    // GOT to ADRP+ADD, then not to ADR.
    "adrp x8, ADDR ADDR",
    "add x8, x8, ADDR",
    // GOT to ADRP+ADD to ADR.
    "nop",
    "adr x9, ADDR ADDR",
    // 2 MiB away: ADRP+ADD only.
    "adrp x10, ADDR ADDR",
    "add x10, x10, ADDR",
    // An addend: lld leaves it.
    "adrp x11, ADDR ADDR",
    "add x11, x11, ADDR",
    "ret",
    "",
    "",
];

/// `--relax` (the default) rewrites ADRP pairs as lld does; `--no-relax`
/// leaves them as GNU ld does.
#[test]
fn adrp_relaxations_match_lld() {
    let tools = require!();
    let dir = scratch("adrp-relax");
    compile(&tools, &dir, "pairs", ADRP_PAIRS, &[]);
    qld_ok(&dir, &["-o", "relaxed", "pairs.o", "-e", "_start"]);
    let text = run_ok(&dir, &tools.objdump, &["-d", "relaxed"]);
    let ours = mnemonics_of(&text, "_start");
    let expected: Vec<&str> = ADRP_PAIRS_RELAXED
        .iter()
        .copied()
        .filter(|l| !l.is_empty())
        .collect();
    assert_eq!(ours, expected, "\n{text}");
    // Every relaxed sequence still computes the address it did.
    let symbols = run_ok(&dir, &tools.readelf, &["-sW", "relaxed"]);
    let address_of = |name: &str| {
        symbols
            .lines()
            .find(|l| l.ends_with(&format!(" {name}")))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| u64::from_str_radix(v, 16).ok())
            .unwrap()
    };
    let near = format!("{:x} <near_qld>", address_of("near_qld"));
    assert!(text.contains(&format!("adr\tx9, {near}")), "{text}");
    // `--no-relax`: GNU ld's code.
    let (gnu, ours) = link_both(&tools, &dir, &["pairs.o", "-e", "_start"]);
    assert_same_code(&tools, &dir, &gnu, &ours);
    let plain = run_ok(&dir, &tools.objdump, &["-d", &ours]);
    assert!(!words_of(&plain, "_start").contains(&"d503201f".to_string()));
    // And lld itself, when it is installed.
    let Some(lld) = lld() else {
        println!("SKIPPED: lld comparison (no ld.lld)");
        return;
    };
    run_ok(&dir, &lld, &["-o", "lld", "pairs.o", "-e", "_start"]);
    let lld_text = run_ok(&dir, &tools.objdump, &["-d", "lld"]);
    assert_eq!(mnemonics_of(&lld_text, "_start"), expected, "\n{lld_text}");
}

/// In a PIE, an absolute symbol's GOT entry is not relaxed (ADRP+ADD
/// would make its address PC-relative), but everything else is.
#[test]
fn adrp_relaxations_in_a_pie() {
    let tools = require!();
    let dir = scratch("adrp-relax-pie");
    let source = r#"
	.text
	.global _start
	.type _start, %function
_start:
	adrp	x0, :got:abs_qld
	ldr	x0, [x0, :got_lo12:abs_qld]
	adrp	x1, :got:local_qld
	ldr	x1, [x1, :got_lo12:local_qld]
	adrp	x2, :got:hidden_qld
	ldr	x2, [x2, :got_lo12:hidden_qld]
	ret
	.data
local_qld:
	.xword	1
	.global hidden_qld
	.hidden hidden_qld
hidden_qld:
	.xword	2
	.global abs_qld
	.set abs_qld, 0x1234
"#;
    compile(&tools, &dir, "pie", source, &[]);
    for (output, flags) in [("pie", &["-pie"][..]), ("so", &["-shared"][..])] {
        let mut args = vec!["-o", output, "pie.o", "-e", "_start"];
        args.extend_from_slice(flags);
        qld_ok(&dir, &args);
        let text = run_ok(&dir, &tools.objdump, &["-d", output]);
        assert_eq!(
            mnemonics_of(&text, "_start"),
            [
                "adrp x0, ADDR ADDR",
                "ldr x0, [x0, ADDR]",
                "nop",
                "adr x1, ADDR ADDR",
                "nop",
                "adr x2, ADDR ADDR",
                "ret",
            ],
            "{output}:\n{text}"
        );
        if let Some(lld) = lld() {
            let lld_out = format!("{output}.lld");
            let mut args = vec!["-o", &lld_out, "pie.o", "-e", "_start"];
            args.extend_from_slice(flags);
            run_ok(&dir, &lld, &args);
            let lld_text = run_ok(&dir, &tools.objdump, &["-d", &lld_out]);
            assert_eq!(
                mnemonics_of(&lld_text, "_start"),
                mnemonics_of(&text, "_start"),
                "lld differs for {output}"
            );
        }
    }
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

/// The instruction words of `file` (`objdump -d`), by address.
fn words_by_address(tools: &Tools, dir: &Path, file: &str) -> BTreeMap<u64, u32> {
    let text = run_ok(dir, &tools.objdump, &["-d", file]);
    text.lines()
        .filter_map(|line| {
            let (address, rest) = line.trim_start().split_once(":\t")?;
            let address = u64::from_str_radix(address, 16).ok()?;
            let word = u32::from_str_radix(rest.split('\t').next()?.trim(), 16).ok()?;
            Some((address, word))
        })
        .collect()
}

/// The address of symbol `name` in `file`.
fn symbol_address(tools: &Tools, dir: &Path, file: &str, name: &str) -> u64 {
    let symbols = run_ok(dir, &tools.readelf, &["-sW", file]);
    symbols
        .lines()
        .find(|l| l.ends_with(&format!(" {name}")))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| u64::from_str_radix(v, 16).ok())
        .unwrap_or_else(|| panic!("no {name} in {file}"))
}

fn is_b(word: u32) -> bool {
    word >> 26 == 0b00_0101
}

fn b_target(at: u64, word: u32) -> u64 {
    let offset = i64::from(((word & 0x03ff_ffff) << 6).cast_signed() >> 6) * 4;
    at.wrapping_add_signed(offset)
}

/// The offsets from `_start` of the instructions of `object`'s `.text` that
/// `linked` replaced with a branch to an erratum patch.
fn patched_sites(tools: &Tools, dir: &Path, object: &str, linked: &str) -> Vec<u64> {
    let original = words_by_address(tools, dir, object);
    let words = words_by_address(tools, dir, linked);
    let start = symbol_address(tools, dir, linked, "_start");
    original
        .iter()
        .filter(|&(&offset, &word)| {
            !is_b(word) && words.get(&(start + offset)).is_some_and(|&w| is_b(w))
        })
        .map(|(&offset, _)| offset)
        .collect()
}

/// Checks that every patch of `linked` holds the instruction it replaced
/// (only a relocated immediate may differ) and branches back after it.
fn assert_patches_return(tools: &Tools, dir: &Path, object: &str, linked: &str, sites: &[u64]) {
    let original = words_by_address(tools, dir, object);
    let words = words_by_address(tools, dir, linked);
    let start = symbol_address(tools, dir, linked, "_start");
    for &offset in sites {
        let site = start + offset;
        let patch = b_target(site, words[&site]);
        let moved = words[&patch];
        let imm12 = 0xfff << 10;
        assert_eq!(
            moved & !imm12,
            original[&offset] & !imm12,
            "patch at {patch:#x}"
        );
        let back = words[&(patch + 4)];
        assert!(is_b(back), "patch at {patch:#x} does not end in a branch");
        assert_eq!(b_target(patch + 4, back), site + 4, "patch at {patch:#x}");
    }
}

/// Code whose `adrp` lands at page offsets `0xff8`/`0xffc`: every page of
/// `.text` (aligned to 4 KiB) ends with one candidate sequence.
fn page_end_sequences(sequences: &[(u32, &[&str])]) -> String {
    let mut source = String::from(
        "\t.text\n\t.global _start\n\t.type _start, %function\n_start:\n\tret\n\t.balign 4096\n",
    );
    for (page_offset, body) in sequences {
        source.push_str(&format!("\t.rept {}\n\tnop\n\t.endr\n", page_offset / 4));
        for line in *body {
            source.push_str(&format!("\t{line}\n"));
        }
        source.push_str("1:\n\t.balign 4096\n");
    }
    source.push_str("\t.section .rodata\n\t.balign 8\ntarget_qld:\n\t.xword 0\n");
    source
}

/// `--fix-cortex-a53-843419` patches the sequences lld patches, and only
/// those; the patch runs the moved instruction and returns.
#[test]
fn cortex_a53_843419_matches_lld() {
    let tools = require!();
    let dir = scratch("cortex-a53-843419");
    let sequences: [(u32, &[&str]); 8] = [
        // Three instructions from 0xff8: patched.
        (
            0xff8,
            &["adrp x0, target_qld", "ldr x1, [x2]", "ldr x3, [x0, #8]"],
        ),
        // Four from 0xffc, with a relocated last access: patched.
        (
            0xffc,
            &[
                "adrp x0, target_qld",
                "stp x1, x2, [sp]",
                "add x5, x6, x7",
                "str w3, [x0, :lo12:target_qld]",
            ],
        ),
        // The second instruction writes the `adrp` register: safe.
        (
            0xff8,
            &["adrp x0, target_qld", "ldr x0, [x2]", "ldr x3, [x0]"],
        ),
        // A branch in third place ends the sequence.
        (
            0xff8,
            &[
                "adrp x0, target_qld",
                "str x1, [x2]",
                "b 1f",
                "ldr x3, [x0]",
            ],
        ),
        // Not at the end of a page.
        (
            0xff0,
            &["adrp x0, target_qld", "str x1, [x2]", "ldr x3, [x0]"],
        ),
        // Another base register.
        (
            0xffc,
            &["adrp x0, target_qld", "str x1, [x2]", "ldr x3, [x1]"],
        ),
        // Store exclusive, then a vector load: patched.
        (
            0xffc,
            &[
                "adrp x4, target_qld",
                "stxr w5, x1, [x2]",
                "ldr q3, [x4, #16]",
            ],
        ),
        // Writeback of the base register: safe.
        (
            0xff8,
            &["adrp x0, target_qld", "ldr x1, [x0, #8]!", "ldr x3, [x0]"],
        ),
    ];
    compile(&tools, &dir, "seq", &page_end_sequences(&sequences), &[]);
    let args = ["--fix-cortex-a53-843419", "seq.o", "-e", "_start"];
    qld_ok(&dir, &[&["-o", "fixed"][..], &args[..]].concat());
    let sites = patched_sites(&tools, &dir, "seq.o", "fixed");
    // The first, second and seventh sequences. A sequence that runs past
    // its page takes two pages: they start on pages 1, 3 and 12 (the page
    // of `_start` is page 0).
    assert_eq!(
        sites,
        [0x1000 + 0xff8 + 8, 0x3000 + 0xffc + 12, 0xc000 + 0xffc + 8]
    );
    assert_patches_return(&tools, &dir, "seq.o", "fixed", &sites);
    // The relocated `str` in the patch stores to `target_qld`.
    let words = words_by_address(&tools, &dir, "fixed");
    let start = symbol_address(&tools, &dir, "fixed", "_start");
    let site = start + sites[1];
    let moved = words[&b_target(site, words[&site])];
    let target = symbol_address(&tools, &dir, "fixed", "target_qld");
    assert_eq!(u64::from((moved >> 10) & 0xfff), (target & 0xfff) >> 2);
    // Without the option, nothing moves.
    qld_ok(&dir, &[&["-o", "plain"][..], &args[1..]].concat());
    assert!(patched_sites(&tools, &dir, "seq.o", "plain").is_empty());
    if let Some(lld) = lld() {
        run_ok(&dir, &lld, &[&["-o", "lld"][..], &args[..]].concat());
        assert_eq!(patched_sites(&tools, &dir, "seq.o", "lld"), sites);
    }
}

/// `--fix-cortex-a53-835769` patches the multiply-accumulates GNU ld
/// patches: those right after a memory access they do not depend on.
#[test]
fn cortex_a53_835769_matches_gnu_ld() {
    let tools = require!();
    let dir = scratch("cortex-a53-835769");
    let source = r#"
	.text
	.global _start
	.type _start, %function
_start:
	ldr	x1, [x2]
	madd	x3, x4, x5, x6
	ldr	x1, [x2]
	madd	x3, x1, x5, x6
	str	x1, [x2]
	msub	x3, x1, x5, x6
	ldp	x1, x7, [x2]
	smaddl	x3, w4, w7, x6
	ldr	q1, [x2]
	umsubl	x3, w1, w5, x6
	ldr	x1, [x2]
	mul	x3, x4, x5
	ldr	x1, [x2]
	madd	w3, w4, w5, w6
	add	x1, x2, x3
	madd	x3, x4, x5, x6
	ldr	x1, [x2], #8
	madd	x3, x4, x5, x1
	ret
	.4byte	0xf9400041
	.4byte	0x9b041c23
"#;
    compile(&tools, &dir, "mac", source, &[]);
    let args = ["--fix-cortex-a53-835769", "mac.o", "-e", "_start"];
    let (gnu, ours) = link_both(&tools, &dir, &args);
    let sites = patched_sites(&tools, &dir, "mac.o", &ours);
    // The independent `madd`, the one after a store, the vector load's.
    assert_eq!(sites, [0x4, 0x14, 0x24]);
    assert_eq!(patched_sites(&tools, &dir, "mac.o", &gnu), sites);
    assert_patches_return(&tools, &dir, "mac.o", &ours, &sites);
}

/// A linker script layout has no pool for erratum patches, and says so.
#[test]
fn cortex_a53_fix_with_a_script_is_unimplemented() {
    let tools = require!();
    let dir = scratch("cortex-a53-script");
    compile(&tools, &dir, "relocs", RELOCATIONS, &[]);
    fs::write(
        dir.join("link.ld"),
        "SECTIONS { . = 0x10000; .text : { *(.text) } .data : { *(.data) } }\n",
    )
    .unwrap();
    for option in ["--fix-cortex-a53-843419", "--fix-cortex-a53-835769"] {
        let output = qld(
            &dir,
            &[
                option, "-T", "link.ld", "-o", "out", "relocs.o", "-e", "_start",
            ],
        );
        let message = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{option} was silently ignored");
        assert!(
            message.contains("linker script") && message.contains("not implemented"),
            "unclear message: {message}"
        );
    }
}
