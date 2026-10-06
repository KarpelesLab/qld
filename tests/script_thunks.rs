//! Range-extension thunks and Cortex-A53 erratum patches under linker
//! scripts (workstream W58).
//!
//! Each test links the same objects through the same `-T` script with qld
//! and with `ld.lld` (and the AArch64 GNU cross `ld` where it is installed),
//! with code placed farther apart than a branch reaches, and compares the
//! results symbol by symbol: every input symbol has the same address, and
//! every branch of every input function ends up, through whatever thunks
//! the linker inserted, at the same symbol. Thunks are followed by decoding
//! them (qld's, lld's and GNU ld's forms), so the linkers' different thunk
//! shapes and placements do not matter. Symbols that script expressions
//! compute from section sizes (`_etext = .`, `SIZEOF(.text)`, an `ALIGN`
//! the thunks push across a page) are checked against the output's own
//! section headers.
//!
//! The objects come from `clang` (AArch64, ARMv7-A in ARM and Thumb, and
//! PowerPC64 LE); nothing needs a sysroot, and nothing is run. Output files
//! stay small: the gaps between sections are address space, not file
//! contents, except in the tests about pools inside a large section.
//!
//! Tools: `clang`, `ld.lld`, from `PATH` or `QLD_CC`/`QLD_LLD`; a test
//! prints `SKIPPED:` and passes when one is missing.

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

fn tool(env: &str, name: &str) -> Option<PathBuf> {
    match std::env::var_os(env) {
        Some(path) if !path.is_empty() => Some(PathBuf::from(path)),
        _ => in_path(name),
    }
}

struct Tools {
    cc: PathBuf,
    lld: PathBuf,
}

fn tools() -> Option<Tools> {
    let tools = Tools {
        cc: tool("QLD_CC", "clang")?,
        lld: tool("QLD_LLD", "ld.lld")?,
    };
    let targets = Command::new(&tools.cc)
        .arg("--print-targets")
        .output()
        .ok()?;
    let targets = String::from_utf8_lossy(&targets.stdout);
    ["aarch64", "arm", "ppc64le"]
        .iter()
        .all(|t| targets.contains(t))
        .then_some(tools)
}

macro_rules! require {
    () => {
        match tools() {
            Some(tools) => tools,
            None => {
                println!(
                    "SKIPPED: no clang with the AArch64, Arm and PowerPC targets, or no ld.lld"
                );
                return;
            }
        }
    };
}

/// The AArch64 GNU cross linker, when installed.
fn gnu_aarch64_ld() -> Option<PathBuf> {
    [
        "aarch64-unknown-linux-gnu-ld",
        "aarch64-linux-gnu-ld",
        "aarch64-none-linux-gnu-ld",
    ]
    .iter()
    .find_map(|name| in_path(name))
}

