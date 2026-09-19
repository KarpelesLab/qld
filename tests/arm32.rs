//! 32-bit Arm ELF tests (workstream W46).
//!
//! Every test links the same objects with qld and with lld and compares
//! what the two produced *symbolically*: every address an instruction
//! computes — a branch, a literal pool word added to the PC, a `movw`/
//! `movt` pair, a GOT load, a thread-pointer offset — is printed as
//! `symbol+offset`, so the comparison holds wherever each linker placed
//! things. A branch through a range-extension or interworking thunk is
//! resolved to what the thunk branches to, and a branch to a PLT entry to
//! the symbol its `.rel.plt` entry names, so the two linkers' different
//! thunk and PLT placement does not matter.
//!
//! The exception index, dynamic relocations and tags, symbol sizes and
//! `.ARM.attributes` are compared the same way.
//!
//! The objects come from `clang --target=armv7a-linux-gnueabihf`, in both
//! A32 and Thumb, and are freestanding: nothing here needs an Arm sysroot
//! or can run the output (the `arm-*` fixtures do, under qemu, in CI).
//!
//! Tools: `clang` with the Arm target, `ld.lld`, `llvm-objdump`,
//! `llvm-readelf`, from `PATH` or from `QLD_ARM_CC`, `QLD_LLD`,
//! `QLD_LLVM_OBJDUMP`. A test prints `SKIPPED:` and passes when one is
//! missing, unless `QLD_REQUIRE_ARM_TOOLS=1`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;

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

/// The tools the tests need.
struct Tools {
    cc: PathBuf,
    lld: PathBuf,
    objdump: PathBuf,
    readelf: PathBuf,
}

fn probe() -> Result<Tools, String> {
    let cc = tool("QLD_ARM_CC", "clang").ok_or("no clang")?;
    let lld = tool("QLD_LLD", "ld.lld").ok_or("no ld.lld")?;
    let objdump = tool("QLD_LLVM_OBJDUMP", "llvm-objdump").ok_or("no llvm-objdump")?;
    let readelf = tool("QLD_LLVM_READELF", "llvm-readelf").ok_or("no llvm-readelf")?;
    let version = Command::new(&objdump)
        .arg("--version")
        .output()
        .map_err(|e| e.to_string())?;
    if !String::from_utf8_lossy(&version.stdout).contains("arm") {
        return Err(format!("{} has no Arm target", objdump.display()));
    }
    let targets = Command::new(&cc)
        .arg("--print-targets")
        .output()
        .map_err(|e| e.to_string())?;
    if !String::from_utf8_lossy(&targets.stdout).contains("arm") {
        return Err(format!("{} has no Arm target", cc.display()));
    }
    Ok(Tools {
        cc,
        lld,
        objdump,
        readelf,
    })
}

fn tools() -> Option<&'static Tools> {
    static TOOLS: OnceLock<Result<Tools, String>> = OnceLock::new();
    match TOOLS.get_or_init(probe) {
        Ok(tools) => Some(tools),
        Err(why) => {
            let required = std::env::var_os("QLD_REQUIRE_ARM_TOOLS")
                .is_some_and(|v| !v.is_empty() && v != "0");
            assert!(!required, "QLD_REQUIRE_ARM_TOOLS is set but {why}");
            println!("SKIPPED: {why}");
            None
        }
    }
}

macro_rules! require {
    () => {
        match tools() {
            Some(tools) => tools,
            None => return,
        }
    };
}

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("arm32-tests")
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

/// Compiles `source` (C, C++ when `name` ends in `.cpp`, or assembly when
/// it ends in `.s`) for ARMv7-A hard float, plus `extra`, into `object`.
fn compile(tools: &Tools, dir: &Path, name: &str, source: &str, object: &str, extra: &[&str]) {
    fs::write(dir.join(name), source).unwrap();
    let cxx = name.ends_with(".cpp");
    let mut args = vec![
        "--target=armv7a-linux-gnueabihf",
        "-O1",
        "-ffreestanding",
        "-fno-stack-protector",
        "-c",
        name,
        "-o",
        object,
    ];
    if cxx {
        args.extend_from_slice(&["-x", "c++", "-fno-rtti", "-nostdinc++"]);
    }
    args.extend_from_slice(extra);
    run_ok(dir, &tools.cc, &args);
}

/// Links with lld and qld into `<name>.lld` and `<name>.qld`.
fn link_both(tools: &Tools, dir: &Path, name: &str, args: &[&str]) -> (String, String) {
    let lld = format!("{name}.lld");
    let ours = format!("{name}.qld");
    let mut lld_args: Vec<&str> = vec!["--threads=2"];
    lld_args.extend_from_slice(args);
    lld_args.extend_from_slice(&["-o", &lld]);
    run_ok(dir, &tools.lld, &lld_args);
    let mut our_args: Vec<&str> = vec!["--threads=2"];
    our_args.extend_from_slice(args);
    our_args.extend_from_slice(&["-o", &ours]);
    run_ok(dir, Path::new(env!("CARGO_BIN_EXE_qld")), &our_args);
    (lld, ours)
}

fn hex(text: &str) -> Option<u64> {
    u64::from_str_radix(text.trim().trim_start_matches("0x"), 16).ok()
}

/// An immediate as `llvm-objdump` prints it: `#0x1f`, `#-0x8` or `#12`.
fn imm(text: &str) -> Option<i64> {
    let text = text.trim().trim_start_matches('#');
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let value = match digits.strip_prefix("0x") {
        Some(h) => i64::from_str_radix(h, 16).ok()?,
        None => digits.parse().ok()?,
    };
    Some(if negative { -value } else { value })
}

struct Section {
    name: String,
    addr: u64,
    offset: u64,
    size: u64,
    nobits: bool,
}

