//! s390x ELF tests (workstream W45).
//!
//! s390x is qld's first big-endian target, and GNU ld is the reference
//! (lld has no s390x port). Every test links the same objects with qld and
//! with `s390x-linux-gnu-ld` and compares what the two produced
//! *symbolically*: in each function the test's own sources define, every
//! address an instruction computes — a `brasl` target, a `larl` address, a
//! `lgrl` literal, a GOT-relative displacement off the GOT register — is
//! printed as the symbol it names, as the dynamic relocation that fills
//! the GOT slot it reads, or as the value the literal holds, so the
//! comparison holds wherever each linker placed things. qld follows GNU
//! ld's relaxation decisions (TLS to local-exec or initial-exec, GOT loads
//! turned into `larl`), so the relaxed code must match instruction for
//! instruction. Dynamic relocations are compared as `(type, symbol)`
//! multisets and `.dynamic` by tag.
//!
//! The programs also *run*, when `qemu-s390x` is installed: every binary
//! is executed and must print what its GNU ld counterpart prints,
//! including a qld executable with a GNU ld shared library and the
//! reverse.
//!
//! Tools: `s390x-linux-gnu-gcc`, `-g++`, `-ld`, `-objdump` and `-readelf`
//! (Debian/Ubuntu `gcc-s390x-linux-gnu` and `binutils-s390x-linux-gnu`),
//! and `qemu-s390x` (`qemu-user-static`) to run. A test prints `SKIPPED:`
//! and passes when a tool is missing, unless `QLD_REQUIRE_S390X_TOOLS=1`;
//! running is skipped the same way unless `QLD_REQUIRE_S390X_RUN=1`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;

const TRIPLE: &str = "s390x-linux-gnu";

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

fn required(var: &str) -> bool {
    std::env::var_os(var).is_some_and(|v| !v.is_empty() && v != "0")
}