fn scratch(name: &str) -> PathBuf {
    common::scratch::scratch_dir("script-thunks", name)
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
        "`{} {}` failed:\n{}{}",
        program.display(),
        args.join(" "),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn qld_ok(dir: &Path, args: &[&str]) {
    run_ok(dir, Path::new(env!("CARGO_BIN_EXE_qld")), args);
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Arch {
    AArch64,
    Arm,
    Ppc64,
}

impl Arch {
    fn target(self) -> &'static str {
        match self {
            Self::AArch64 => "--target=aarch64-linux-gnu",
            Self::Arm => "--target=armv7a-linux-gnueabihf",
            Self::Ppc64 => "--target=powerpc64le-linux-gnu",
        }
    }
}

/// Assembles `source` into `<name>.o`.
fn assemble(tools: &Tools, dir: &Path, arch: Arch, name: &str, source: &str) {
    let file = format!("{name}.s");
    fs::write(dir.join(&file), source).unwrap();
    let object = format!("{name}.o");
    run_ok(dir, &tools.cc, &[arch.target(), "-c", &file, "-o", &object]);
}

// --- A small ELF reader (little-endian ELF32 and ELF64). -----------------

struct Section {
    name: String,
    addr: u64,
    offset: u64,
    size: u64,
    sh_type: u32,
}

struct Symbol {
    value: u64,
    size: u64,
    global_func: bool,
}

struct Image {
    bytes: Vec<u8>,
    sections: Vec<Section>,
    symbols: BTreeMap<String, Symbol>,
}

fn le(bytes: &[u8], at: u64, width: usize) -> u64 {
    let at = usize::try_from(at).unwrap();
    bytes[at..at + width]
        .iter()
        .rev()
        .fold(0, |acc, &b| (acc << 8) | u64::from(b))
}

impl Image {
    fn load(dir: &Path, file: &str) -> Self {
        let bytes = fs::read(dir.join(file)).unwrap();
        assert_eq!(&bytes[..4], b"\x7fELF", "{file} is not ELF");
        let wide = bytes[4] == 2;
        let word = if wide { 8 } else { 4 };
        let (shoff, shentsize, shnum, shstrndx) = if wide {
            (
                le(&bytes, 0x28, 8),
                le(&bytes, 0x3a, 2),
                le(&bytes, 0x3c, 2),
                le(&bytes, 0x3e, 2),
            )
        } else {
            (
                le(&bytes, 0x20, 4),
                le(&bytes, 0x2e, 2),
                le(&bytes, 0x30, 2),
                le(&bytes, 0x32, 2),
            )
        };
        let mut raw = Vec::new();
        for index in 0..shnum {
            let at = shoff + index * shentsize;
            let field = |n: u64| -> u64 {
                // sh_name, sh_type: 4 bytes; then word-sized fields.
                match n {
                    0 => le(&bytes, at, 4),
                    1 => le(&bytes, at + 4, 4),
                    _ => le(&bytes, at + 8 + (n - 2) * word as u64, word),
                }
            };
            // name, type, flags, addr, offset, size, link
            raw.push((
                field(0),
                field(1) as u32,
                field(3),
                field(4),
                field(5),
                if wide {
                    le(&bytes, at + 0x28, 4)
                } else {
                    le(&bytes, at + 0x18, 4)
                },
            ));
        }
        let strtab = |section: usize, name: u64| -> String {
            let (_, _, _, offset, size, _) = raw[section];
            let start = usize::try_from(offset + name).unwrap();
            let end = usize::try_from(offset + size).unwrap();
            let text = &bytes[start..end];
            let len = text.iter().position(|&b| b == 0).unwrap_or(text.len());
            String::from_utf8_lossy(&text[..len]).into_owned()
        };
        let sections: Vec<Section> = raw
            .iter()
            .map(|&(name, sh_type, addr, offset, size, _)| Section {
                name: strtab(usize::try_from(shstrndx).unwrap(), name),
                addr,
                offset,
                size,
                sh_type,
            })
            .collect();
        let mut symbols = BTreeMap::new();
        if let Some(symtab) = sections.iter().position(|s| s.sh_type == 2) {
            let link = usize::try_from(raw[symtab].5).unwrap();
            let entsize = if wide { 24 } else { 16 };
            let count = sections[symtab].size / entsize;
            for index in 1..count {
                let at = sections[symtab].offset + index * entsize;
                let (name, value, size, info) = if wide {
                    (
                        le(&bytes, at, 4),
                        le(&bytes, at + 8, 8),
                        le(&bytes, at + 16, 8),
                        le(&bytes, at + 4, 1),
                    )
                } else {
                    (
                        le(&bytes, at, 4),
                        le(&bytes, at + 4, 4),
                        le(&bytes, at + 8, 4),
                        le(&bytes, at + 12, 1),
                    )
                };
                let name = strtab(link, name);
                if name.is_empty() || name.starts_with('$') {
                    continue;
                }
                let global_func = info == 0x12;
                if symbols.contains_key(&name) && !global_func {
                    continue;
                }
                symbols.insert(
                    name,
                    Symbol {
                        value,
                        size,
                        global_func,
                    },
                );
            }
        }
        Self {
            bytes,
            sections,
            symbols,
        }
    }

    fn section(&self, name: &str) -> &Section {
        self.sections
            .iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("no section {name}"))
    }

    fn value(&self, name: &str) -> u64 {
        self.symbols
            .get(name)
            .unwrap_or_else(|| panic!("no symbol {name}"))
            .value
    }

    /// `width` bytes at address `addr` of an allocated section with
    /// contents (or at offset `addr` of section `.text` in an object).
    fn read(&self, addr: u64, width: usize) -> u64 {
        let section = self
            .sections
            .iter()
            .find(|s| {
                s.sh_type != 8
                    && s.addr != 0
                    && (s.addr..s.addr + s.size).contains(&addr)
                    && addr + width as u64 <= s.addr + s.size
            })
            .unwrap_or_else(|| panic!("nothing at {addr:#x}"));
        le(&self.bytes, section.offset + addr - section.addr, width)
    }

    fn word(&self, addr: u64) -> u32 {
        self.read(addr, 4) as u32
    }

    fn half(&self, addr: u64) -> u16 {
        self.read(addr, 2) as u16
    }

    /// The global function at `addr` (with the Thumb bit on Arm).
    fn function_at(&self, addr: u64) -> Option<&str> {
        self.symbols
            .iter()
            .find(|(_, s)| s.global_func && s.value == addr)
            .map(|(name, _)| name.as_str())
    }
}

fn sext(value: u64, bits: u32) -> i64 {
    let shift = 64 - bits;
    ((value << shift) as i64) >> shift
}

// --- Branches and thunks. -------------------------------------------------

