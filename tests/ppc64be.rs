//! PowerPC64 big-endian, ELFv1 (`elf64ppc`) tests (workstream W48).
//!
//! These run on any host with a compiler that targets
//! `powerpc64-linux-gnu`, without being able to *run* the binaries: every
//! test links freestanding objects and checks the ELFv1 structure qld
//! produced, and, when `powerpc64-linux-gnu-ld` is installed, that GNU ld
//! resolves the same calls to the same functions.
//!
//! What is ELFv1-specific and checked here:
//!
//! - **Function descriptors.** A function symbol names a three-doubleword
//!   record in `.opd` (entry point, TOC pointer, environment pointer), so
//!   `e_entry` is `_start`'s descriptor and every descriptor's second word
//!   is `.TOC.` (`R_PPC64_TOC`).
//! - **Calls.** A `bl` must reach the *code*, so the linker follows the
//!   descriptor's `R_PPC64_ADDR64`; the tests decode the branch
//!   displacement out of the linked image and compare it with the
//!   descriptor's first word.
//! - **Byte order.** Instruction words are big-endian, and a relocation
//!   whose field is 16 bits names that halfword, two bytes into the
//!   instruction (GNU ld's `d_offset`): the `addis`/`addi` pair of a
//!   TOC-relative access is decoded and its `#ha`/`#lo` checked against
//!   the address the ABI prescribes.
//!
//! Programs that need a C library are fixtures (`tests/fixtures/ppc64-*`),
//! which CI runs under `qemu-ppc64`.
//!
//! Tools come from `QLD_PPC64BE_CC` (default `powerpc64-linux-gnu-gcc`,
//! then `clang --target=powerpc64-linux-gnu`) and `QLD_PPC64BE_LD`
//! (default `powerpc64-linux-gnu-ld`). A test prints `SKIPPED:` and passes
//! when a tool is missing, unless `QLD_REQUIRE_TOOLS` is set.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Finds `name` in `PATH` (or checks it when it is a path).
fn find(name: &str) -> Option<PathBuf> {
    if name.contains('/') {
        let path = PathBuf::from(name);
        return path.is_file().then_some(path);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// A tool from environment variable `var`, or the first of `names` in
/// `PATH`. An empty variable means "not installed".
fn tool(var: &str, names: &[&str]) -> Option<PathBuf> {
    match std::env::var(var) {
        Ok(value) if value.is_empty() => None,
        Ok(value) => find(&value),
        Err(_) => names.iter().find_map(|name| find(name)),
    }
}

struct Tools {
    cc: PathBuf,
    cc_args: Vec<String>,
    /// GNU ld for `powerpc64-linux-gnu`, when it is installed.
    reference: Option<PathBuf>,
}

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("ppc64be")
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

fn run_ok(dir: &Path, program: &Path, args: &[&str]) {
    let output = run(dir, program, args);
    assert!(
        output.status.success(),
        "{} {} failed:\n{}",
        program.display(),
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Whether `cc` with `args` compiles for PowerPC64 big-endian.
fn compiles(cc: &Path, args: &[String]) -> bool {
    let dir = scratch("probe");
    fs::write(dir.join("probe.c"), "int probe(void) { return 1; }\n").unwrap();
    let mut all: Vec<&str> = args.iter().map(String::as_str).collect();
    all.extend(["-c", "probe.c", "-o", "probe.o"]);
    if !run(&dir, cc, &all).status.success() {
        return false;
    }
    // ELF64, ELFDATA2MSB, EM_PPC64: the compiler really targets ELFv1.
    let head = fs::read(dir.join("probe.o")).unwrap_or_default();
    head.get(4..6) == Some(&[2, 2]) && head.get(18..20) == Some(&[0, 21])
}

fn discover() -> Result<Tools, String> {
    let mut candidates: Vec<(PathBuf, Vec<String>)> = Vec::new();
    if let Some(cc) = tool("QLD_PPC64BE_CC", &["powerpc64-linux-gnu-gcc"]) {
        let args = if cc.to_string_lossy().contains("clang") {
            vec!["--target=powerpc64-linux-gnu".to_string()]
        } else {
            Vec::new()
        };
        candidates.push((cc, args));
    }
    if std::env::var_os("QLD_PPC64BE_CC").is_none()
        && let Some(clang) = find("clang")
    {
        candidates.push((clang, vec!["--target=powerpc64-linux-gnu".to_string()]));
    }
    let (cc, cc_args) = candidates
        .into_iter()
        .find(|(cc, args)| compiles(cc, args))
        .ok_or("no compiler for powerpc64-linux-gnu")?;
    Ok(Tools {
        cc,
        cc_args,
        reference: tool("QLD_PPC64BE_LD", &["powerpc64-linux-gnu-ld"]),
    })
}

fn tools() -> Result<&'static Tools, &'static str> {
    static TOOLS: std::sync::OnceLock<Result<Tools, String>> = std::sync::OnceLock::new();
    TOOLS.get_or_init(discover).as_ref().map_err(String::as_str)
}

macro_rules! require {
    () => {
        match tools() {
            Ok(tools) => tools,
            Err(reason) => {
                let required = std::env::var_os("QLD_REQUIRE_TOOLS")
                    .is_some_and(|v| !v.is_empty() && v != "0");
                assert!(!required, "QLD_REQUIRE_TOOLS is set but: {reason}");
                println!("SKIPPED: {reason}");
                return;
            }
        }
    };
}

fn compile(tools: &Tools, dir: &Path, name: &str, source: &str, extra: &[&str]) {
    fs::write(dir.join(name), source).unwrap();
    let stem = name.rsplit_once('.').map_or(name, |(stem, _)| stem);
    let object = format!("{stem}.o");
    let mut args: Vec<&str> = tools.cc_args.iter().map(String::as_str).collect();
    args.extend(["-c", name, "-o", &object]);
    args.extend_from_slice(extra);
    run_ok(dir, &tools.cc, &args);
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

/// A minimal big-endian ELF64 reader for what these tests check.
mod elf {
    use std::collections::BTreeMap;
    use std::path::Path;

    pub struct Section {
        pub name: String,
        pub addr: u64,
        pub offset: u64,
        pub size: u64,
    }

    pub struct Symbol {
        pub value: u64,
        pub shndx: u16,
    }

    pub struct Elf {
        pub data: Vec<u8>,
        pub e_entry: u64,
        pub e_flags: u32,
        pub sections: Vec<Section>,
        pub symbols: BTreeMap<String, Symbol>,
    }

    fn u16be(data: &[u8], at: usize) -> u16 {
        u16::from_be_bytes(data[at..at + 2].try_into().unwrap())
    }

    fn u32be(data: &[u8], at: usize) -> u32 {
        u32::from_be_bytes(data[at..at + 4].try_into().unwrap())
    }

    fn u64be(data: &[u8], at: usize) -> u64 {
        u64::from_be_bytes(data[at..at + 8].try_into().unwrap())
    }

    fn name_at(strtab: &[u8], offset: usize) -> String {
        let rest = &strtab[offset.min(strtab.len())..];
        let end = rest.iter().position(|b| *b == 0).unwrap_or(rest.len());
        String::from_utf8_lossy(&rest[..end]).into_owned()
    }

    impl Elf {
        pub fn read(path: &Path) -> Self {
            let data = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            assert_eq!(&data[..4], b"\x7fELF", "{}: not an ELF", path.display());
            assert_eq!(data[4], 2, "{}: not ELF64", path.display());
            assert_eq!(data[5], 2, "{}: not big-endian", path.display());
            assert_eq!(u16be(&data, 18), 21, "{}: not EM_PPC64", path.display());
            let e_entry = u64be(&data, 24);
            let e_flags = u32be(&data, 48);
            let shoff = u64be(&data, 40) as usize;
            let shentsize = u16be(&data, 58) as usize;
            let shnum = u16be(&data, 60) as usize;
            let shstrndx = u16be(&data, 62) as usize;
            let raw: Vec<&[u8]> = (0..shnum)
                .map(|i| &data[shoff + i * shentsize..shoff + (i + 1) * shentsize])
                .collect();
            let shstr = {
                let s = raw[shstrndx];
                let off = u64be(s, 24) as usize;
                let size = u64be(s, 32) as usize;
                data[off..off + size].to_vec()
            };
            let sections: Vec<Section> = raw
                .iter()
                .map(|s| Section {
                    name: name_at(&shstr, u32be(s, 0) as usize),
                    addr: u64be(s, 16),
                    offset: u64be(s, 24),
                    size: u64be(s, 32),
                })
                .collect();
            let mut symbols = BTreeMap::new();
            for (index, s) in raw.iter().enumerate() {
                if u32be(s, 4) != 2 {
                    continue; // not SHT_SYMTAB
                }
                let off = u64be(s, 24) as usize;
                let size = u64be(s, 32) as usize;
                let link = u32be(s, 40) as usize;
                let strtab = {
                    let t = raw[link];
                    let off = u64be(t, 24) as usize;
                    let len = u64be(t, 32) as usize;
                    data[off..off + len].to_vec()
                };
                let _ = index;
                for entry in data[off..off + size].as_chunks::<24>().0 {
                    let name = name_at(&strtab, u32be(entry, 0) as usize);
                    if name.is_empty() {
                        continue;
                    }
                    symbols.insert(
                        name,
                        Symbol {
                            shndx: u16be(entry, 6),
                            value: u64be(entry, 8),
                        },
                    );
                }
            }
            Self {
                data,
                e_entry,
                e_flags,
                sections,
                symbols,
            }
        }

        pub fn section(&self, name: &str) -> &Section {
            self.sections
                .iter()
                .find(|s| s.name == name)
                .unwrap_or_else(|| panic!("no section {name}"))
        }

        /// The bytes at virtual address `addr`, when some section covers it.
        pub fn at(&self, addr: u64, len: usize) -> &[u8] {
            let section = self
                .sections
                .iter()
                .find(|s| s.addr != 0 && addr >= s.addr && addr + len as u64 <= s.addr + s.size)
                .unwrap_or_else(|| panic!("no section holds {addr:#x}"));
            let start = (section.offset + (addr - section.addr)) as usize;
            &self.data[start..start + len]
        }

        pub fn word(&self, addr: u64) -> u64 {
            u64be(self.at(addr, 8), 0)
        }

        pub fn insn(&self, addr: u64) -> u32 {
            u32be(self.at(addr, 4), 0)
        }

        /// The symbol `addr` is the start of, if any.
        pub fn symbol_at(&self, addr: u64) -> Option<&str> {
            self.symbols
                .iter()
                .find(|(_, s)| s.value == addr && s.shndx != 0)
                .map(|(name, _)| name.as_str())
        }
    }
}

/// `S + A` of the `bl` at `addr`: the sign-extended 24-bit displacement.
fn branch_target(image: &elf::Elf, addr: u64) -> u64 {
    let insn = image.insn(addr);
    assert_eq!(insn >> 26, 18, "{addr:#x}: not a b/bl: {insn:#010x}");
    let field = (insn & 0x03ff_fffc) as i32;
    let displacement = i64::from((field << 6) >> 6);
    addr.wrapping_add_signed(displacement)
}

/// The TOC pointer of the image: `.got` plus the ABI's 0x8000 bias, which
/// is also what `.TOC.` and every descriptor's second word hold.
fn toc_base(image: &elf::Elf) -> u64 {
    image.section(".got").addr + 0x8000
}

const FREESTANDING: &str = r#"
extern int add_qld(int a, int b);
extern int pick_qld(int index);
extern int table_qld[4];
int (*indirect_qld)(int, int) = add_qld;
static const char message_qld[] = "ok\n";

__attribute__((noinline)) int caller_qld(void) {
    return add_qld(pick_qld(2), pick_qld(1)) + (int)message_qld[0];
}

void _start(void) {
    caller_qld();
}
"#;

const LIBRARY: &str = r#"
int table_qld[4] = {1, 2, 3, 4};
int add_qld(int a, int b) { return a + b; }
int pick_qld(int index) { return table_qld[index & 3]; }
"#;

/// Links the two freestanding objects into `out` in `dir` with qld.
fn link_freestanding(tools: &Tools, dir: &Path, out: &str) -> elf::Elf {
    compile(tools, dir, "main.c", FREESTANDING, &["-O2"]);
    compile(tools, dir, "lib.c", LIBRARY, &["-O2"]);
    qld_ok(
        dir,
        &[
            "-m", "elf64ppc", "-o", out, "-e", "_start", "main.o", "lib.o",
        ],
    );
    elf::Elf::read(&dir.join(out))
}

/// The header says ELFv1, and `e_entry` is `_start`'s descriptor.
#[test]
fn the_entry_point_is_a_descriptor() {
    let tools = require!();
    let dir = scratch("entry");
    let image = link_freestanding(tools, &dir, "out");
    assert_eq!(image.e_flags & 3, 1, "e_flags must record ABI version 1");
    let opd = image.section(".opd");
    let start = image.symbols.get("_start").expect("no _start");
    assert_eq!(
        image.e_entry, start.value,
        "e_entry must be _start's symbol"
    );
    assert!(
        image.e_entry >= opd.addr && image.e_entry < opd.addr + opd.size,
        "e_entry {:#x} must be in .opd ({:#x}..{:#x})",
        image.e_entry,
        opd.addr,
        opd.addr + opd.size
    );
}

/// Every descriptor holds the function's code and the TOC pointer, and the
/// first `.got` word holds the TOC pointer too.
#[test]
fn descriptors_hold_the_code_and_the_toc() {
    let tools = require!();
    let dir = scratch("descriptors");
    let image = link_freestanding(tools, &dir, "out");
    let text = image.section(".text");
    let toc = toc_base(&image);
    assert_eq!(
        image.word(image.section(".got").addr),
        toc,
        "the first .got word is the TOC pointer"
    );
    if let Some(symbol) = image.symbols.get(".TOC.") {
        assert_eq!(symbol.value, toc, ".TOC. is .got + 0x8000");
    }
    for name in ["_start", "caller_qld", "add_qld", "pick_qld"] {
        let descriptor = image
            .symbols
            .get(name)
            .unwrap_or_else(|| panic!("no {name}"));
        let entry = image.word(descriptor.value);
        assert!(
            entry >= text.addr && entry < text.addr + text.size,
            "{name}: descriptor entry {entry:#x} must be in .text"
        );
        assert_eq!(
            image.word(descriptor.value + 8),
            toc,
            "{name}: the descriptor's second word is R_PPC64_TOC"
        );
    }
}

/// A `bl` reaches the code the descriptor points at, not the descriptor.
#[test]
fn calls_follow_the_descriptor() {
    let tools = require!();
    let dir = scratch("calls");
    let image = link_freestanding(tools, &dir, "out");
    let caller = image.word(image.symbols["caller_qld"].value);
    let mut called: BTreeMap<String, usize> = BTreeMap::new();
    // The callee entry points, by their descriptors.
    let entries: BTreeMap<u64, &str> = ["add_qld", "pick_qld", "caller_qld"]
        .into_iter()
        .map(|name| (image.word(image.symbols[name].value), name))
        .collect();
    for offset in (0..0x80).step_by(4) {
        let addr = caller + offset;
        let insn = image.insn(addr);
        if insn >> 26 != 18 || insn & 1 == 0 {
            continue; // not a `bl`
        }
        let target = branch_target(&image, addr);
        let name = entries.get(&target).unwrap_or_else(|| {
            panic!("bl at {addr:#x} goes to {target:#x}, which is no function's entry point")
        });
        *called.entry((*name).to_string()).or_default() += 1;
    }
    assert_eq!(
        called.get("pick_qld").copied(),
        Some(2),
        "two pick_qld calls"
    );
    assert_eq!(called.get("add_qld").copied(), Some(1), "one add_qld call");
}

/// A function pointer in data keeps the descriptor's address, unlike a
/// call.
#[test]
fn pointers_keep_the_descriptor() {
    let tools = require!();
    let dir = scratch("pointers");
    let image = link_freestanding(tools, &dir, "out");
    let pointer = image.symbols.get("indirect_qld").expect("no indirect_qld");
    assert_eq!(
        image.word(pointer.value),
        image.symbols["add_qld"].value,
        "a function pointer is the address of the descriptor"
    );
}

/// Assembly whose relocations name exactly the fields being checked: a
/// TOC-relative `addis`/`addi` pair, a DS-form `ld` through `.toc`, and
/// the halves of an absolute address.
const HALVES: &str = r#"
	.section ".opd","aw"
	.align 3
	.globl start_qld
	.type start_qld,@function
start_qld:
	.quad .Lcode_qld, .TOC.@tocbase, 0
	.text
.Lcode_qld:
	addis 3,2,target_qld@toc@ha
	addi  3,3,target_qld@toc@l
	addis 4,2,.Ltoc_qld@toc@ha
	ld    4,.Ltoc_qld@toc@l(4)
	lis   5,target_qld@highest
	ori   5,5,target_qld@higher
	blr
	.section ".toc","aw"
	.align 3
.Ltoc_qld:
	.quad target_qld
	.data
	.globl target_qld
	.align 3
target_qld:
	.quad 0x1234
"#;

/// A relocation whose field is 16 bits names that halfword, which is the
/// second half of a big-endian instruction: qld subtracts GNU ld's
/// `d_offset` before rewriting the instruction, so the `#ha` and `#lo` of
/// a TOC-relative pair land in the right place and compute the symbol's
/// address.
#[test]
fn sixteen_bit_fields_are_the_low_halfword() {
    let tools = require!();
    let dir = scratch("halves");
    compile(tools, &dir, "halves.s", HALVES, &[]);
    qld_ok(
        &dir,
        &["-m", "elf64ppc", "-o", "out", "-e", "start_qld", "halves.o"],
    );
    let image = elf::Elf::read(&dir.join("out"));
    let code = image.word(image.symbols["start_qld"].value);
    let toc = toc_base(&image);
    let target = image.symbols["target_qld"].value;

    let high = |addr: u64| i64::from(image.insn(addr) as u16 as i16) << 16;
    let low = |addr: u64| i64::from(image.insn(addr) as u16 as i16);
    // addis 3,2,target@toc@ha ; addi 3,3,target@toc@l
    assert_eq!(
        toc.wrapping_add_signed(high(code) + low(code + 4)),
        target,
        "the TOC-relative pair must compute target_qld ({target:#x})"
    );
    // addis 4,2,.Ltoc@toc@ha ; ld 4,.Ltoc@toc@l(4). The `.toc` entry holds
    // a non-preemptible address, so qld turns the load into the `addi` of
    // that address, as GNU ld and lld do; either way the value is
    // `target_qld`. The DS form keeps its low two bits, which the
    // relocation must not overwrite.
    let second = image.insn(code + 12);
    let value = toc.wrapping_add_signed(high(code + 8) + (low(code + 12) & !3));
    match second >> 26 {
        14 => assert_eq!(value, target, "the relaxed `addi` must compute target_qld"),
        58 => {
            assert_eq!(second & 3, 0, "`ld` keeps its DS opcode bits");
            assert_eq!(
                image.word(value),
                target,
                "the .toc entry the DS-form load reads must hold target_qld"
            );
        }
        opcode => panic!("unexpected instruction after the addis: opcode {opcode}"),
    }
    // lis 5,target@highest ; ori 5,5,target@higher
    let halves = ((image.insn(code + 16) as u64 & 0xffff) << 48)
        | ((image.insn(code + 20) as u64 & 0xffff) << 32);
    assert_eq!(
        halves,
        target & 0xffff_ffff_0000_0000,
        "the @highest and @higher halves must be the top of {target:#x}"
    );
}

/// GNU ld resolves the same calls to the same functions.
#[test]
fn calls_match_gnu_ld() {
    let tools = require!();
    let Some(reference) = &tools.reference else {
        let required =
            std::env::var_os("QLD_REQUIRE_TOOLS").is_some_and(|v| !v.is_empty() && v != "0");
        assert!(!required, "QLD_REQUIRE_TOOLS is set but GNU ld is missing");
        println!("SKIPPED: no powerpc64-linux-gnu-ld");
        return;
    };
    let dir = scratch("gnu");
    let ours = link_freestanding(tools, &dir, "out");
    run_ok(
        &dir,
        reference,
        &["-o", "out.gnu", "-e", "_start", "main.o", "lib.o"],
    );
    let theirs = elf::Elf::read(&dir.join("out.gnu"));
    assert_eq!(theirs.e_flags & 3, 1);
    let calls = |image: &elf::Elf| -> Vec<String> {
        let caller = image.word(image.symbols["caller_qld"].value);
        let entries: BTreeMap<u64, &str> = ["add_qld", "pick_qld", "caller_qld"]
            .into_iter()
            .map(|name| (image.word(image.symbols[name].value), name))
            .collect();
        (0..0x80)
            .step_by(4)
            .filter_map(|offset| {
                let addr = caller + offset;
                let insn = image.insn(addr);
                (insn >> 26 == 18 && insn & 1 == 1).then(|| {
                    let target = branch_target(image, addr);
                    entries
                        .get(&target)
                        .map_or_else(|| format!("{target:#x}"), |name| (*name).to_string())
                })
            })
            .collect()
    };
    assert_eq!(
        calls(&ours),
        calls(&theirs),
        "qld and GNU ld must call the same functions in the same order"
    );
    for name in ["add_qld", "pick_qld", "caller_qld", "_start"] {
        let ours_entry = ours.symbol_at(ours.word(ours.symbols[name].value));
        let theirs_entry = theirs.symbol_at(theirs.word(theirs.symbols[name].value));
        // Both linkers may or may not name the code address; when GNU ld
        // does, qld must agree.
        if let Some(expected) = theirs_entry {
            assert_eq!(
                ours_entry,
                Some(expected),
                "{name}: the descriptor must point at the same code"
            );
        }
    }
}

/// A dynamic ELFv1 output is refused rather than written wrong: the ABI's
/// PLT is an array of function descriptors qld does not build yet.
#[test]
fn dynamic_output_is_refused() {
    let tools = require!();
    let dir = scratch("dynamic");
    compile(tools, &dir, "lib.c", LIBRARY, &["-O2", "-fPIC"]);
    let output = qld(
        &dir,
        &["-m", "elf64ppc", "-shared", "-o", "lib.so", "lib.o"],
    );
    assert!(!output.status.success(), "a shared ELFv1 output must fail");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("ELFv1"),
        "the diagnostic must name the ABI, got:\n{stderr}"
    );
}

/// `-m elf64ppc` alone selects the backend, with no input to sniff.
#[test]
fn the_emulation_selects_the_backend() {
    let tools = require!();
    let dir = scratch("emulation");
    compile(tools, &dir, "lib.c", LIBRARY, &["-O2"]);
    qld_ok(
        &dir,
        &[
            "-m",
            "elf64ppc",
            "-o",
            "out",
            "-e",
            "add_qld",
            "--unresolved-symbols=ignore-all",
            "lib.o",
        ],
    );
    let image = elf::Elf::read(&dir.join("out"));
    assert_eq!(image.e_flags & 3, 1);
}

/// `Elf32Be` is instantiated in `elf::link`'s dispatch: a big-endian ELF32
/// input reaches the pipeline and is turned away by the architecture check
/// inside it, not by the dispatch. No architecture produces ELF32
/// big-endian output yet (32-bit Arm BE8 is the next one), so this is what
/// can be observed from outside.
#[test]
fn elf32_big_endian_reaches_the_pipeline() {
    let dir = scratch("elf32be");
    // A minimal ELF32 big-endian EM_ARM relocatable object: header only,
    // with no sections.
    let mut object = vec![0u8; 52];
    object[..4].copy_from_slice(b"\x7fELF");
    object[4] = 1; // ELFCLASS32
    object[5] = 2; // ELFDATA2MSB
    object[6] = 1; // EV_CURRENT
    object[16..18].copy_from_slice(&1u16.to_be_bytes()); // ET_REL
    object[18..20].copy_from_slice(&40u16.to_be_bytes()); // EM_ARM
    object[20..24].copy_from_slice(&1u32.to_be_bytes()); // EV_CURRENT
    object[40..42].copy_from_slice(&52u16.to_be_bytes()); // e_ehsize
    object[46..48].copy_from_slice(&40u16.to_be_bytes()); // e_shentsize
    fs::write(dir.join("be32.o"), &object).unwrap();
    let output = qld(&dir, &["-o", "out", "be32.o"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "the link must not succeed");
    assert!(
        !stderr.contains("needs a big-endian architecture"),
        "the dispatch must instantiate Elf32Be, got:\n{stderr}"
    );
    // `EM_ARM` is 40: read big-endian the header says Arm, read
    // little-endian it says machine 0x2800, which qld does not know. The
    // diagnostic naming Arm is what proves the ELF32 big-endian pipeline
    // parsed the input.
    assert!(
        stderr.contains("Arm"),
        "the ELF32 big-endian pipeline must decode the header, got:\n{stderr}"
    );
}