struct Tools {
    cc: PathBuf,
    cxx: Option<PathBuf>,
    objdump: PathBuf,
    readelf: PathBuf,
    /// A directory whose `ld` is qld, for `gcc -B`.
    shim: PathBuf,
    /// `qemu-s390x`, when s390x programs can be run.
    qemu: Option<PathBuf>,
    /// `QEMU_LD_PREFIX`: the cross sysroot, when there is one.
    sysroot: Option<PathBuf>,
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

#[cfg(unix)]
fn link_shim(shim: &Path) -> Result<(), String> {
    let link = shim.join("ld");
    let _ = fs::remove_file(&link);
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_qld"), link).map_err(|e| e.to_string())
}

#[cfg(not(unix))]
fn link_shim(_: &Path) -> Result<(), String> {
    Err("not a Unix host".into())
}

fn probe() -> Result<Tools, String> {
    let cc = tool("QLD_S390X_CC", &[&format!("{TRIPLE}-gcc")]).ok_or("no s390x C compiler")?;
    let ld = tool(
        "QLD_S390X_LD",
        &[&format!("{TRIPLE}-ld.bfd"), &format!("{TRIPLE}-ld")],
    )
    .ok_or("no s390x GNU ld")?;
    let objdump =
        tool("QLD_S390X_OBJDUMP", &[&format!("{TRIPLE}-objdump")]).ok_or("no s390x objdump")?;
    let readelf = tool(
        "QLD_S390X_READELF",
        &[&format!("{TRIPLE}-readelf"), "readelf"],
    )
    .ok_or("no readelf")?;
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("s390x-probe");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    fs::write(dir.join("t.c"), "int main(void) { return 0; }\n").map_err(|e| e.to_string())?;
    let compiled = run(&dir, &cc, &["t.c", "-o", "t"]);
    if !compiled.status.success() {
        return Err(format!(
            "{} cannot link: {}",
            cc.display(),
            String::from_utf8_lossy(&compiled.stderr)
                .lines()
                .next()
                .unwrap_or("")
        ));
    }
    let emulations = run(&dir, &ld, &["-V"]);
    if !String::from_utf8_lossy(&emulations.stdout).contains("elf64_s390") {
        return Err(format!("{} has no elf64_s390 emulation", ld.display()));
    }
    let shim = Path::new(env!("CARGO_TARGET_TMPDIR")).join("s390x-ld");
    fs::create_dir_all(&shim).map_err(|e| e.to_string())?;
    link_shim(&shim)?;
    let cxx = tool("QLD_S390X_CXX", &[&format!("{TRIPLE}-g++")]).filter(|cxx| {
        fs::write(dir.join("t.cpp"), "int main() { return 0; }\n").is_ok()
            && run(&dir, cxx, &["t.cpp", "-o", "tx"]).status.success()
    });
    let sysroot = std::env::var_os("QEMU_LD_PREFIX")
        .map(PathBuf::from)
        .or_else(|| Some(PathBuf::from("/usr").join(TRIPLE)).filter(|p| p.is_dir()));
    Ok(Tools {
        cc,
        cxx,
        objdump,
        readelf,
        shim,
        qemu: find("qemu-s390x").or_else(|| find("qemu-s390x-static")),
        sysroot,
    })
}

fn tools() -> Option<&'static Tools> {
    static TOOLS: OnceLock<Result<Tools, String>> = OnceLock::new();
    match TOOLS.get_or_init(probe) {
        Ok(tools) => Some(tools),
        Err(why) => {
            assert!(
                !required("QLD_REQUIRE_S390X_TOOLS"),
                "QLD_REQUIRE_S390X_TOOLS is set but {why}"
            );
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
        .join("s390x-tests")
        .join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("gnu")).unwrap();
    fs::create_dir_all(dir.join("qld")).unwrap();
    dir
}

/// Writes `source` into `dir` and compiles it to `object`.
fn compile(tools: &Tools, dir: &Path, name: &str, source: &str, extra: &[&str]) {
    fs::write(dir.join(name), source).unwrap();
    let compiler = if name.ends_with(".cpp") {
        tools.cxx.clone().expect("a C++ compiler")
    } else {
        tools.cc.clone()
    };
    let object = format!("{}.o", name.rsplit_once('.').map_or(name, |(s, _)| s));
    let mut args = vec!["-O2", "-c", name, "-o", &object];
    args.extend_from_slice(extra);
    run_ok(dir, &compiler, &args);
}

/// Links `args` with both linkers through the compiler driver, into
/// `gnu/<output>` and `qld/<output>`.
fn drive_both(tools: &Tools, dir: &Path, cxx: bool, output: &str, args: &[&str]) {
    let driver = if cxx {
        tools.cxx.clone().expect("a C++ compiler")
    } else {
        tools.cc.clone()
    };
    for linker in ["gnu", "qld"] {
        let shim = format!("-B{}", tools.shim.display());
        let out = format!("{linker}/{output}");
        let library_path = format!("-L{linker}");
        let mut all: Vec<&str> = Vec::new();
        if linker == "qld" {
            all.push(&shim);
        }
        all.push(&library_path);
        all.extend_from_slice(args);
        all.extend_from_slice(&["-o", &out]);
        run_ok(dir, &driver, &all);
    }
}

/// Runs `<program>` under qemu with `LD_LIBRARY_PATH` set to `<libs>/`.
fn execute(tools: &Tools, dir: &Path, program: &str, libs: &str) -> Option<String> {
    let Some(qemu) = &tools.qemu else {
        assert!(
            !required("QLD_REQUIRE_S390X_RUN"),
            "QLD_REQUIRE_S390X_RUN is set but qemu-s390x is not installed"
        );
        return None;
    };
    let mut command = Command::new(qemu);
    command
        .arg(dir.join(program))
        .current_dir(dir)
        .env("LD_LIBRARY_PATH", dir.join(libs));
    if let Some(sysroot) = &tools.sysroot {
        command.env("QEMU_LD_PREFIX", sysroot);
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{program} (libraries from {libs}/) failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Runs a program linked by both linkers, each with its own libraries and
/// with the other's, and checks all print `expected`.
fn run_all(tools: &Tools, dir: &Path, program: &str, expected: &str) {
    for (exe, libs) in [
        ("gnu", "gnu"),
        ("qld", "qld"),
        ("qld", "gnu"),
        ("gnu", "qld"),
    ] {
        if let Some(out) = execute(tools, dir, &format!("{exe}/{program}"), libs) {
            assert_eq!(out, expected, "{exe}/{program} with {libs}/ libraries");
        }
    }
}

fn hex(text: &str) -> Option<u64> {
    u64::from_str_radix(text.trim_start_matches("0x"), 16).ok()
}

/// What the symbolizer knows about one output.
struct Image {
    data: Vec<u8>,
    /// `(address, file offset, size, name)`.
    sections: Vec<(u64, u64, u64, String)>,
    /// Defined symbols: `(address, size, name, is a function)`.
    symbols: Vec<(u64, u64, String, bool)>,
    /// Dynamic relocations by place: `TYPE(symbol)`.
    dynrel: BTreeMap<u64, String>,
    got_base: u64,
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
            let [name, _, addr, offset, size, ..] = fields.as_slice() else {
                continue;
            };
            let (Some(addr), Some(offset), Some(size)) = (hex(addr), hex(offset), hex(size)) else {
                continue;
            };
            if addr != 0 {
                sections.push((addr, offset, size, (*name).to_string()));
            }
        }
        let mut symbols = Vec::new();
        let mut got_base = None;
        for line in run_ok(dir, &tools.readelf, &["-sW", file]).lines() {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let [index, value, size, kind, _, _, ndx, name, ..] = fields.as_slice() else {
                continue;
            };
            if !index.ends_with(':') || *ndx == "UND" {
                continue;
            }
            let (Some(value), Ok(size)) = (hex(value), size.parse::<u64>()) else {
                continue;
            };
            let name = name.split('@').next().unwrap_or(name).to_string();
            if name == "_GLOBAL_OFFSET_TABLE_" {
                got_base = Some(value);
            }
            if *ndx != "ABS" && matches!(*kind, "FUNC" | "OBJECT" | "NOTYPE" | "IFUNC") {
                symbols.push((value, size, name, matches!(*kind, "FUNC" | "IFUNC")));
            }
        }
        symbols.sort();
        symbols.dedup_by(|a, b| a.0 == b.0 && a.2 == b.2);
        let got_base =
            got_base.unwrap_or_else(|| sections.iter().find(|s| s.3 == ".got").map_or(0, |s| s.0));
        let mut dynrel = BTreeMap::new();
        for line in run_ok(dir, &tools.readelf, &["-rW", file]).lines() {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let [place, _, kind, rest @ ..] = fields.as_slice() else {
                continue;
            };
            if place.len() != 16 || !kind.starts_with("R_390_") {
                continue;
            }
            let Some(place) = hex(place) else { continue };
            let symbol = rest.get(1).map_or("", |s| s.split('@').next().unwrap_or(s));
            let addend = rest.get(3).copied().unwrap_or("");
            let detail = if symbol.is_empty() && !addend.is_empty() {
                String::new()
            } else {
                symbol.to_string()
            };
            dynrel.insert(place, format!("{kind}({detail})"));
        }
        Self {
            data,
            sections,
            symbols,
            dynrel,
            got_base,
        }
    }

    fn section_of(&self, address: u64) -> Option<&(u64, u64, u64, String)> {
        self.sections
            .iter()
            .find(|s| s.0 <= address && address < s.0.saturating_add(s.2))
    }

    /// The eight bytes at `address`, big-endian.
    fn word(&self, address: u64) -> Option<u64> {
        let (addr, offset, _, name) = self.section_of(address)?;
        if name == ".bss" || name == ".tbss" {
            return None;
        }
        let at = usize::try_from(offset + (address - addr)).ok()?;
        let bytes = self.data.get(at..at.checked_add(8)?)?;
        Some(u64::from_be_bytes(bytes.try_into().ok()?))
    }

    /// The four bytes at `address`, big-endian.
    fn half(&self, address: u64) -> Option<u32> {
        let (addr, offset, _, _) = self.section_of(address)?;
        let at = usize::try_from(offset + (address - addr)).ok()?;
        let bytes = self.data.get(at..at.checked_add(4)?)?;
        Some(u32::from_be_bytes(bytes.try_into().ok()?))
    }

    /// `address` as the nearest symbol at or below it, or a section offset.
    fn name(&self, address: u64) -> String {
        let at = self.symbols.partition_point(|s| s.0 <= address);
        if let Some((value, size, name, _)) = at.checked_sub(1).and_then(|i| self.symbols.get(i))
            && (address - value < (*size).max(1) || *size == 0 && address - value < 0x100)
        {
            let offset = address - value;
            return if offset == 0 {
                name.clone()
            } else {
                format!("{name}+{offset:#x}")
            };
        }
        match self.section_of(address) {
            Some((addr, _, _, section)) => format!("{section}+{:#x}", address - addr),
            None => format!("{address:#x}"),
        }
    }

    /// The name of the section holding `address`.
    fn section_name(&self, address: u64) -> &str {
        self.section_of(address).map_or("", |s| s.3.as_str())
    }

    /// `address` symbolically: a GOT slot by the dynamic relocation that
    /// fills it or the address it holds, a PLT entry by the slot it jumps
    /// through, anything else by [`Self::name`].
    fn symbolize(&self, address: u64) -> String {
        if let Some(reloc) = self.dynrel.get(&address) {
            return format!("[{reloc}]");
        }
        match self.section_name(address) {
            ".got" | ".got.plt" => match self.word(address) {
                Some(value) => format!("GOT[&{}]", self.name(value)),
                None => "GOT[?]".to_string(),
            },
            ".plt" | ".iplt" => {
                // A PLT entry starts with `larl %r1,<its GOT slot>`, whose
                // operand counts halfwords from the entry, signed. Entries
                // are 32 bytes from the start of the section (the header
                // of a dynamic output's `.plt` is 32 bytes too).
                let start = self.section_of(address).map_or(address, |s| s.0);
                let entry = start + (address - start) / 32 * 32;
                match self.half(entry + 2) {
                    Some(halves) => {
                        let slot = entry.wrapping_add_signed(i64::from(halves as i32) * 2);
                        format!("PLT[{}]", self.symbolize(slot))
                    }
                    None => format!("PLT+{:#x}", address.wrapping_sub(entry)),
                }
            }
            _ => self.name(address),
        }
    }

    /// The value a literal pool entry at `address` holds: the dynamic
    /// relocation that fills it, the GOT entry it points at, an address it
    /// names, or the number. A GOT offset (what TLS literals hold) is
    /// resolved against the GOT pointer, since the two linkers place their
    /// GOT entries differently.
    fn literal(&self, address: u64, width: usize) -> String {
        if let Some(reloc) = self.dynrel.get(&address) {
            return format!("lit[{reloc}]");
        }
        if matches!(self.section_name(address), ".got" | ".got.plt" | ".plt") {
            return self.symbolize(address);
        }
        let value = if width == 4 {
            self.half(address).map(|v| v as i32 as i64 as u64)
        } else {
            self.word(address)
        };
        match value {
            // A literal pool word of position-independent code that holds
            // a GOT offset: what TLS accesses load before adding the GOT
            // pointer.
            Some(value)
                if width == 8
                    && value != 0
                    && value % 8 == 0
                    && matches!(
                        self.section_name(self.got_base.wrapping_add(value)),
                        ".got" | ".got.plt"
                    ) =>
            {
                format!(
                    "lit(GOT+{})",
                    self.symbolize(self.got_base.wrapping_add(value))
                )
            }
            Some(value) if self.section_of(value).is_some() => {
                format!("lit(&{})", self.name(value))
            }
            Some(value) => format!("lit({:#x})", value as i64),
            None => format!("lit@{}", self.name(address)),
        }
    }

    /// The NUL-terminated string at `address`, if it is one: string
    /// literals of the same program land at different offsets of
    /// `.rodata`, so they are compared by their contents.
    fn string(&self, address: u64) -> Option<String> {
        if !self.section_name(address).starts_with(".rodata") {
            return None;
        }
        let (addr, offset, size, _) = self.section_of(address)?;
        let at = usize::try_from(offset + (address - addr)).ok()?;
        let end = usize::try_from(offset + size).ok()?;
        let bytes = self.data.get(at..end)?;
        let length = bytes.iter().take(64).position(|b| *b == 0)?;
        let text = bytes.get(..length)?;
        if text.is_empty() || !text.iter().all(|b| b.is_ascii_graphic() || *b == b' ') {
            return None;
        }
        Some(format!("{:?}", String::from_utf8_lossy(text)))
    }

    /// The instructions of function `name`, symbolized.
    fn function(&self, tools: &Tools, dir: &Path, file: &str, name: &str) -> Vec<String> {
        let (value, size, ..) = self
            .symbols
            .iter()
            .find(|s| s.2 == name && s.3)
            .unwrap_or_else(|| panic!("{file} has no function {name}"));
        let start = format!("--start-address={value:#x}");
        let stop = format!("--stop-address={:#x}", value + size.max(&1));
        let listing = run_ok(
            dir,
            &tools.objdump,
            &["-d", "--no-show-raw-insn", &start, &stop, file],
        );
        let mut out = Vec::new();
        // The register holding the GOT pointer, once a `larl` loaded it.
        let mut got_register: Option<String> = None;
        for line in listing.lines() {
            let Some((addr, text)) = line.trim_start().split_once(":\t") else {
                continue;
            };
            if hex(addr).is_none() {
                continue;
            }
            let text = text.split(" <").next().unwrap_or(text).trim();
            out.push(self.rewrite(text, &mut got_register));
        }
        out
    }

    /// Rewrites one instruction: PC-relative operands become symbols, and
    /// displacements off the GOT register become what they address.
    fn rewrite(&self, text: &str, got_register: &mut Option<String>) -> String {
        let (mnemonic, operands) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
        let operands = operands.trim();
        let (head, target) = operands.rsplit_once(',').unwrap_or(("", operands));
        // PC-relative operands: `larl`, the relative loads and stores, and
        // the long branches. objdump prints them as a bare hex address.
        if is_pc_relative(mnemonic)
            && let Some(address) = u64::from_str_radix(target, 16).ok().filter(|a| *a > 0xff)
        {
            let symbolic = if mnemonic == "larl" {
                if address == self.got_base {
                    *got_register = head.strip_prefix("%").map(|r| format!("%{r}"));
                    "GOT".to_string()
                } else {
                    self.string(address)
                        .unwrap_or_else(|| self.symbolize(address))
                }
            } else if mnemonic.ends_with("rl") {
                self.literal(address, literal_width(mnemonic))
            } else {
                self.symbolize(address)
            };
            return format!("{mnemonic} {head},{symbolic}")
                .replace(" ,", " ")
                .to_string();
        }
        // `D(%rN)` and `D(%rX,%rN)` while `%rN` holds the GOT pointer.
        if let Some(got) = got_register.clone()
            && let Some(rewritten) = self.rewrite_got_operand(operands, &got)
        {
            return format!("{mnemonic} {rewritten}");
        }
        // An instruction that writes the GOT register ends its GOT role.
        if let Some(got) = got_register.clone()
            && operands.starts_with(&format!("{got},"))
            && !mnemonic.starts_with("st")
            && !mnemonic.starts_with('c')
        {
            *got_register = None;
        }
        text.to_string()
    }

    /// `D(%rX,%GOT)` or `D(%GOT)` as what it addresses.
    fn rewrite_got_operand(&self, operands: &str, got: &str) -> Option<String> {
        let (head, memory) = operands.rsplit_once(',')?;
        let (displacement, rest) = memory.split_once('(')?;
        let registers = rest.strip_suffix(')')?;
        let uses_got = registers
            .split(',')
            .any(|register| register == got || register == "%r0");
        if !registers.split(',').any(|register| register == got) || !uses_got {
            return None;
        }
        let displacement: i64 = displacement.parse().ok()?;
        let address = self.got_base.wrapping_add_signed(displacement);
        let index = registers
            .split(',')
            .filter(|r| *r != got)
            .collect::<Vec<_>>()
            .join(",");
        let slot = self.symbolize(address);
        Some(if index.is_empty() || index == "%r0" {
            format!("{head},{slot}(%GOT)")
        } else {
            format!("{head},{slot}({index},%GOT)")
        })
    }

    /// The dynamic relocations as sorted `TYPE(symbol)` strings.
    fn dynamic_relocations(&self) -> Vec<String> {
        let mut all: Vec<String> = self.dynrel.values().cloned().collect();
        all.sort();
        all
    }

    /// The same, without the `GLOB_DAT` relocations of symbols `defined`
    /// has: GNU ld binds a GOT entry of an exported symbol through the
    /// dynamic linker even when the output defines it (glibc's `crt1.o`
    /// reaches `main` that way), while qld, like lld, binds it at link
    /// time.
    fn dynamic_relocations_binding(&self, defined: &Image) -> Vec<String> {
        self.dynamic_relocations()
            .into_iter()
            .filter(|reloc| {
                let Some(symbol) = reloc
                    .strip_prefix("R_390_GLOB_DAT(")
                    .and_then(|s| s.strip_suffix(')'))
                else {
                    return true;
                };
                !defined.symbols.iter().any(|s| s.2 == symbol)
            })
            .collect()
    }
}

/// How many bytes a relative load or store of `mnemonic` touches.
fn literal_width(mnemonic: &str) -> usize {
    match mnemonic {
        "lgrl" | "stgrl" | "cgrl" | "clgrl" => 8,
        "lhrl" | "llhrl" | "sthrl" | "chrl" | "clhrl" => 2,
        _ => 4,
    }
}

/// Whether the last operand of `mnemonic` is a PC-relative target, which
/// objdump prints as an address.
fn is_pc_relative(mnemonic: &str) -> bool {
    const PREFIXES: [&str; 9] = [
        "bras", "brc", "bra", "bpp", "bprp", "brct", "brxh", "brxle", "loop",
    ];
    // The compare-and-branch family: `crj`, `cgij`, `clgrj`, … all end
    // with `j` plus a condition, and `j`/`jg` are the plain branches.
    let compare_and_branch = mnemonic.starts_with('c')
        && mnemonic
            .trim_end_matches(|c: char| c.is_ascii_alphabetic())
            .is_empty()
        && mnemonic.contains('j');
    mnemonic.ends_with("rl")
        || mnemonic.starts_with('j')
        || compare_and_branch
        || PREFIXES.iter().any(|p| mnemonic.starts_with(p))
}

/// The `.dynamic` tags of `file`, sorted, without values.
fn dynamic_tags(tools: &Tools, dir: &Path, file: &str) -> Vec<String> {
    let mut tags: Vec<String> = run_ok(dir, &tools.readelf, &["-dW", file])
        .lines()
        .filter_map(|line| {
            let (_, rest) = line.trim().split_once('(')?;
            let (tag, _) = rest.split_once(')')?;
            // GNU ld says BIND_NOW with a DT_BIND_NOW entry, qld with the
            // DT_FLAGS bit; both also set DT_FLAGS_1's.
            (!matches!(tag, "BIND_NOW" | "FLAGS")).then(|| tag.to_string())
        })
        .collect();
    tags.sort();
    tags.dedup();
    tags
}

/// Compares the functions `names` of `file` as both linkers wrote them.
fn compare_functions(tools: &Tools, dir: &Path, file: &str, names: &[&str]) -> Image {
    let gnu_file = format!("gnu/{file}");
    let qld_file = format!("qld/{file}");
    let gnu = Image::load(tools, dir, &gnu_file);
    let ours = Image::load(tools, dir, &qld_file);
    for name in names {
        let theirs = gnu.function(tools, dir, &gnu_file, name);
        let mine = ours.function(tools, dir, &qld_file, name);
        assert_eq!(
            theirs.join("\n"),
            mine.join("\n"),
            "{file}: {name} differs (GNU ld left, qld right)"
        );
    }
    assert_eq!(
        gnu.dynamic_relocations_binding(&gnu),
        ours.dynamic_relocations_binding(&gnu),
        "{file}: dynamic relocations differ"
    );
    assert_eq!(
        dynamic_tags(tools, dir, &gnu_file),
        dynamic_tags(tools, dir, &qld_file),
        "{file}: .dynamic tags differ"
    );
    ours
}

const CALLS: &str = r#"
#include <stdio.h>

int counter_qld = 7;
const char message_qld[] = "message";
static int local_qld[4] = {1, 2, 3, 4};

extern int other_qld(int);

__attribute__((noinline)) int add_local_qld(int n) {
    return local_qld[n & 3] + counter_qld;
}

__attribute__((noinline)) const char *text_qld(void) {
    return message_qld;
}

__attribute__((noinline)) int call_chain_qld(int n) {
    return other_qld(add_local_qld(n)) + (int)text_qld()[0];
}

int main(void) {
    printf("calls %d %s\n", call_chain_qld(2), text_qld());
    return 0;
}
"#;

const OTHER: &str = r#"
extern int counter_qld;
__attribute__((noinline)) int other_qld(int n) {
    return n * 2 + counter_qld;
}
"#;

#[test]
fn calls_and_addresses_match() {
    let tools = require!();
    let dir = scratch("calls");
    for (name, flags) in [("pic", "-fPIC"), ("nopic", "-fno-pic")] {
        compile(tools, &dir, &format!("calls-{name}.c"), CALLS, &[flags]);
        compile(tools, &dir, &format!("other-{name}.c"), OTHER, &[flags]);
    }
    let functions = ["add_local_qld", "text_qld", "call_chain_qld"];
    drive_both(
        tools,
        &dir,
        false,
        "static",
        &["-static", "calls-nopic.o", "other-nopic.o"],
    );
    compare_functions(tools, &dir, "static", &functions);
    drive_both(
        tools,
        &dir,
        false,
        "pie",
        &["-pie", "calls-pic.o", "other-pic.o"],
    );
    compare_functions(tools, &dir, "pie", &functions);
    drive_both(
        tools,
        &dir,
        false,
        "nopie",
        &["-no-pie", "calls-nopic.o", "other-nopic.o"],
    );
    compare_functions(tools, &dir, "nopie", &functions);
    for program in ["static", "pie", "nopie"] {
        run_all(tools, &dir, program, "calls 136 message\n");
    }
}

const LIBRARY: &str = r#"
#include <stdio.h>

__thread int lib_tls_qld = 5;
int lib_data_qld = 11;

extern int exe_callback_qld(int n);

int lib_call_qld(int n) {
    return exe_callback_qld(n) + lib_data_qld + lib_tls_qld;
}

void lib_print_qld(void) {
    printf("lib %d\n", lib_call_qld(1));
}
"#;

const DYNAMIC_MAIN: &str = r#"
#include <stdio.h>

extern int lib_data_qld;
extern __thread int lib_tls_qld;
int lib_call_qld(int n);
void lib_print_qld(void);

int exe_callback_qld(int n) {
    return n * 10;
}

int main(void) {
    lib_print_qld();
    printf("exe %d %d %d\n", lib_call_qld(2), lib_data_qld, lib_tls_qld);
    return 0;
}
"#;

#[test]
fn dynamic_linking_matches() {
    let tools = require!();
    let dir = scratch("dynamic");
    compile(tools, &dir, "library.c", LIBRARY, &["-fPIC"]);
    compile(tools, &dir, "dynmain.c", DYNAMIC_MAIN, &["-fPIE"]);
    drive_both(
        tools,
        &dir,
        false,
        "libdemo.so",
        &["-shared", "-Wl,-soname,libdemo.so", "library.o"],
    );
    compare_functions(
        tools,
        &dir,
        "libdemo.so",
        &["lib_call_qld", "lib_print_qld"],
    );
    drive_both(
        tools,
        &dir,
        false,
        "exe",
        &[
            "-pie",
            "dynmain.o",
            "-ldemo",
            "-Wl,-rpath,$ORIGIN",
            "-Wl,--enable-new-dtags",
            "-rdynamic",
        ],
    );
    let image = compare_functions(tools, &dir, "exe", &["main", "exe_callback_qld"]);
    // The library's TLS variable is initial-exec in the executable, which
    // needs one `R_390_TLS_TPOFF` GOT entry, and the calls into the
    // library go through the PLT.
    let relocations = image.dynamic_relocations();
    assert_eq!(
        relocations
            .iter()
            .filter(|r| r.starts_with("R_390_TLS_TPOFF"))
            .count(),
        1,
        "{relocations:?}"
    );
    assert!(
        relocations
            .iter()
            .any(|r| r.starts_with("R_390_JMP_SLOT(lib_call_qld)")),
        "{relocations:?}"
    );
    run_all(tools, &dir, "exe", "lib 26\nexe 36 11 5\n");
}

const TLS: &str = r#"
#include <stdio.h>

__thread int gd_qld __attribute__((tls_model("global-dynamic")));
static __thread int ld_a_qld __attribute__((tls_model("local-dynamic")));
static __thread int ld_b_qld __attribute__((tls_model("local-dynamic")));
__thread int ie_qld __attribute__((tls_model("initial-exec")));
#ifdef EXECUTABLE
__thread int le_qld __attribute__((tls_model("local-exec")));
#else
__thread int le_qld;
#endif

__attribute__((noinline)) void set_tls_qld(int n) {
    gd_qld = n;
    ld_a_qld = n + 1;
    ld_b_qld = n + 2;
    ie_qld = n + 3;
    le_qld = n + 4;
}

__attribute__((noinline)) int sum_tls_qld(void) {
    return gd_qld + ld_a_qld + ld_b_qld + ie_qld + le_qld;
}

__attribute__((noinline)) int *gd_address_qld(void) {
    return &gd_qld;
}

#ifdef EXECUTABLE
int main(void) {
    set_tls_qld(1);
    printf("tls %d %d\n", sum_tls_qld(), *gd_address_qld());
    return 0;
}
#endif
"#;

#[test]
fn tls_models_match() {
    let tools = require!();
    let dir = scratch("tls");
    // -fpic and -fPIC differ on s390x: small PIC reaches the GOT with a
    // 20-bit displacement off %r12, large PIC with `larl`/`lgrl`.
    for (name, flags) in [("small", "-fpic"), ("large", "-fPIC")] {
        compile(
            tools,
            &dir,
            &format!("tls-{name}.c"),
            TLS,
            &[flags, "-DEXECUTABLE"],
        );
        compile(tools, &dir, &format!("tlslib-{name}.c"), TLS, &[flags]);
    }
    let functions = ["set_tls_qld", "sum_tls_qld", "gd_address_qld"];
    for name in ["small", "large"] {
        let object = format!("tls-{name}.o");
        let library_object = format!("tlslib-{name}.o");
        drive_both(
            tools,
            &dir,
            false,
            &format!("{name}-static"),
            &["-static", &object],
        );
        compare_functions(tools, &dir, &format!("{name}-static"), &functions);
        drive_both(
            tools,
            &dir,
            false,
            &format!("{name}-pie"),
            &["-pie", &object],
        );
        let image = compare_functions(tools, &dir, &format!("{name}-pie"), &functions);
        // Everything is in the executable, so every model relaxed to
        // local-exec: no TLS dynamic relocation is left.
        assert!(
            !image
                .dynamic_relocations()
                .iter()
                .any(|r| r.contains("R_390_TLS_")),
            "{name}: {:?}",
            image.dynamic_relocations()
        );
        drive_both(
            tools,
            &dir,
            false,
            &format!("{name}-shared.so"),
            &["-shared", &library_object],
        );
        let image = compare_functions(tools, &dir, &format!("{name}-shared.so"), &functions);
        // A shared object keeps the dynamic models: the module/offset
        // pairs of its two general-dynamic variables and the module of
        // the local-dynamic base.
        let relocations = image.dynamic_relocations();
        assert_eq!(
            relocations
                .iter()
                .filter(|r| r.starts_with("R_390_TLS_DTPMOD"))
                .count(),
            3,
            "{name}: {relocations:?}"
        );
        for program in ["static", "pie"] {
            run_all(tools, &dir, &format!("{name}-{program}"), "tls 15 1\n");
        }
    }
}

const IFUNC: &str = r#"
#include <stdio.h>

static int impl_one_qld(void) { return 1; }
static int impl_two_qld(void) { return 2; }

static void *resolve_pick_qld(void) {
    return (void *)(sizeof(void *) == 8 ? impl_two_qld : impl_one_qld);
}

int pick_qld(void) __attribute__((ifunc("resolve_pick_qld")));

int (*pointer_qld)(void) = pick_qld;

__attribute__((noinline)) int use_pick_qld(void) {
    return pick_qld() + pointer_qld() + (pointer_qld == pick_qld);
}

int main(void) {
    printf("ifunc %d\n", use_pick_qld());
    return 0;
}
"#;

#[test]
fn ifunc_matches() {
    let tools = require!();
    let dir = scratch("ifunc");
    compile(tools, &dir, "ifunc.c", IFUNC, &["-fPIC"]);
    drive_both(tools, &dir, false, "static", &["-static", "ifunc.o"]);
    let image = compare_functions(tools, &dir, "static", &["use_pick_qld"]);
    assert!(
        image
            .dynamic_relocations()
            .iter()
            .any(|r| r.starts_with("R_390_IRELATIVE")),
        "static: {:?}",
        image.dynamic_relocations()
    );
    drive_both(tools, &dir, false, "pie", &["-pie", "ifunc.o"]);
    let image = Image::load(tools, &dir, "qld/pie");
    assert!(
        image
            .dynamic_relocations()
            .iter()
            .any(|r| r.starts_with("R_390_IRELATIVE")),
        "pie: {:?}",
        image.dynamic_relocations()
    );
    for name in ["static", "pie"] {
        run_all(tools, &dir, name, "ifunc 5\n");
    }
}

const EXCEPTIONS: &str = r#"
#include <cstdio>
#include <stdexcept>
#include <string>

__attribute__((noinline)) int thrower_qld(int n) {
    if (n > 2) {
        throw std::runtime_error("boom " + std::to_string(n));
    }
    return n;
}

int main() {
    try {
        thrower_qld(5);
    } catch (const std::runtime_error &e) {
        std::printf("caught %s\n", e.what());
        return 0;
    }
    return 1;
}
"#;

#[test]
fn cxx_exceptions_match() {
    let tools = require!();
    if tools.cxx.is_none() {
        assert!(
            !required("QLD_REQUIRE_S390X_TOOLS"),
            "QLD_REQUIRE_S390X_TOOLS is set but there is no s390x C++ compiler"
        );
        println!("SKIPPED: no s390x C++ compiler");
        return;
    }
    let dir = scratch("cxx");
    compile(tools, &dir, "throw.cpp", EXCEPTIONS, &["-fPIE"]);
    drive_both(tools, &dir, true, "exe", &["-pie", "throw.o"]);
    compare_functions(tools, &dir, "exe", &["_Z11thrower_qldi"]);
    run_all(tools, &dir, "exe", "caught boom 5\n");
}

#[test]
fn hash_table_and_pgste_match() {
    let tools = require!();
    let dir = scratch("options");
    compile(tools, &dir, "library.c", LIBRARY, &["-fPIC"]);
    drive_both(
        tools,
        &dir,
        false,
        "libhash.so",
        &["-shared", "library.o", "-Wl,--hash-style=both"],
    );
    // The s390x ABI's `.hash` has eight-byte entries.
    for linker in ["gnu", "qld"] {
        let file = format!("{linker}/libhash.so");
        let headers = run_ok(&dir, &tools.readelf, &["-SW", &file]);
        let line = headers
            .lines()
            .find(|l| l.contains(" .hash "))
            .unwrap_or_else(|| panic!("{file} has no .hash"));
        let fields: Vec<&str> = line.split_whitespace().collect();
        let entsize = fields
            .iter()
            .position(|f| *f == ".hash")
            .and_then(|i| fields.get(i + 5))
            .copied()
            .unwrap_or("");
        assert_eq!(entsize, "08", "{file}: {line}");
        let image = Image::load(tools, &dir, &file);
        let hash = image
            .sections
            .iter()
            .find(|s| s.3 == ".hash")
            .expect(".hash");
        // The first two words are nbucket and nchain, each eight bytes.
        let nbucket = image.word(hash.0).unwrap();
        let nchain = image.word(hash.0 + 8).unwrap();
        assert!(nbucket > 0 && nchain > 0, "{file}: {nbucket} {nchain}");
        assert_eq!(
            hash.2,
            (2 + nbucket + nchain) * 8,
            "{file}: .hash size for {nbucket} buckets and {nchain} chains"
        );
    }
    // `--s390-pgste` adds an empty PT_S390_PGSTE segment.
    drive_both(
        tools,
        &dir,
        false,
        "libpgste.so",
        &["-shared", "library.o", "-Wl,--s390-pgste"],
    );
    for linker in ["gnu", "qld"] {
        let file = format!("{linker}/libpgste.so");
        let segments = run_ok(&dir, &tools.readelf, &["-lW", &file]);
        assert!(
            segments.contains("S390_PGSTE") || segments.contains("0x70000000"),
            "{file}: no PT_S390_PGSTE:\n{segments}"
        );
    }
}

/// A GOT load of a symbol defined in the same position-independent output
/// becomes `larl`, as GNU ld does, and its GOT entry stays.
#[test]
fn got_loads_relax_to_larl() {
    let tools = require!();
    let dir = scratch("relax");
    compile(tools, &dir, "calls.c", CALLS, &["-fPIC"]);
    compile(tools, &dir, "other.c", OTHER, &["-fPIC"]);
    drive_both(tools, &dir, false, "pie", &["-pie", "calls.o", "other.o"]);
    let image = compare_functions(tools, &dir, "pie", &["add_local_qld", "text_qld"]);
    let text = image.function(tools, &dir, "qld/pie", "text_qld");
    assert!(
        text.iter().any(|i| i.starts_with("larl")),
        "the address of a local symbol should be computed with larl: {text:?}"
    );
    assert!(
        !text.iter().any(|i| i.starts_with("lgrl")),
        "the GOT load should have been relaxed: {text:?}"
    );
}