/// The destination of the branch at `at`, its length, and whether it is
/// one; on Arm `at` carries the Thumb bit, and so does the destination.
fn branch(image: &Image, arch: Arch, at: u64) -> (Option<u64>, u64) {
    match arch {
        Arch::AArch64 => {
            let w = image.word(at);
            // `b` and `bl`.
            let dest = (w >> 26 & 0x1f == 0b00101)
                .then(|| at.wrapping_add_signed(sext(u64::from(w & 0x03ff_ffff), 26) * 4));
            (dest, 4)
        }
        Arch::Ppc64 => {
            let w = image.word(at);
            // `b` and `bl` (I-form), relative.
            let dest = (w >> 26 == 18 && w & 2 == 0)
                .then(|| at.wrapping_add_signed(sext(u64::from(w & 0x03ff_fffc), 26)));
            (dest, 4)
        }
        Arch::Arm if at & 1 == 0 => {
            let w = image.word(at);
            if w >> 25 & 7 != 5 {
                return (None, 4);
            }
            let offset = sext(u64::from(w & 0x00ff_ffff), 24) * 4;
            let pc = at + 8;
            let dest = if w >> 28 == 0xf {
                // `blx` to Thumb.
                pc.wrapping_add_signed(offset) + u64::from(w >> 24 & 1) * 2 + 1
            } else {
                pc.wrapping_add_signed(offset)
            };
            (Some(dest), 4)
        }
        Arch::Arm => {
            let at = at & !1;
            let h1 = u32::from(image.half(at));
            if !matches!(h1 >> 11, 0x1d..=0x1f) {
                return (None, 2);
            }
            let h2 = u32::from(image.half(at + 2));
            let kind = h2 & 0xd000;
            if h1 & 0xf800 != 0xf000 || !matches!(kind, 0xd000 | 0xc000 | 0x9000) {
                return (None, 4);
            }
            let s = h1 >> 10 & 1;
            let i1 = !(h2 >> 13 ^ s) & 1;
            let i2 = !(h2 >> 11 ^ s) & 1;
            let imm = s << 24 | i1 << 23 | i2 << 22 | (h1 & 0x3ff) << 12 | (h2 & 0x7ff) << 1;
            let offset = sext(u64::from(imm), 25);
            let dest = if kind == 0xc000 {
                // `blx` to ARM.
                ((at + 4) & !3).wrapping_add_signed(offset)
            } else {
                (at + 4).wrapping_add_signed(offset) | 1
            };
            (Some(dest), 4)
        }
    }
}

/// `movw`/`movt` immediates, ARM (A1) or Thumb (T3/T1) encoding, of the
/// instruction at `at`; `top` selects `movt`.
fn arm_mov(image: &Image, at: u64, thumb: bool, top: bool) -> Option<u64> {
    if thumb {
        let h1 = image.half(at);
        let h2 = image.half(at + 2);
        let opcode = if top { 0xf2c0 } else { 0xf240 };
        (h1 & 0xfbf0 == opcode && h2 >> 8 & 0xf == 12).then(|| {
            u64::from(h1 & 0xf) << 12
                | u64::from(h1 >> 10 & 1) << 11
                | u64::from(h2 >> 12 & 7) << 8
                | u64::from(h2 & 0xff)
        })
    } else {
        let w = image.word(at);
        let opcode = if top { 0x0340_c000 } else { 0x0300_c000 };
        (w & 0x0ff0_f000 == opcode).then(|| u64::from(w >> 4 & 0xf000 | w & 0xfff))
    }
}