/// What the symbolizer knows about one output.
struct Image {
    data: Vec<u8>,
    sections: Vec<Section>,
    tls: bool,
    /// Defined symbols as `(value, size, name)`, without the Thumb bit.
    symbols: Vec<(u64, u64, String)>,
    /// The addresses of Thumb functions (their symbols' bit 0).
    thumb: std::collections::BTreeSet<u64>,
    tls_symbols: Vec<(u64, u64, String)>,
    /// PLT entry address to the symbol its slot is bound to.
    plt: BTreeMap<u64, String>,
    /// Addresses that hold a thunk: what lld names and what qld leaves
    /// outside any function symbol.
    thunks: std::collections::BTreeSet<u64>,
    /// Dynamic relocation place to `type symbol`.
    dynrel: BTreeMap<u64, String>,
}

/// Dynamic relocations as `(place, type, rest)`.
fn dyn_relocs(tools: &Tools, dir: &Path, file: &str) -> Vec<(u64, String, String)> {
    let mut out = Vec::new();
    let mut dynamic = false;
    for line in run_ok(dir, &tools.readelf, &["-rW", file]).lines() {
        if line.starts_with("Relocation section") {
            dynamic = line.contains("'.rel.dyn'") || line.contains("'.rel.plt'");
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        let [place, _, kind, rest @ ..] = fields.as_slice() else {
            continue;
        };
        if !dynamic || place.len() != 8 || !kind.starts_with("R_ARM_") {
            continue;
        }
        let Some(place) = hex(place) else { continue };
        // Drop the symbol's value, which the two linkers place differently.
        let rest: Vec<&str> = match rest.first() {
            Some(value) if value.len() == 8 && rest.len() > 1 => rest.get(1..).unwrap().to_vec(),
            _ => rest.to_vec(),
        };
        out.push((place, (*kind).to_string(), rest.join(" ")));
    }
    out
}

impl Image {
    fn load(tools: &Tools, dir: &Path, file: &str) -> Self {
        let data = fs::read(dir.join(file)).unwrap();
        let mut sections = Vec::new();
        for line in run_ok(dir, &tools.readelf, &["-SW", file]).lines() {
            let Some((_, rest)) = line.split_once(']') else {
                continue;
            };
            let fields: Vec<&str> = rest.split_whitespace().collect();
            let [name, kind, addr, offset, size, ..] = fields.as_slice() else {
                continue;
            };
            let (Some(addr), Some(offset), Some(size)) = (hex(addr), hex(offset), hex(size)) else {
                continue;
            };
            sections.push(Section {
                name: (*name).to_string(),
                addr,
                offset,
                size,
                nobits: *kind == "NOBITS",
            });
        }
        let tls = run_ok(dir, &tools.readelf, &["-lW", file])
            .lines()
            .any(|l| l.trim_start().starts_with("TLS "));
        let mut symbols = Vec::new();
        let mut tls_symbols = Vec::new();
        let mut thumb = std::collections::BTreeSet::new();
        let mut thunk_addresses = std::collections::BTreeSet::new();
        for line in run_ok(dir, &tools.readelf, &["-sW", file]).lines() {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let [index, value, size, kind, _, _, ndx, name, ..] = fields.as_slice() else {
                continue;
            };
            if !index.ends_with(':') || index.trim_end_matches(':').parse::<u32>().is_err() {
                continue;
            }
            if *ndx == "UND"
                || *kind == "SECTION"
                || *kind == "FILE"
                || name.starts_with(".L")
                // Mapping symbols, and lld's names for its thunks.
                || name.starts_with('$')
            {
                continue;
            }
            // lld names its thunks and gives them a size; qld's are
            // nameless code after the last function of the section.
            if name.contains("Thunk") {
                if let Some(value) = hex(value) {
                    thunk_addresses.insert(value & !1);
                }
                continue;
            }
            let (Some(value), Some(size)) = (hex(value), imm(size)) else {
                continue;
            };
            let entry = (value & !1, size as u64, (*name).to_string());
            if *kind == "TLS" {
                tls_symbols.push(entry);
            } else {
                if value & 1 != 0 {
                    thumb.insert(value & !1);
                }
                symbols.push(entry);
            }
        }
        symbols.sort();
        tls_symbols.sort();
        let mut dynrel = BTreeMap::new();
        let mut plt_slots: BTreeMap<u64, String> = BTreeMap::new();
        for (place, kind, rest) in dyn_relocs(tools, dir, file) {
            if kind == "R_ARM_JUMP_SLOT" {
                plt_slots.insert(place, rest.clone());
            }
            let what = if kind.ends_with("RELATIVE") {
                "addend".to_string()
            } else {
                rest
            };
            dynrel.entry(place).or_insert(format!("{kind} {what}"));
        }
        let mut image = Self {
            data,
            sections,
            tls,
            thumb,
            symbols,
            tls_symbols,
            plt: BTreeMap::new(),
            thunks: thunk_addresses,
            dynrel,
        };
        image.plt = image.plt_entries(&plt_slots);
        image
    }

    /// The `.plt` entries, by the symbol of the `.got.plt` slot each one
    /// jumps through. Both linkers write GNU ld's three-instruction entry
    /// (`add ip, pc; add ip, ip; ldr pc, [ip]!`).
    fn plt_entries(&self, slots: &BTreeMap<u64, String>) -> BTreeMap<u64, String> {
        let mut out = BTreeMap::new();
        let Some(plt) = self.sections.iter().find(|s| s.name == ".plt") else {
            return out;
        };
        let mut at = plt.addr;
        while at < plt.addr + plt.size {
            if let Some(slot) = self.plt_slot(at)
                && let Some(name) = slots.get(&slot)
            {
                out.insert(at, name.clone());
            }
            at += 4;
        }
        out
    }

    /// The `.got.plt` slot the PLT entry at `at` jumps through.
    fn plt_slot(&self, at: u64) -> Option<u64> {
        let (a, b, c) = (self.word(at)?, self.word(at + 4)?, self.word(at + 8)?);
        if a & 0xffff_ff00 != 0xe28f_c600
            || b & 0xffff_ff00 != 0xe28c_ca00
            || c & 0xffff_f000 != 0xe5bc_f000
        {
            return None;
        }
        let offset = u64::from(((a & 0xff) << 20) + ((b & 0xff) << 12) + (c & 0xfff));
        Some(at + 8 + offset)
    }

    /// Where the thunk at `at` branches, if there is one there: both
    /// linkers write `movw ip; movt ip; bx ip`, with an `add ip, pc`
    /// before the `bx` in position-independent output, and lld may write
    /// a plain `b` when the destination is in range of the thunk.
    fn thunk_target(&self, at: u64) -> Option<u64> {
        // A thunk lld named, or code outside any function: qld's pools
        // sit after the last function of their output section.
        if !self.thunks.contains(&at) && self.function_at(at).is_some() {
            return None;
        }
        if let Some(word) = self.word(at) {
            // A32 `b <label>`.
            if word & 0xff00_0000 == 0xea00_0000 {
                let offset = ((word & 0x00ff_ffff) << 8) as i32 >> 6;
                return Some(at.wrapping_add(8).wrapping_add(offset as u64) & 0xffff_ffff);
            }
            // Thumb `b.w <label>`.
            let (hi, lo) = (word & 0xffff, word >> 16);
            if hi & 0xf800 == 0xf000 && lo & 0xd000 == 0x9000 {
                let s = (hi >> 10) & 1;
                let i1 = !((lo >> 13) ^ s) & 1;
                let i2 = !((lo >> 11) ^ s) & 1;
                let value = (s << 24)
                    | (i1 << 23)
                    | (i2 << 22)
                    | ((hi & 0x3ff) << 12)
                    | ((lo & 0x7ff) << 1);
                let offset = ((value << 7) as i32 >> 7) as i64;
                return Some(at.wrapping_add(4).wrapping_add_signed(offset) & 0xffff_ffff | 1);
            }
        }
        self.long_thunk_target(at)
    }

    /// The destination of a `movw`/`movt`/`bx ip` thunk at `at`.
    fn long_thunk_target(&self, at: u64) -> Option<u64> {
        let arm_imm16 = |word: u32| ((word >> 4) & 0xf000) | (word & 0xfff);
        let thumb_imm16 = |hi: u32, lo: u32| {
            ((hi & 0xf) << 12) | ((hi & 0x400) << 1) | ((lo & 0x7000) >> 4) | (lo & 0xff)
        };
        let (a, b) = (self.word(at)?, self.word(at + 4)?);
        // A32 `movw ip` / `movt ip`.
        if a & 0xfff0_f000 == 0xe300_c000 && b & 0xfff0_f000 == 0xe340_c000 {
            let value = u64::from(arm_imm16(a) | (arm_imm16(b) << 16));
            return match self.word(at + 8)? {
                0xe12f_ff1c => Some(value),
                // `add ip, ip, pc` reads the PC as the thunk plus 16.
                0xe08c_c00f if self.word(at + 12) == Some(0xe12f_ff1c) => {
                    Some(value.wrapping_add(at + 16) & 0xffff_ffff)
                }
                _ => None,
            };
        }
        // Thumb `movw ip` / `movt ip`, two halfwords each.
        let (h0, h1, h2, h3) = (a & 0xffff, a >> 16, b & 0xffff, b >> 16);
        if h0 & 0xfbf0 == 0xf240 && h2 & 0xfbf0 == 0xf2c0 && (h1 & 0x0f00) == 0x0c00 {
            let value = u64::from(thumb_imm16(h0, h1) | (thumb_imm16(h2, h3) << 16));
            let (next, after) = (self.half(at + 8)?, self.half(at + 10).unwrap_or(0));
            return match (next, after) {
                (0x4760, _) => Some(value),
                // `add ip, pc` reads the PC as the thunk plus 12.
                (0x44fc, 0x4760) => Some(value.wrapping_add(at + 12) & 0xffff_ffff),
                _ => None,
            };
        }
        None
    }

    fn section_of(&self, address: u64) -> Option<&Section> {
        self.sections
            .iter()
            .rfind(|s| s.addr != 0 && s.addr <= address && address < s.addr + s.size.max(1))
    }

    fn bytes_at(&self, address: u64, len: usize) -> Option<&[u8]> {
        let s = self.section_of(address)?;
        if s.nobits {
            return None;
        }
        let at = (s.offset + address - s.addr) as usize;
        self.data.get(at..at + len)
    }

    fn word(&self, address: u64) -> Option<u32> {
        let bytes = self.bytes_at(address, 4)?;
        Some(u32::from_le_bytes(bytes.try_into().ok()?))
    }

    fn half(&self, address: u64) -> Option<u32> {
        let bytes = self.bytes_at(address, 2)?;
        Some(u32::from(u16::from_le_bytes(bytes.try_into().ok()?)))
    }

    /// The name of the code at `address`: a PLT entry, a thunk resolved to
    /// what it branches to, or a symbol.
    fn symbolize_code(&self, address: u64) -> String {
        let address = address & !1;
        if let Some(name) = self.plt.get(&address) {
            return format!("PLT({name})");
        }
        if let Some(target) = self.thunk_target(address)
            && target != address
        {
            return format!("THUNK({})", self.symbolize_code(target));
        }
        self.symbolize(address, false)
    }

    fn symbolize(&self, address: u64, deref: bool) -> String {
        let lowest = self
            .sections
            .iter()
            .filter(|s| s.addr != 0)
            .map(|s| s.addr)
            .min()
            .unwrap_or(0)
            & !0xffff;
        let highest = self
            .sections
            .iter()
            .map(|s| s.addr + s.size)
            .max()
            .unwrap_or(0);
        if address < lowest || address > highest.saturating_add(0x10_0000) {
            return format!("{address:#x}");
        }
        if let Some(name) = self.plt.get(&address) {
            return format!("PLT({name})");
        }
        let section = self.section_of(address);
        if let Some(s) = section
            && (s.name == ".got" || s.name == ".got.plt")
        {
            if let Some(reloc) = self.dynrel.get(&address) {
                return format!("GOT[{reloc}]");
            }
            if deref {
                return match self.word(address) {
                    Some(0) | None => "GOT[0]".to_string(),
                    Some(word) => format!("GOT[{}]", self.symbolize(u64::from(word), false)),
                };
            }
        }
        let best = self
            .symbols
            .iter()
            .filter(|(value, size, _)| {
                *value <= address && (address < value + size || (*size == 0 && address == *value))
            })
            .max_by_key(|(value, size, _)| (*value, *size));
        if let Some((value, _, name)) = best {
            return if address == *value {
                name.clone()
            } else {
                format!("{name}+{:#x}", address - value)
            };
        }
        if let Some(s) = section {
            return format!("{}+{:#x}", s.name, address - s.addr);
        }
        let end = self
            .sections
            .iter()
            .filter(|s| s.addr != 0 && s.addr + s.size <= address)
            .map(|s| (s.addr + s.size, &s.name))
            .max();
        match end {
            Some((end, name)) if end == address => format!("END({name})"),
            Some((end, name)) => format!("END({name})+{:#x}", address - end),
            None => format!("{address:#x}"),
        }
    }

    fn tls_symbolize(&self, offset: i64) -> String {
        // The thread pointer is eight bytes below the block (variant I).
        let offset = offset.wrapping_sub(8) as u64;
        for (value, size, name) in &self.tls_symbols {
            if *value <= offset && offset < value + (*size).max(1) {
                return if offset == *value {
                    format!("TLS({name})")
                } else {
                    format!("TLS({name}+{:#x})", offset - value)
                };
            }
        }
        format!("TLS({offset:#x})")
    }

    /// The function symbol covering `address` and whether it is Thumb
    /// code, for grouping a listing.
    fn function_at(&self, address: u64) -> Option<(&str, bool)> {
        self.symbols
            .iter()
            .filter(|(value, size, _)| *value <= address && address < value + size.max(&1))
            .max_by_key(|(value, ..)| *value)
            .map(|(value, _, name)| (name.as_str(), self.thumb.contains(value)))
    }
}

/// What one register holds while a function is disassembled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Value {
    /// A number (a literal pool word, or a `movw`/`movt` pair).
    Number(u64),
    /// An address the code computed.
    Address(u64),
    /// The thread pointer.
    Tp,
}