/// Where the thunk at `at` goes, for the thunk forms of qld, lld and GNU
/// ld; `None` when `at` holds no thunk.
fn thunk(image: &Image, arch: Arch, at: u64) -> Option<u64> {
    match arch {
        Arch::AArch64 => {
            let (w0, w1) = (image.word(at), image.word(at + 4));
            // adrp x16, page; add x16, x16, :lo12:; br x16
            if w0 & 0x9f00_001f == 0x9000_0010
                && w1 & 0xffc0_03ff == 0x9100_0210
                && image.word(at + 8) == 0xd61f_0200
            {
                let page = u64::from(w0 >> 29 & 3 | (w0 >> 5 & 0x7ffff) << 2);
                let base = (at & !0xfff).wrapping_add_signed(sext(page, 21) << 12);
                return Some(base + u64::from(w1 >> 10 & 0xfff));
            }
            // ldr x16, .+8; br x16; .xword target
            if w0 == 0x5800_0050 && w1 == 0xd61f_0200 {
                return Some(image.read(at + 8, 8));
            }
            None
        }
        Arch::Arm => {
            let thumb = at & 1 != 0;
            let at = at & !1;
            let low = arm_mov(image, at, thumb, false)?;
            let high = arm_mov(image, at + 4, thumb, true)?;
            let bx = if thumb {
                u32::from(image.half(at + 8)) == 0x4760
            } else {
                image.word(at + 8) == 0xe12f_ff1c
            };
            bx.then_some(high << 16 | low)
        }
        Arch::Ppc64 => {
            let mut at = at;
            // The TOC-saving form starts with `std r2, 24(r1)`.
            if image.word(at) == 0xf841_0018 {
                at += 4;
            }
            let words: Vec<u32> = (0..8).map(|i| image.word(at + i * 4)).collect();
            // qld: mflr r12; bcl 20,31,.+4; mflr r11; mtlr r12;
            // addis r12, r11, ha; addi r12, r12, lo; mtctr r12; bctr
            if words[..4] == [0x7d88_02a6, 0x429f_0005, 0x7d68_02a6, 0x7d88_03a6]
                && words[4] & 0xffff_0000 == 0x3d8b_0000
                && words[5] & 0xffff_0000 == 0x398c_0000
                && words[6..8] == [0x7d89_03a6, 0x4e80_0420]
            {
                let ha = sext(u64::from(words[4] & 0xffff), 16) << 16;
                let lo = sext(u64::from(words[5] & 0xffff), 16);
                return Some((at + 8).wrapping_add_signed(ha + lo));
            }
            // lld: addis r12, r2, ha; ld r12, lo(r12); mtctr r12; bctr,
            // with the destination in `.branch_lt`, addressed from the TOC
            // pointer, which the test program stores at `toc_word`.
            if words[0] & 0xffff_0000 == 0x3d82_0000
                && words[1] & 0xffff_0003 == 0xe98c_0000
                && words[2..4] == [0x7d89_03a6, 0x4e80_0420]
            {
                let toc = image.read(image.value("toc_word"), 8);
                let ha = sext(u64::from(words[0] & 0xffff), 16) << 16;
                let lo = sext(u64::from(words[1] & 0xffff), 16);
                return Some(image.read(toc.wrapping_add_signed(ha + lo), 8));
            }
            None
        }
    }
}

/// The function a branch to `dest` reaches, through thunks.
fn reach(image: &Image, arch: Arch, mut dest: u64) -> String {
    for _ in 0..4 {
        if let Some(name) = image.function_at(dest) {
            return name.to_string();
        }
        dest = match thunk(image, arch, dest) {
            Some(next) => next,
            // A thunk that is just a branch.
            None => branch(image, arch, dest)
                .0
                .unwrap_or_else(|| panic!("no thunk or function at {dest:#x}")),
        };
    }
    panic!("thunks do not lead anywhere from {dest:#x}");
}

/// Every branch of every global function of `image`, as `(function,
/// offset, function it reaches)`.
fn calls(image: &Image, arch: Arch) -> Vec<(String, u64, String)> {
    let mut out = Vec::new();
    for (name, symbol) in &image.symbols {
        if !symbol.global_func {
            continue;
        }
        let thumb = symbol.value & 1;
        let start = symbol.value & !1;
        assert!(symbol.size != 0, "{name} has no size");
        let mut offset = 0;
        while offset < symbol.size {
            let (dest, len) = branch(image, arch, (start + offset) | thumb);
            if let Some(dest) = dest {
                out.push((name.clone(), offset, reach(image, arch, dest)));
            }
            offset += len;
        }
    }
    out
}

/// Links `args` with qld, lld and (`gnu`) GNU ld into `<name>.{qld,lld,gnu}`
/// and checks that every input symbol has the same address in each and
/// that every branch reaches the same function. `sized` names the script
/// symbols whose values depend on thunk sizes: they are left out.
fn compare(
    tools: &Tools,
    dir: &Path,
    arch: Arch,
    name: &str,
    args: &[&str],
    gnu: Option<&Path>,
    sized: &[&str],
) -> Image {
    let ours = format!("{name}.qld");
    qld_ok(dir, &[args, &["-o", &ours]].concat());
    let mut oracles = vec![("lld", tools.lld.clone())];
    if let Some(gnu) = gnu {
        oracles.push(("gnu", gnu.to_path_buf()));
    }
    let image = Image::load(dir, &ours);
    let our_calls = calls(&image, arch);
    assert!(!our_calls.is_empty(), "{name}: no branches found in {ours}");
    for (label, linker) in oracles {
        let file = format!("{name}.{label}");
        run_ok(dir, &linker, &[args, &["-o", &file]].concat());
        let oracle = Image::load(dir, &file);
        assert_eq!(
            calls(&oracle, arch),
            our_calls,
            "{name}: branches differ from {label}"
        );
        for (symbol, value) in &oracle.symbols {
            let thunk_symbol = !value.global_func && !image.symbols.contains_key(symbol);
            if thunk_symbol || sized.contains(&symbol.as_str()) {
                continue;
            }
            assert_eq!(
                image.symbols.get(symbol).map(|s| s.value),
                Some(value.value),
                "{name}: {symbol} differs from {label}"
            );
        }
    }
    // The same output with one thread and with two.
    let single = format!("{name}.qld1");
    qld_ok(dir, &[args, &["--threads=1", "-o", &single]].concat());
    let two = format!("{name}.qld2");
    qld_ok(dir, &[args, &["--threads=2", "-o", &two]].concat());
    assert!(
        fs::read(dir.join(&single)).unwrap() == fs::read(dir.join(&two)).unwrap(),
        "{name}: output depends on the thread count"
    );
    image
}