/// The disassembly of `file` with every computed address symbolized, one
/// block per function, blocks sorted by name.
fn listing(tools: &Tools, dir: &Path, file: &str) -> String {
    let image = Image::load(tools, dir, file);
    let text = run_ok(dir, &tools.objdump, &["-d", "--no-show-raw-insn", file]);
    let mut blocks: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut regs: BTreeMap<String, Value> = BTreeMap::new();
    let mut current = String::new();
    let mut thumb = false;
    for line in text.lines() {
        let Some((pc, insn)) = line.split_once(':') else {
            continue;
        };
        let Some(pc) = hex(pc.trim()) else { continue };
        let Some((function, is_thumb)) = image.function_at(pc) else {
            continue;
        };
        if function != current {
            current = function.to_string();
            thumb = is_thumb;
            regs.clear();
            blocks.entry(current.clone()).or_default();
        }
        // The disassembler's `<symbol>` is dropped, since the two linkers
        // name things differently; the comment after it holds the address
        // of a literal.
        let mut insn = insn.to_string();
        while let (Some(open), Some(close)) = (insn.find('<'), insn.find('>')) {
            if close < open {
                break;
            }
            insn.replace_range(open..=close, "");
        }
        let (insn, comment) = match insn.split_once('@') {
            Some((insn, comment)) => (insn.trim().to_string(), comment.trim().to_string()),
            None => (insn.trim().to_string(), String::new()),
        };
        let (op, args) = insn.split_once(char::is_whitespace).unwrap_or((&insn, ""));
        // Literal pool data, which objdump prints as raw bytes and a
        // `.word`: the instructions that use it are compared instead.
        if op.starts_with('.')
            || op == "unimp"
            || op.is_empty()
            || (op.len() == 2 && hex(op).is_some())
        {
            continue;
        }
        let ops: Vec<String> = args
            .split(',')
            .map(|a| a.trim().to_string())
            .filter(|a| !a.is_empty())
            .collect();
        let literal = comment
            .split_whitespace()
            .next()
            .and_then(hex)
            .filter(|_| comment.starts_with("0x"));
        let text = render(&image, &mut regs, pc, thumb, op, &ops, literal);
        if let Some(block) = blocks.get_mut(&current) {
            block.push(format!("  {text}"));
        }
    }
    blocks
        .into_iter()
        .map(|(name, lines)| format!("{name}:\n{}", lines.join("\n")))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The PC an instruction at `pc` reads: four bytes ahead in Thumb, eight
/// in A32. Thumb instructions are two-byte aligned, A32 four.
fn pc_value(pc: u64, thumb: bool) -> u64 {
    if thumb { pc + 4 } else { pc + 8 }
}

/// Renders one instruction, symbolizing what it computes and tracking what
/// the registers hold.
fn render(
    image: &Image,
    regs: &mut BTreeMap<String, Value>,
    pc: u64,
    thumb: bool,
    op: &str,
    ops: &[String],
    literal: Option<u64>,
) -> String {
    let all = ops.join(" ");
    let base = op.trim_end_matches(".w").trim_end_matches(".n");
    // Branches: the operand is the target address the disassembler
    // resolved.
    if (base.starts_with('b') || base.starts_with("cb"))
        && !base.starts_with("bic")
        && !base.starts_with("bfi")
        && !base.starts_with("bl.")
        && let Some(target) = ops.last().and_then(|o| o.strip_prefix("0x")).and_then(hex)
    {
        if let Some(first) = ops.first() {
            regs.remove(first);
        }
        let rest = ops.get(..ops.len() - 1).unwrap_or_default().join(" ");
        let target = image.symbolize_code(target);
        let op = if target.starts_with("THUNK(") && (op == "bl" || op == "blx") {
            "bl*"
        } else {
            op
        };
        return format!("{op} {rest} {target}");
    }
    // A literal load: the word is the value, and the address it was loaded
    // from is not compared (the pools differ).
    if op.starts_with("ldr") && ops.len() >= 2 && ops[1].starts_with("[pc") {
        let value = literal.and_then(|at| image.word(at));
        match (ops.first(), value) {
            (Some(reg), Some(value)) => {
                regs.insert(reg.clone(), Value::Number(u64::from(value)));
                return format!("{op} {reg} <literal>");
            }
            _ => return format!("{op} {all}"),
        }
    }
    // `mrc p15, #0, rX, c13, c0, #3` reads the thread pointer.
    if op == "mrc"
        && ops.len() >= 3
        && ops[0] == "p15"
        && ops.get(4).map(String::as_str) == Some("c0")
    {
        regs.insert(ops[2].clone(), Value::Tp);
        return format!("{op} {} TP", ops[2]);
    }
    if op.starts_with("movw") && ops.len() == 2 {
        if let Some(value) = imm(&ops[1]) {
            regs.insert(ops[0].clone(), Value::Number(value as u64 & 0xffff));
        }
        return format!("{op} {}", ops[0]);
    }
    if op.starts_with("movt") && ops.len() == 2 {
        let low = match regs.get(&ops[0]) {
            Some(Value::Number(value)) => *value & 0xffff,
            _ => 0,
        };
        let value = (imm(&ops[1]).unwrap_or(0) as u64) << 16 | low;
        regs.insert(ops[0].clone(), Value::Number(value));
        // The pair is one address: print it here.
        return format!("{op} {} {}", ops[0], image.symbolize_code(value));
    }
    // `add rD, pc, rS` (A32) and `add rD, pc` (Thumb) make an address.
    if op.starts_with("add")
        && (ops.get(1).map(String::as_str) == Some("pc")
            || ops.get(2).map(String::as_str) == Some("pc"))
    {
        let source = if ops.get(1).map(String::as_str) == Some("pc") {
            ops.get(2).or(ops.first())
        } else {
            ops.first()
        };
        let value = match source.and_then(|r| regs.get(r)) {
            Some(Value::Number(value)) => Some(*value),
            _ => None,
        };
        return match value {
            Some(value) => {
                let address = pc_value(pc, thumb).wrapping_add(value) & 0xffff_ffff;
                regs.insert(ops[0].clone(), Value::Address(address));
                format!("{op} {} {}", ops[0], image.symbolize(address, false))
            }
            None => {
                regs.remove(&ops[0]);
                format!("{op} {all}")
            }
        };
    }
    // `ldr rD, [pc, rS]`: a GOT entry addressed from the PC.
    if op.starts_with("ldr")
        && ops.len() == 2
        && ops[1].starts_with("[pc,")
        && let Some(register) = ops[1]
            .trim_start_matches("[pc,")
            .trim_end_matches(']')
            .trim()
            .strip_prefix('r')
    {
        let register = format!("r{register}");
        if let Some(Value::Number(value)) = regs.get(&register) {
            let address = pc_value(pc, thumb).wrapping_add(*value) & 0xffff_ffff;
            regs.insert(ops[0].clone(), Value::Address(address));
            return format!("{op} {} {}", ops[0], image.symbolize(address, true));
        }
    }
    // A load or store through a register whose value is known.
    if (op.starts_with("ldr") || op.starts_with("str")) && ops.len() >= 2 {
        let inside = ops[1].trim_start_matches('[').trim_end_matches(']');
        let offset = ops
            .get(2)
            .and_then(|o| imm(o.trim_end_matches(']')))
            .unwrap_or(0);
        match regs.get(inside) {
            Some(Value::Address(address)) => {
                let address = address.wrapping_add(offset as u64);
                let text = format!("{op} {} [{}]", ops[0], image.symbolize(address, true));
                if op.starts_with("ldr") {
                    regs.remove(&ops[0]);
                }
                return text;
            }
            Some(Value::Tp) if image.tls => {
                let text = format!("{op} {} {}", ops[0], image.tls_symbolize(offset));
                if op.starts_with("ldr") {
                    regs.remove(&ops[0]);
                }
                return text;
            }
            _ => {}
        }
        // `ldr rD, [rTp, rOff]`: local-exec and initial-exec accesses.
        if let (Some(Value::Tp), Some(Value::Number(offset))) = (
            regs.get(inside),
            ops.get(2)
                .map(|o| o.trim_end_matches(']'))
                .and_then(|r| regs.get(r)),
        ) {
            let text = format!("{op} {} {}", ops[0], image.tls_symbolize(*offset as i64));
            if op.starts_with("ldr") {
                regs.remove(&ops[0]);
            }
            return text;
        }
    }
    if let Some(first) = ops.first() {
        regs.remove(first);
    }
    format!("{op} {all}")
}

/// The exception index of `file`, as `function: unwind`.
fn exception_index(image: &Image) -> Vec<String> {
    let mut out = Vec::new();
    let Some(exidx) = image.sections.iter().find(|s| s.name == ".ARM.exidx") else {
        return out;
    };
    let mut at = exidx.addr;
    while at + 8 <= exidx.addr + exidx.size {
        let (Some(first), Some(second)) = (image.word(at), image.word(at + 4)) else {
            break;
        };
        let target = at.wrapping_add(prel31(first)) & 0xffff_ffff;
        let what = match second {
            1 => "EXIDX_CANTUNWIND".to_string(),
            word if word & 0x8000_0000 != 0 => format!("inline {word:#010x}"),
            word => {
                let extab = (at + 4).wrapping_add(prel31(word)) & 0xffff_ffff;
                format!("extab {}", image.symbolize(extab, false))
            }
        };
        // The sentinel covers the end of the last executable section,
        // which each linker follows with a different section.
        let sentinel = at + 8 == exidx.addr + exidx.size;
        let covered = if sentinel {
            "END(code)".to_string()
        } else {
            image.symbolize_code(target)
        };
        out.push(format!("exidx {covered}: {what}"));
        at += 8;
    }
    // The entries are in address order; the order of the code itself is
    // the linker's (GNU ld sorts `.text.unlikely` before `.text`, lld
    // keeps the input order), so the comparison is by content.
    assert!(
        out.len() * 8 == (exidx.size as usize),
        "exception index of {} entries in {} bytes",
        out.len(),
        exidx.size
    );
    out.sort();
    out
}

/// A `PREL31` offset, sign-extended.
fn prel31(word: u32) -> u64 {
    let value = word & 0x7fff_ffff;
    if value & 0x4000_0000 != 0 {
        (value | 0x8000_0000) as u64 | 0xffff_ffff_0000_0000
    } else {
        u64::from(value)
    }
}

/// Dynamic relocations and tags, symbol sizes, the exception index and the
/// build attributes of `file`, symbolized.
fn metadata(tools: &Tools, dir: &Path, file: &str) -> String {
    let image = Image::load(tools, dir, file);
    let mut out = Vec::new();
    let mut relocs: Vec<String> = dyn_relocs(tools, dir, file)
        .into_iter()
        .map(|(place, kind, rest)| {
            let rest = if kind.ends_with("RELATIVE") {
                hex(&rest).map_or(rest, |a| image.symbolize(a, false))
            } else {
                rest
            };
            format!("reloc {kind} at {}: {rest}", image.symbolize(place, false))
        })
        .collect();
    relocs.sort();
    out.extend(relocs);
    let mut tags = Vec::new();
    for line in run_ok(dir, &tools.readelf, &["-dW", file]).lines() {
        let Some((_, rest)) = line.split_once('(') else {
            continue;
        };
        let Some((tag, value)) = rest.split_once(')') else {
            continue;
        };
        if !line.trim_start().starts_with("0x") || tag == "HASH" {
            continue;
        }
        if matches!(tag, "NEEDED" | "SONAME" | "FLAGS" | "FLAGS_1" | "TEXTREL") {
            tags.push(format!("dyn {tag} {}", value.trim()));
        } else {
            tags.push(format!("dyn {tag}"));
        }
    }
    tags.sort();
    out.extend(tags);
    // `_DYNAMIC` and the symbols GNU ld's default script always defines in
    // an executable are in qld's symbol table whether or not anything
    // refers to them; lld defines them only when something does.
    let always_defined = [
        "_DYNAMIC",
        "_edata",
        "__bss_start",
        "_end",
        "__bss_start__",
        "_bss_end__",
        "__bss_end__",
        "__end__",
        "__exidx_start",
        "__exidx_end",
    ];
    let mut sizes: Vec<String> = image
        .symbols
        .iter()
        .filter(|(_, _, name)| !always_defined.contains(&name.as_str()))
        .map(|(_, size, name)| format!("sym {name} size {size}"))
        .collect();
    sizes.sort();
    out.extend(sizes);
    out.extend(exception_index(&image));
    out.join("\n")
}

/// Asserts that `lld` and `ours` agree symbolically.
fn assert_same(tools: &Tools, dir: &Path, lld: &str, ours: &str) {
    for (what, f) in [
        ("code", listing as fn(&Tools, &Path, &str) -> String),
        ("metadata", metadata),
    ] {
        let expected = f(tools, dir, lld);
        let actual = f(tools, dir, ours);
        if expected != actual {
            let diff: Vec<String> = expected
                .lines()
                .zip(actual.lines())
                .filter(|(a, b)| a != b)
                .take(20)
                .map(|(a, b)| format!("  lld: {a}\n  qld: {b}"))
                .collect();
            panic!(
                "{ours}: {what} differs from lld ({} vs {} lines):\n{}",
                expected.lines().count(),
                actual.lines().count(),
                diff.join("\n")
            );
        }
    }
}

/// Their definitions: absolute addresses, one A32 and one Thumb.
const FARDEF_S: &str = r#"
	.globl far_func
	.type far_func, %function
	.set far_func, 0x4000000
	.globl far_thumb
	.type far_thumb, %function
	.set far_thumb, 0x4000011
"#;

const MAIN_C: &str = r#"
extern int arm_func(int);
extern int thumb_func(int);
extern int far_func(int);
extern int far_caller(void);
int data_var = 5;
int bss_var;
static const char *strs[] = {"a", "bb", "ccc"};
int (*fp)(int) = thumb_func;
int dispatch(int x) {
  switch (x) {
    case 0: return arm_func(1);
    case 1: return thumb_func(2);
    case 2: return far_func(3) + far_caller();
    case 3: return data_var;
    case 4: return bss_var;
    default: return (int)(long)strs[1];
  }
}
int tail(int x) { return arm_func(x + 1); }
void _start(void) {
  int r = dispatch(3) + tail(4) + fp(5);
  for (;;) __asm__ volatile("" :: "r"(r));
}
"#;

const ARM_C: &str = r#"
extern int thumb_func(int);
int arm_func(int x) { return thumb_func(x) * 2; }
"#;

const THUMB_C: &str = r#"
extern int arm_func(int);
int thumb_func(int x) { return x > 1 ? arm_func(x - 1) : 1; }
"#;

/// Destinations no `bl` reaches in either state: absolute addresses far
/// from the image, so that a thunk at the end of the output section is
/// near its callers (as in the AArch64 thunk fixture).
const FAR_S: &str = r#"
	.syntax unified
	.text
	.arm
	.globl far_caller
	.type far_caller, %function
far_caller:
	bl far_func
	bl far_thumb
	b far_func
	bx lr
	.size far_caller, .-far_caller
	.thumb
	.globl far_thumb_caller
	.thumb_func
	.type far_thumb_caller, %function
far_thumb_caller:
	bl far_thumb
	bl far_func
	b.w far_thumb
	bx lr
	.size far_thumb_caller, .-far_thumb_caller
"#;

/// Branches of every kind, in both states, to both states.
const BRANCHES_S: &str = r#"
	.syntax unified
	.text
	.arm
	.globl arm_caller
	.type arm_caller, %function
arm_caller:
	bl arm_target
	bl thumb_target
	bleq arm_target
	b arm_target
	beq thumb_target
	bx lr
	.size arm_caller, .-arm_caller

	.thumb
	.globl thumb_caller
	.thumb_func
	.type thumb_caller, %function
thumb_caller:
	bl thumb_target
	bl arm_target
	b.w thumb_target
	beq.w arm_target
	bne.n thumb_target
	bx lr
	.size thumb_caller, .-thumb_caller

	.arm
	.globl arm_target
	.type arm_target, %function
arm_target:
	bx lr
	.size arm_target, .-arm_target

	.thumb
	.globl thumb_target
	.thumb_func
	.type thumb_target, %function
thumb_target:
	bx lr
	.size thumb_target, .-thumb_target

	.globl _start
	.thumb_func
	.type _start, %function
_start:
	bl arm_caller
	bl thumb_caller
	bx lr
	.size _start, .-_start
"#;

#[test]
fn interworking_and_thunks_match_lld() {
    let tools = require!();
    let dir = scratch("interworking");
    compile(tools, &dir, "main.c", MAIN_C, "main.o", &["-mthumb"]);
    compile(tools, &dir, "arm.c", ARM_C, "arm.o", &["-marm"]);
    compile(tools, &dir, "thumb.c", THUMB_C, "thumb.o", &["-mthumb"]);
    compile(tools, &dir, "far.s", FAR_S, "far.o", &[]);
    compile(tools, &dir, "fardef.s", FARDEF_S, "fardef.o", &[]);
    compile(tools, &dir, "branches.s", BRANCHES_S, "branches.o", &[]);
    for (name, args) in [
        (
            "static",
            &[
                "-static", "-e", "_start", "main.o", "arm.o", "thumb.o", "far.o", "fardef.o",
            ][..],
        ),
        ("branches", &["-static", "-e", "_start", "branches.o"]),
    ] {
        let (lld, ours) = link_both(tools, &dir, name, args);
        assert_same(tools, &dir, &lld, &ours);
    }
    // The far calls went through thunks (the destinations are absolute
    // addresses, which have no symbol), and the near ones interwork.
    let code = listing(tools, &dir, "static.qld");
    assert!(code.contains("THUNK(0x4000000)"), "{code}");
    assert!(code.contains("THUNK(0x4000010)"), "{code}");
    assert!(code.contains("blx  arm_func"), "{code}");
}

const LIB_C: &str = r#"
__thread int lib_tls = 1;
static __thread int lib_tls_local = 2;
extern __thread int ext_tls;
int lib_data = 42;
extern int exe_func(int);
extern void (*hook)(void);
int lib_func(int x) { return x + lib_data + lib_tls + lib_tls_local + ext_tls + exe_func(x); }
static int helper(int x) { return x * 7; }
int lib_func2(int x) { return helper(x) + lib_func(x); }
int *lib_ptr = &lib_data;
int (*fp2)(int) = lib_func;
void call_hook(void) { if (hook) hook(); }
"#;

const DEP_C: &str = r#"
__thread int ext_tls = 3;
void (*hook)(void);
int exe_func(int x) { return x; }
"#;

const EXE_C: &str = r#"
extern int lib_func(int);
extern int lib_func2(int);
extern int lib_data;
extern __thread int lib_tls;
__thread int exe_tls = 4;
static __thread int exe_tls_local = 5;
int (*ptr)(int) = lib_func;
int main_value;
void _start(void) {
  int r = lib_func(1) + lib_func2(2) + lib_data + lib_tls + exe_tls + exe_tls_local + ptr(3);
  main_value = r;
  for (;;) __asm__ volatile("" :: "r"(r));
}
"#;

#[test]
fn dynamic_links_match_lld() {
    let tools = require!();
    let dir = scratch("dynamic");
    compile(tools, &dir, "lib.c", LIB_C, "lib.o", &["-fPIC", "-mthumb"]);
    compile(
        tools,
        &dir,
        "lib.c",
        LIB_C,
        "lib-arm.o",
        &["-fPIC", "-marm"],
    );
    compile(tools, &dir, "dep.c", DEP_C, "dep.o", &["-fPIC"]);
    compile(tools, &dir, "exe.c", EXE_C, "exe.o", &["-fPIE", "-mthumb"]);
    for (name, args) in [
        (
            "lib",
            &["-shared", "-soname", "lib.so", "lib.o", "dep.o"][..],
        ),
        (
            "lib-arm",
            &["-shared", "-soname", "lib.so", "lib-arm.o", "dep.o"],
        ),
    ] {
        let (lld, ours) = link_both(tools, &dir, name, args);
        assert_same(tools, &dir, &lld, &ours);
    }
    // The executable links against lld's copy of the library, so both
    // linkers see the same dynamic symbols.
    let (lld, ours) = link_both(
        tools,
        &dir,
        "exe",
        &[
            "-pie",
            "--allow-shlib-undefined",
            "-e",
            "_start",
            "exe.o",
            "lib.lld",
            "-rpath",
            ".",
        ],
    );
    assert_same(tools, &dir, &lld, &ours);
}

const TLS_C: &str = r#"
__thread int le_var = 1;
__attribute__((tls_model("initial-exec"))) __thread int ie_var = 2;
__attribute__((tls_model("local-dynamic"))) static __thread int ld_var[4];
__attribute__((tls_model("global-dynamic"))) __thread int gd_var = 4;
extern int sink(int);
int use_le(void) { return le_var; }
int use_ie(void) { return ie_var; }
int use_ld(int i) { return ld_var[i] + sink(ld_var[0]); }
int use_gd(void) { return gd_var; }
"#;

#[test]
fn tls_models_match_lld() {
    let tools = require!();
    let dir = scratch("tls");
    compile(tools, &dir, "tls.c", TLS_C, "tls-pic.o", &["-fPIC"]);
    compile(tools, &dir, "tls.c", TLS_C, "tls.o", &[]);
    compile(
        tools,
        &dir,
        "tls.c",
        TLS_C,
        "tls-thumb.o",
        &["-fPIC", "-mthumb"],
    );
    let stub = "int sink(int x) { return x; }\nvoid _start(void) {}\n";
    compile(tools, &dir, "stub.c", stub, "stub.o", &["-fPIC"]);
    for (name, args) in [
        ("tls-shared", &["-shared", "tls-pic.o", "stub.o"][..]),
        ("tls-thumb", &["-shared", "tls-thumb.o", "stub.o"]),
        (
            "tls-static",
            &["-static", "-e", "_start", "tls.o", "stub.o"],
        ),
    ] {
        let (lld, ours) = link_both(tools, &dir, name, args);
        assert_same(tools, &dir, &lld, &ours);
    }
}

const THROW_CPP: &str = r#"
struct Boom { int code; };
int risky(int x);
int caller(int x) {
  try {
    return risky(x);
  } catch (const Boom &b) {
    return b.code;
  } catch (int v) {
    return v;
  }
}
int thrower(int x) {
  if (x > 2) throw Boom{x};
  throw x;
}
int noexcept_fn(int x) noexcept { return x + 1; }
"#;

/// Executable sections with and without unwind information, to exercise
/// the `EXIDX_CANTUNWIND` entries the linker adds for the gaps.
const GAPS_S: &str = r#"
	.syntax unified
	.section .text.one, "ax", %progbits
	.globl one
	.type one, %function
one:
	bx lr
	.size one, .-one
	.section .ARM.exidx.text.one, "ao", %unwind, .text.one
	.word one(PREL31)
	.word 0x80b0b0b0

	.section .text.two, "ax", %progbits
	.globl two
	.type two, %function
two:
	bx lr
	.size two, .-two

	.section .text.three, "ax", %progbits
	.globl three
	.type three, %function
three:
	bx lr
	.size three, .-three
	.section .ARM.exidx.text.three, "ao", %unwind, .text.three
	.word three(PREL31)
	.word 1

	.section .text.four, "ax", %progbits
	.globl four
	.globl _start
	.type four, %function
four:
_start:
	bl one
	bl two
	bl three
	bx lr
	.size four, .-four
"#;

#[test]
fn exception_tables_match_lld() {
    let tools = require!();
    let dir = scratch("exceptions");
    compile(tools, &dir, "throw.cpp", THROW_CPP, "throw.o", &["-fPIC"]);
    compile(
        tools,
        &dir,
        "throw.cpp",
        THROW_CPP,
        "throw-thumb.o",
        &["-fPIC", "-mthumb"],
    );
    compile(tools, &dir, "gaps.s", GAPS_S, "gaps.o", &[]);
    for (name, args) in [
        ("throw", &["-shared", "throw.o"][..]),
        ("throw-thumb", &["-shared", "throw-thumb.o"]),
        ("gaps", &["-static", "-e", "_start", "gaps.o"]),
    ] {
        let (lld, ours) = link_both(tools, &dir, name, args);
        assert_same(tools, &dir, &lld, &ours);
    }
    // The gaps got their own entries, and the table ends with a sentinel.
    let image = Image::load(tools, &dir, "gaps.qld");
    let index = exception_index(&image);
    assert!(
        index.iter().any(|e| e.contains("two: EXIDX_CANTUNWIND")),
        "{index:?}"
    );
    assert!(
        index.last().is_some_and(|e| e.contains("EXIDX_CANTUNWIND")),
        "{index:?}"
    );
}

const IFUNC_C: &str = r#"
static int impl_one(void) { return 1; }
static int impl_two(void) { return 2; }
static void *resolve_pick(void) { return (void *)impl_two; }
int pick(void) __attribute__((ifunc("resolve_pick")));
int (*pointer)(void) = pick;
void _start(void) {
  int r = pick() + pointer();
  for (;;) __asm__ volatile("" :: "r"(r));
}
"#;

#[test]
fn ifunc_links_match_lld() {
    let tools = require!();
    let dir = scratch("ifunc");
    compile(tools, &dir, "ifunc.c", IFUNC_C, "ifunc.o", &[]);
    compile(
        tools,
        &dir,
        "ifunc.c",
        IFUNC_C,
        "ifunc-thumb.o",
        &["-mthumb"],
    );
    for (name, args) in [
        ("ifunc", &["-static", "-e", "_start", "ifunc.o"][..]),
        ("ifunc-thumb", &["-static", "-e", "_start", "ifunc-thumb.o"]),
    ] {
        let (lld, ours) = link_both(tools, &dir, name, args);
        assert_same(tools, &dir, &lld, &ours);
    }
}

/// Absolute addressing: `movw`/`movt` pairs and data relocations, which a
/// position-dependent executable may use.
const ABSOLUTE_S: &str = r#"
	.syntax unified
	.text
	.arm
	.globl _start
	.type _start, %function
_start:
	movw r0, :lower16:target
	movt r0, :upper16:target
	movw r1, :lower16:thumb_target
	movt r1, :upper16:thumb_target
	movw r2, :lower16:(data + 8)
	movt r2, :upper16:(data + 8)
	ldr r3, [r0]
	bx lr
	.size _start, .-_start

	.globl target
	.type target, %function
target:
	bx lr
	.size target, .-target

	.thumb
	.globl thumb_target
	.thumb_func
	.type thumb_target, %function
thumb_target:
	movw r0, :lower16:target
	movt r0, :upper16:target
	bx lr
	.size thumb_target, .-thumb_target

	.data
	.globl data
data:
	.word target
	.word thumb_target
	.word data + 4
	.word 0
"#;

#[test]
fn absolute_addressing_matches_lld() {
    let tools = require!();
    let dir = scratch("absolute");
    compile(tools, &dir, "absolute.s", ABSOLUTE_S, "absolute.o", &[]);
    let (lld, ours) = link_both(
        tools,
        &dir,
        "absolute",
        &["-static", "-e", "_start", "absolute.o"],
    );
    assert_same(tools, &dir, &lld, &ours);
}

#[test]
fn gc_sections_matches_lld() {
    let tools = require!();
    let dir = scratch("gc");
    compile(
        tools,
        &dir,
        "main.c",
        MAIN_C,
        "main.o",
        &["-ffunction-sections", "-fdata-sections", "-mthumb"],
    );
    compile(
        tools,
        &dir,
        "arm.c",
        ARM_C,
        "arm.o",
        &["-ffunction-sections", "-marm"],
    );
    compile(
        tools,
        &dir,
        "thumb.c",
        THUMB_C,
        "thumb.o",
        &["-ffunction-sections", "-mthumb"],
    );
    compile(tools, &dir, "far.s", FAR_S, "far.o", &[]);
    compile(tools, &dir, "fardef.s", FARDEF_S, "fardef.o", &[]);
    let (lld, ours) = link_both(
        tools,
        &dir,
        "gc",
        &[
            "-static",
            "--gc-sections",
            "-e",
            "_start",
            "main.o",
            "arm.o",
            "thumb.o",
            "far.o",
            "fardef.o",
        ],
    );
    assert_same(tools, &dir, &lld, &ours);
}