/// `_etext`, set after the input sections of `.text`, is the end of
/// `.text`, thunks included, and so is `ADDR(.text) + SIZEOF(.text)`.
fn assert_etext(image: &Image) {
    let text = image.section(".text");
    assert_eq!(image.value("_etext"), text.addr + text.size);
    if image.symbols.contains_key("text_end") {
        assert_eq!(image.value("text_end"), text.addr + text.size);
    }
}

// --- AArch64. -------------------------------------------------------------

const AARCH64_S: &str = r#"
	.text
	.globl _start
	.type _start, %function
_start:
	bl far1
	bl far2
	b far1
	bl near
	ret
	.size _start, .-_start
	.globl near
	.type near, %function
near:
	bl far2
	ret
	.size near, .-near
	// .text ends 12 bytes before a page boundary: any thunk pushes the
	// ALIGN after it to the next page.
	.space 0xfd8
	.section .text.far,"ax",%progbits
	.globl far1
	.type far1, %function
far1:
	bl _start
	ret
	.size far1, .-far1
	.globl far2
	.type far2, %function
far2:
	b near
	.size far2, .-far2
	.data
	.globl datum
datum:
	.xword far1
"#;

/// `.far` is 200 MiB past `.text`, out of reach of `b`/`bl` both ways.
const AARCH64_LD: &str = "SECTIONS {
  . = 0x10000;
  .text : { *(.text) _etext = .; }
  text_end = ADDR(.text) + SIZEOF(.text);
  . = ALIGN(0x1000);
  .data : { *(.data) }
  data_addr = ADDR(.data);
  . = 0x10000 + 200M;
  .far : { *(.text.far) }
}
";

#[test]
fn aarch64_calls_across_a_script_gap_match_lld_and_gnu_ld() {
    let tools = require!();
    let dir = scratch("aarch64");
    assemble(&tools, &dir, Arch::AArch64, "calls", AARCH64_S);
    fs::write(dir.join("link.ld"), AARCH64_LD).unwrap();
    let gnu = gnu_aarch64_ld();
    let image = compare(
        &tools,
        &dir,
        Arch::AArch64,
        "calls",
        &["-T", "link.ld", "calls.o", "-e", "_start"],
        gnu.as_deref(),
        &["_etext", "text_end"],
    );
    assert_etext(&image);
    // The thunks moved `.data` to the next page (so did lld's and GNU
    // ld's, which the comparison of `data_addr` checked).
    assert_eq!(image.value("data_addr"), 0x12000);
    assert_eq!(image.read(image.value("datum"), 8), image.value("far1"));
    assert_eq!(image.section(".far").addr, 0x10000 + (200 << 20));
}

// --- 32-bit Arm. ----------------------------------------------------------

const ARM_S: &str = r#"
	.syntax unified
	.text
	.arm
	.globl _start
	.type _start, %function
_start:
	bl far_arm
	bl far_thumb
	b far_arm
	bl thumb_near
	bx lr
	.size _start, .-_start
	.thumb
	.globl thumb_near
	.thumb_func
	.type thumb_near, %function
thumb_near:
	bl far_thumb
	bl far_arm
	b.w far_thumb
	bx lr
	.size thumb_near, .-thumb_near
	.section .text.far,"ax",%progbits
	.arm
	.globl far_arm
	.type far_arm, %function
far_arm:
	bl _start
	bl thumb_near
	bx lr
	.size far_arm, .-far_arm
	.thumb
	.globl far_thumb
	.thumb_func
	.type far_thumb, %function
far_thumb:
	bl thumb_near
	bl _start
	bx lr
	.size far_thumb, .-far_thumb
	.data
	.globl datum
datum:
	.word far_thumb
"#;

/// Two memory regions 64 MiB apart, beyond an ARM `bl` (±32 MiB) and a
/// Thumb one (±16 MiB). `.data` between them keeps them in separate
/// segments, so the file stays small.
const ARM_LD: &str = "MEMORY {
  near (rx) : ORIGIN = 0x10000, LENGTH = 1M
  ram (rw) : ORIGIN = 0x200000, LENGTH = 1M
  far (rx) : ORIGIN = 0x4010000, LENGTH = 1M
}
SECTIONS {
  .text : { *(.text) _etext = .; } > near
  text_end = ADDR(.text) + SIZEOF(.text);
  .data : { *(.data) } > ram
  .far : { *(.text.far) } > far
}
";

#[test]
fn arm_and_thumb_calls_across_a_script_gap_match_lld() {
    let tools = require!();
    let dir = scratch("arm");
    assemble(&tools, &dir, Arch::Arm, "calls", ARM_S);
    fs::write(dir.join("link.ld"), ARM_LD).unwrap();
    let image = compare(
        &tools,
        &dir,
        Arch::Arm,
        "calls",
        &["-T", "link.ld", "calls.o", "-e", "_start"],
        None,
        &["_etext", "text_end"],
    );
    assert_etext(&image);
    // The thunks of both regions are marked as code.
    let far = image.section(".far");
    assert!(far.size > 0x18, "no thunks in .far: {:#x}", far.size);
}

/// A Thumb `bl` reaches ±16 MiB, so an 18 MiB output section placed by a
/// script needs a pool inside it, not only one at its end.
#[test]
fn thumb_pools_are_spread_through_a_large_scripted_section() {
    let tools = require!();
    let dir = scratch("arm-pools");
    let mut source = String::from("\t.syntax unified\n");
    for index in 0..18 {
        source.push_str(&format!(
            "\t.section .text.f{index:02},\"ax\",%progbits\n\t.thumb\n\
             \t.globl f{index:02}\n\t.thumb_func\n\t.type f{index:02}, %function\n\
             f{index:02}:\n\tbl far_thumb\n\tbx lr\n\t.size f{index:02}, .-f{index:02}\n\
             \t.space 0x100000\n"
        ));
    }
    source.push_str(
        "\t.section .text.far,\"ax\",%progbits\n\t.thumb\n\t.globl far_thumb\n\
         \t.thumb_func\n\t.type far_thumb, %function\nfar_thumb:\n\tbx lr\n\
         \t.size far_thumb, .-far_thumb\n\t.data\n\t.word 0\n",
    );
    assemble(&tools, &dir, Arch::Arm, "big", &source);
    fs::write(
        dir.join("link.ld"),
        "SECTIONS {\n  . = 0x10000;\n  .text : { *(.text.f0*) *(.text.f1*) _etext = .; }\n  \
         . = ALIGN(0x10000);\n  .data : { *(.data) }\n  \
         . = 0x8000000;\n  .far : { *(.text.far) }\n}\n",
    )
    .unwrap();
    let image = compare(
        &tools,
        &dir,
        Arch::Arm,
        "big",
        &["-T", "link.ld", "big.o", "-e", "f00"],
        None,
        // lld spaces its pools differently: the callers after the first
        // pool are elsewhere.
        &(1..18)
            .map(|i| format!("f{i:02}"))
            .chain(["_etext".to_string()])
            .collect::<Vec<_>>()
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
    );
    assert_etext(&image);
    // Every caller's `bl` lands within reach, and the two ends use
    // different pools.
    let first = |name: &str| {
        let at = image.value(name) & !1;
        let dest = branch(&image, Arch::Arm, at | 1).0.unwrap() & !1;
        assert!(at.abs_diff(dest) < 16 << 20, "{name} branches out of reach");
        dest
    };
    assert_ne!(first("f00"), first("f17"));
}

// --- PowerPC64 LE. --------------------------------------------------------

const PPC64_S: &str = r#"
	.text
	.globl _start
	.type _start, @function
_start:
	bl far1
	nop
	bl far2
	nop
	b far1
	.size _start, .-_start
	.globl near
	.type near, @function
near:
	bl far2
	nop
	blr
	.size near, .-near
	.section .text.far,"ax",@progbits
	.globl far1
	.type far1, @function
far1:
	bl _start
	nop
	blr
	.size far1, .-far1
	.globl far2
	.type far2, @function
far2:
	b near
	.size far2, .-far2
	.data
	.globl toc_word
	.p2align 3
toc_word:
	.quad .TOC.
"#;

/// `.far` is 64 MiB past `.text`, beyond a `bl` (±32 MiB). `.data` between
/// them keeps the two in separate segments, so the file stays small.
const PPC64_LD: &str = "SECTIONS {
  . = 0x10000000;
  .text : { *(.text) _etext = .; }
  text_end = ADDR(.text) + SIZEOF(.text);
  . = ALIGN(0x10000);
  .data : { *(.data) }
  . = 0x10000000 + 64M;
  .far : { *(.text.far) }
}
";

#[test]
fn ppc64le_calls_across_a_script_gap_match_lld() {
    let tools = require!();
    let dir = scratch("ppc64le");
    assemble(&tools, &dir, Arch::Ppc64, "calls", PPC64_S);
    fs::write(dir.join("link.ld"), PPC64_LD).unwrap();
    let image = compare(
        &tools,
        &dir,
        Arch::Ppc64,
        "calls",
        &["-T", "link.ld", "calls.o", "-e", "_start"],
        None,
        // `.got`, which the TOC pointer follows, is an orphan that each
        // linker places in its own way.
        &["_etext", "text_end", ".TOC."],
    );
    assert_etext(&image);
}

// --- Cortex-A53 erratum patches. ------------------------------------------

/// Words of `.text` of the object `file`, by offset.
fn object_text(dir: &Path, file: &str) -> BTreeMap<u64, u32> {
    let image = Image::load(dir, file);
    let text = image.section(".text");
    (0..text.size / 4)
        .map(|i| (i * 4, le(&image.bytes, text.offset + i * 4, 4) as u32))
        .collect()
}

fn is_b(word: u32) -> bool {
    word >> 26 == 0b00_0101
}

fn b_target(at: u64, word: u32) -> u64 {
    at.wrapping_add_signed(sext(u64::from(word & 0x03ff_ffff), 26) * 4)
}

/// The offsets from `_start` of the instructions of the object's `.text`
/// that `linked` replaced with a branch to an erratum patch, with the
/// patch addresses. Each patch is checked: it holds the instruction it
/// replaced (only a relocated immediate may differ) and branches back
/// after it.
fn patches(dir: &Path, object: &str, linked: &str) -> Vec<(u64, u64)> {
    let original = object_text(dir, object);
    let image = Image::load(dir, linked);
    let start = image.value("_start");
    let mut out = Vec::new();
    for (&offset, &word) in &original {
        let site = start + offset;
        let linked_word = image.word(site);
        if is_b(word) || !is_b(linked_word) {
            continue;
        }
        let patch = b_target(site, linked_word);
        let imm12 = 0xfff << 10;
        assert_eq!(
            image.word(patch) & !imm12,
            word & !imm12,
            "{linked}: patch at {patch:#x}"
        );
        let back = image.word(patch + 4);
        assert!(is_b(back), "{linked}: patch at {patch:#x} does not return");
        assert_eq!(b_target(patch + 4, back), site + 4, "{linked}: {patch:#x}");
        out.push((offset, patch));
    }
    out
}

fn sites(patches: &[(u64, u64)]) -> Vec<u64> {
    patches.iter().map(|&(site, _)| site).collect()
}

/// Code whose `adrp` lands at page offsets `0xff8`/`0xffc` (`.text` is
/// page-aligned), and a call to a function 200 MiB away, so that the pool
/// holds a thunk before the patches.
fn page_end_sequences(sequences: &[(u32, &[&str])]) -> String {
    let mut source = String::from(
        "\t.text\n\t.global _start\n\t.type _start, %function\n_start:\n\tbl far\n\tret\n\
         \t.balign 4096\n",
    );
    for (page_offset, body) in sequences {
        source.push_str(&format!("\t.rept {}\n\tnop\n\t.endr\n", page_offset / 4));
        for line in *body {
            source.push_str(&format!("\t{line}\n"));
        }
        source.push_str("1:\n\t.balign 4096\n");
    }
    source.push_str(
        "\t.section .rodata\n\t.balign 8\ntarget_qld:\n\t.xword 0\n\
         \t.section .text.far,\"ax\",%progbits\n\t.globl far\n\t.type far, %function\n\
         far:\n\tbl _start\n\tret\n",
    );
    source
}

const ERRATUM_LD: &str = "SECTIONS {
  . = 0x10000;
  .text : { *(.text) }
  .rodata : { *(.rodata) }
  . = 0x10000 + 200M;
  .far : { *(.text.far) }
}
";

/// `--fix-cortex-a53-843419` under a script patches what lld patches, in
/// the pool after the range-extension thunk.
#[test]
fn cortex_a53_843419_under_a_script_matches_lld() {
    let tools = require!();
    let dir = scratch("843419");
    let sequences: [(u32, &[&str]); 4] = [
        // Patched.
        (
            0xff8,
            &["adrp x0, target_qld", "ldr x1, [x2]", "ldr x3, [x0, #8]"],
        ),
        // Patched, with a relocated last access.
        (
            0xffc,
            &[
                "adrp x0, target_qld",
                "stp x1, x2, [sp]",
                "add x5, x6, x7",
                "str w3, [x0, :lo12:target_qld]",
            ],
        ),
        // Safe: the second instruction writes the `adrp` register.
        (
            0xff8,
            &["adrp x0, target_qld", "ldr x0, [x2]", "ldr x3, [x0]"],
        ),
        // Patched.
        (
            0xffc,
            &[
                "adrp x4, target_qld",
                "stxr w5, x1, [x2]",
                "ldr q3, [x4, #16]",
            ],
        ),
    ];
    assemble(
        &tools,
        &dir,
        Arch::AArch64,
        "seq",
        &page_end_sequences(&sequences),
    );
    fs::write(dir.join("link.ld"), ERRATUM_LD).unwrap();
    let args = [
        "--fix-cortex-a53-843419",
        "-T",
        "link.ld",
        "seq.o",
        "-e",
        "_start",
    ];
    qld_ok(&dir, &[&args[..], &["-o", "fixed"]].concat());
    let ours = patches(&dir, "seq.o", "fixed");
    assert_eq!(
        sites(&ours),
        [0x1000 + 0xff8 + 8, 0x3000 + 0xffc + 12, 0x7000 + 0xffc + 8]
    );
    // The patches follow the thunk to `far` at the end of `.text`.
    let image = Image::load(&dir, "fixed");
    let text = image.section(".text");
    let thunk = branch(&image, Arch::AArch64, image.value("_start"))
        .0
        .unwrap();
    assert_eq!(reach(&image, Arch::AArch64, thunk), "far");
    for &(_, patch) in &ours {
        assert!(patch > thunk && patch < text.addr + text.size);
    }
    run_ok(&dir, &tools.lld, &[&args[..], &["-o", "lld"]].concat());
    assert_eq!(sites(&patches(&dir, "seq.o", "lld")), sites(&ours));
    // Without the option, nothing is patched.
    qld_ok(&dir, &[&args[1..], &["-o", "plain"]].concat());
    assert!(patches(&dir, "seq.o", "plain").is_empty());
}

const MAC_S: &str = r#"
	.text
	.global _start
	.type _start, %function
_start:
	bl far
	ldr	x1, [x2]
	madd	x3, x4, x5, x6
	ldr	x1, [x2]
	madd	x3, x1, x5, x6
	str	x1, [x2]
	msub	x3, x1, x5, x6
	ldr	q1, [x2]
	umsubl	x3, w1, w5, x6
	ret
	.section .text.far,"ax",%progbits
	.globl far
	.type far, %function
far:
	bl _start
	ret
"#;

/// `--fix-cortex-a53-835769` under a script patches what GNU ld patches.
#[test]
fn cortex_a53_835769_under_a_script_matches_gnu_ld() {
    let tools = require!();
    let dir = scratch("835769");
    assemble(&tools, &dir, Arch::AArch64, "mac", MAC_S);
    fs::write(dir.join("link.ld"), ERRATUM_LD).unwrap();
    let args = [
        "--fix-cortex-a53-835769",
        "-T",
        "link.ld",
        "mac.o",
        "-e",
        "_start",
    ];
    qld_ok(&dir, &[&args[..], &["-o", "fixed"]].concat());
    let ours = patches(&dir, "mac.o", "fixed");
    // The independent `madd`, the one after a store, the vector load's.
    assert_eq!(sites(&ours), [0x8, 0x18, 0x20]);
    if let Some(gnu) = gnu_aarch64_ld() {
        run_ok(&dir, &gnu, &[&args[..], &["-o", "gnu"]].concat());
        assert_eq!(sites(&patches(&dir, "mac.o", "gnu")), sites(&ours));
    }
}

/// A code section larger than one pool serves (64 MiB of content on
/// AArch64) has a pool inside it, and the erratum patches of the code
/// before it go there rather than to the pool at the end, which a branch
/// from the start of the section might not reach. The output is 64 MiB.
#[test]
fn erratum_patches_use_the_nearest_pool() {
    let tools = require!();
    let dir = scratch("835769-pools");
    let source = "\t.section .text.a,\"ax\",%progbits\n\t.global _start\n\
                  \t.type _start, %function\n_start:\n\tldr x1, [x2]\n\
                  \tmadd x3, x4, x5, x6\n\tret\n\t.space 0x4000000\n\
                  \t.section .text.b,\"ax\",%progbits\n\t.global late\n\
                  \t.type late, %function\nlate:\n\tldr x1, [x2]\n\
                  \tmadd x3, x4, x5, x6\n\tret\n";
    assemble(&tools, &dir, Arch::AArch64, "big", source);
    fs::write(
        dir.join("link.ld"),
        "SECTIONS {\n  . = 0x10000;\n  .text : { *(.text.a) *(.text.b) }\n}\n",
    )
    .unwrap();
    qld_ok(
        &dir,
        &[
            "--fix-cortex-a53-835769",
            "-T",
            "link.ld",
            "big.o",
            "-e",
            "_start",
            "-o",
            "fixed",
        ],
    );
    let image = Image::load(&dir, "fixed");
    let start = image.value("_start");
    let late = image.value("late");
    let mut found = Vec::new();
    for site in [start + 4, late + 4] {
        let word = image.word(site);
        assert!(is_b(word), "{site:#x} is not patched");
        let patch = b_target(site, word);
        assert_eq!(image.word(patch), 0x9b05_1883, "patch at {patch:#x}");
        let back = image.word(patch + 4);
        assert_eq!(b_target(patch + 4, back), site + 4);
        found.push(patch);
    }
    // The first patch is in the pool before `late`, the second after it.
    assert!(found[0] < late && found[0] > start + 0x4000000);
    assert!(found[1] > late);
    drop(image);
    let _ = fs::remove_dir_all(&dir);
}
