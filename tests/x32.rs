//! x32 ELF tests (workstream W43).
//!
//! Every test links the same objects with qld and with GNU ld
//! (`-m elf32_x86_64`) and compares what the two produced *symbolically*: in
//! each function the test's own sources define, every address an
//! instruction computes — a branch target, a `%rip`-relative operand, an
//! absolute address — is printed as `symbol+offset`, and a GOT slot by the
//! dynamic relocation that fills it or the 64-bit value it holds, so the
//! comparison holds wherever each linker placed things. qld follows GNU ld's
//! relaxation decisions, so relaxed code must match instruction for
//! instruction. The PLT sections are compared the same way, dynamic
//! relocations as `(type, symbol)` multisets and `.dynamic` by tag. When
//! `ld.lld` is available, the freestanding programs are also linked with it
//! and compared the same way, apart from the relaxations lld does not make.
//!
//! There is usually no x32 C library, so most tests are freestanding: a
//! minimal runtime (`start.c`) sets up TLS, applies a static PIE's own
//! relocations and IFUNCs, and makes system calls itself. The programs run
//! when the kernel executes x32 binaries (`CONFIG_X86_X32_ABI`), and must
//! print what GNU ld's do. With an x32 C library (Debian/Ubuntu
//! `gcc-multilib`, which brings `libc6-dev-x32`), C and C++ programs against
//! glibc are linked and compared too, and run with the dynamic linker.
//!
//! Tools: `gcc -mx32`, GNU `ld` with the `elf32_x86_64` emulation,
//! `objdump` and `readelf`; `ld.lld` from `QLD_LLD` or `PATH`. A test prints
//! `SKIPPED:` and passes when one is missing, unless
//! `QLD_REQUIRE_X32_TOOLS=1` (compiler and GNU ld), `QLD_REQUIRE_X32_LIBC=1`
//! (the x32 C library), `QLD_REQUIRE_X32_CXX=1` (the C++ one),
//! `QLD_REQUIRE_X32_LLD=1`, or, for running, `QLD_REQUIRE_X32_RUN=1`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;

const DATA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/x32");

/// The x32 dynamic linker, as glibc installs it.
const INTERPRETER: &str = "/libx32/ld-linux-x32.so.2";

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

fn required(var: &str) -> bool {
    std::env::var_os(var).is_some_and(|v| !v.is_empty() && v != "0")
}

struct Tools {
    cc: PathBuf,
    cxx: Option<PathBuf>,
    ld: PathBuf,
    lld: Option<PathBuf>,
    objdump: PathBuf,
    readelf: PathBuf,
    /// A directory whose `ld` is qld, for `gcc -B`.
    shim: PathBuf,
    /// Whether `gcc -mx32` links against an x32 C library.
    libc: bool,
    /// Whether x32 programs run on this host.
    runs: bool,
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

fn probe() -> Result<Tools, String> {
    let cc = in_path("gcc").ok_or("no gcc")?;
    let ld = in_path("ld")
        .or_else(|| in_path("ld.bfd"))
        .ok_or("no GNU ld")?;
    let objdump = in_path("objdump").ok_or("no objdump")?;
    let readelf = in_path("readelf").ok_or("no readelf")?;
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("x32-probe");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    // A program that exits with status 0 through the x32 `exit` system
    // call, which needs no C library.
    fs::write(
        dir.join("exit.c"),
        "void _start(void) { __asm__ volatile(\"syscall\" :: \"a\"(0x4000003c), \"D\"(0)); }\n",
    )
    .map_err(|e| e.to_string())?;
    let compiled = run(
        &dir,
        &cc,
        &[
            "-mx32",
            "-O2",
            "-ffreestanding",
            "-c",
            "exit.c",
            "-o",
            "exit.o",
        ],
    );
    if !compiled.status.success() {
        return Err(format!(
            "gcc -mx32 cannot compile: {}",
            String::from_utf8_lossy(&compiled.stderr)
                .lines()
                .next()
                .unwrap_or("")
        ));
    }
    let emulations = run(&dir, &ld, &["-V"]);
    if !String::from_utf8_lossy(&emulations.stdout).contains("elf32_x86_64") {
        return Err(format!("{} has no elf32_x86_64 emulation", ld.display()));
    }
    let linked = run(
        &dir,
        &ld,
        &["-m", "elf32_x86_64", "-static", "exit.o", "-o", "exit"],
    );
    if !linked.status.success() {
        return Err("ld -m elf32_x86_64 cannot link".into());
    }
    let runs = Command::new(dir.join("exit"))
        .output()
        .is_ok_and(|o| o.status.success());
    fs::write(dir.join("t.c"), "int main(void) { return 0; }\n").map_err(|e| e.to_string())?;
    let libc = run(&dir, &cc, &["-mx32", "t.c", "-o", "t"])
        .status
        .success();
    let shim = Path::new(env!("CARGO_TARGET_TMPDIR")).join("x32-ld");
    let _ = fs::remove_dir_all(&shim);
    fs::create_dir_all(&shim).map_err(|e| e.to_string())?;
    link_shim(&shim)?;
    let cxx = in_path("g++").filter(|cxx| {
        libc && fs::write(dir.join("t.cpp"), "int main() { return 0; }\n").is_ok()
            && run(&dir, cxx, &["-mx32", "t.cpp", "-o", "tx"])
                .status
                .success()
    });
    let lld = tool("QLD_LLD", "ld.lld");
    Ok(Tools {
        cc,
        cxx,
        ld,
        lld,
        objdump,
        readelf,
        shim,
        libc,
        runs,
    })
}

/// Makes `shim/ld` run qld, for `gcc -B shim`.
#[cfg(unix)]
fn link_shim(shim: &Path) -> Result<(), String> {
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_qld"), shim.join("ld"))
        .map_err(|e| e.to_string())
}

#[cfg(not(unix))]
fn link_shim(_: &Path) -> Result<(), String> {
    Err("not a Unix host".into())
}

fn tools() -> Option<&'static Tools> {
    static TOOLS: OnceLock<Result<Tools, String>> = OnceLock::new();
    match TOOLS.get_or_init(probe) {
        Ok(tools) => Some(tools),
        Err(why) => {
            assert!(
                !required("QLD_REQUIRE_X32_TOOLS"),
                "QLD_REQUIRE_X32_TOOLS is set but {why}"
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

/// The tools, if the x32 C library is there too.
fn require_libc() -> Option<&'static Tools> {
    let tools = tools()?;
    if !tools.libc {
        assert!(
            !required("QLD_REQUIRE_X32_LIBC"),
            "QLD_REQUIRE_X32_LIBC is set but gcc -mx32 cannot link a C program"
        );
        println!("SKIPPED: no x32 C library");
        return None;
    }
    Some(tools)
}

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("x32-tests")
        .join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("gnu")).unwrap();
    fs::create_dir_all(dir.join("qld")).unwrap();
    fs::create_dir_all(dir.join("lld")).unwrap();
    dir
}

/// Compiles `source` from the data directory into `object` in `dir`.
fn compile(tools: &Tools, dir: &Path, source: &str, object: &str, extra: &[&str]) {
    let compiler = if source.ends_with(".cpp") {
        match &tools.cxx {
            Some(cxx) => cxx.clone(),
            None => return,
        }
    } else {
        tools.cc.clone()
    };
    let path = format!("{DATA}/{source}");
    let mut args = vec!["-mx32", "-O2", "-c", path.as_str(), "-o", object];
    args.extend_from_slice(extra);
    run_ok(dir, &compiler, &args);
}

/// Compiles freestanding `source`: no C library, no stack protector, no
/// calls the compiler would expect a C library to satisfy.
fn compile_freestanding(tools: &Tools, dir: &Path, source: &str, object: &str, extra: &[&str]) {
    let mut args = vec![
        "-ffreestanding",
        "-fno-builtin",
        "-fno-stack-protector",
        "-fcf-protection=none",
        "-fno-asynchronous-unwind-tables",
        "-fno-tree-loop-distribute-patterns",
    ];
    args.extend_from_slice(extra);
    compile(tools, dir, source, object, &args);
}

/// Assembles `source` from the data directory with `defines`.
fn assemble(tools: &Tools, dir: &Path, source: &str, object: &str, defines: &[&str]) {
    let path = format!("{DATA}/{source}");
    let mut args = vec!["-mx32", "-c", path.as_str(), "-o", object];
    let flags: Vec<String> = defines
        .iter()
        .map(|d| format!("-Wa,--defsym,{d}=1"))
        .collect();
    args.extend(flags.iter().map(String::as_str));
    run_ok(dir, &tools.cc, &args);
}

/// The compiler driver linking `objects` into `<linker>/<output>`, for
/// both linkers.
fn drive_both(tools: &Tools, dir: &Path, cxx: bool, output: &str, args: &[&str]) {
    let driver = if cxx {
        tools.cxx.clone().unwrap_or_else(|| tools.cc.clone())
    } else {
        tools.cc.clone()
    };
    for linker in ["gnu", "qld"] {
        let shim = format!("-B{}", tools.shim.display());
        let out = format!("{linker}/{output}");
        let mut all = vec!["-mx32"];
        if linker == "qld" {
            all.push(&shim);
        }
        let library_path = format!("-L{linker}");
        all.push(&library_path);
        all.extend_from_slice(args);
        all.extend_from_slice(&["-o", &out]);
        run_ok(dir, &driver, &all);
    }
}

/// `ld -m elf32_x86_64` and qld (and lld, if `lld`) on the same arguments.
/// `-L<linker>` finds each linker's own libraries.
///
/// The hash style is explicit: the default is a configure-time choice of
/// GNU ld's.
fn ld_all(tools: &Tools, dir: &Path, output: &str, args: &[&str], lld: bool) {
    let mut linkers = vec![
        ("gnu", tools.ld.clone()),
        ("qld", PathBuf::from(env!("CARGO_BIN_EXE_qld"))),
    ];
    if lld && let Some(path) = &tools.lld {
        linkers.push(("lld", path.clone()));
    }
    for (linker, program) in linkers {
        let out = format!("{linker}/{output}");
        let library_path = format!("-L{linker}");
        let mut all = vec!["-m", "elf32_x86_64", "--hash-style=both", &library_path];
        all.extend_from_slice(args);
        all.extend_from_slice(&["-o", &out]);
        run_ok(dir, &program, &all);
    }
}

fn ld_both(tools: &Tools, dir: &Path, output: &str, args: &[&str]) {
    ld_all(tools, dir, output, args, false);
}

/// Whether `lld/<file>` should be compared: lld is there, or its absence
/// is reported.
fn lld_available(tools: &Tools) -> bool {
    if tools.lld.is_some() {
        return true;
    }
    assert!(
        !required("QLD_REQUIRE_X32_LLD"),
        "QLD_REQUIRE_X32_LLD is set but there is no ld.lld (QLD_LLD or PATH)"
    );
    println!("SKIPPED: no ld.lld; comparing with GNU ld only");
    false
}

/// Whether x32 programs run here, or their not running is reported.
fn can_run(tools: &Tools) -> bool {
    if tools.runs {
        return true;
    }
    assert!(
        !required("QLD_REQUIRE_X32_RUN"),
        "QLD_REQUIRE_X32_RUN is set but x32 programs do not run here \
         (a kernel without CONFIG_X86_X32_ABI)"
    );
    false
}

/// Runs `<linker>/<program>` with `LD_LIBRARY_PATH` set to `<libs>/`, and
/// returns its output.
fn execute(tools: &Tools, dir: &Path, program: &str, libs: &str) -> Option<String> {
    if !can_run(tools) {
        return None;
    }
    let output = Command::new(dir.join(program))
        .current_dir(dir)
        .env("LD_LIBRARY_PATH", dir.join(libs))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{program} (libraries from {libs}/) failed: {:?} {}{}",
        output.status,
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

/// One output section, as `readelf -S` prints it.
#[derive(PartialEq, Eq)]
struct Section {
    addr: u64,
    offset: u64,
    size: u64,
    entsize: u64,
    name: String,
    kind: String,
}

/// What the symbolizer knows about one output.
struct Image {
    data: Vec<u8>,
    /// `(address, file offset, size, name, type)`.
    sections: Vec<Section>,
    /// Defined symbols: `(address, size, name, is a function)`.
    symbols: Vec<(u64, u64, String, bool)>,
    /// Dynamic relocations by place: `TYPE(symbol)`.
    dynrel: BTreeMap<u64, String>,
    /// The addends of the relative relocations, by place: what the word
    /// holds once the dynamic linker has added the load address.
    relative: BTreeMap<u64, u64>,
    /// The index in `.rela.plt` of each PLT relocation, by place: which
    /// PLT entry the slot belongs to.
    plt_index: BTreeMap<u64, u64>,
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
            let [name, kind, addr, offset, size, entsize, ..] = fields.as_slice() else {
                continue;
            };
            let (Some(addr), Some(offset), Some(size), Some(entsize)) =
                (hex(addr), hex(offset), hex(size), hex(entsize))
            else {
                continue;
            };
            if addr != 0 {
                sections.push(Section {
                    addr,
                    offset,
                    size,
                    entsize,
                    name: (*name).to_string(),
                    kind: (*kind).to_string(),
                });
            }
        }
        let mut symbols = Vec::new();
        for line in run_ok(dir, &tools.readelf, &["-sW", file]).lines() {
            // `readelf` names `STT_GNU_IFUNC` only in a file whose OS/ABI
            // is GNU, which qld does not set for a shared object whose
            // IFUNC has no stub of its own (see the report).
            let line = line.replace("<OS specific>: 10", "IFUNC");
            let fields: Vec<&str> = line.split_whitespace().collect();
            let [index, value, size, kind, _, _, ndx, name, ..] = fields.as_slice() else {
                continue;
            };
            if !index.ends_with(':') || *ndx == "UND" || *ndx == "ABS" {
                continue;
            }
            let (Some(value), Ok(size)) = (hex(value), size.parse::<u64>()) else {
                continue;
            };
            let name = name.split('@').next().unwrap_or(name).to_string();
            if matches!(*kind, "FUNC" | "OBJECT" | "NOTYPE" | "IFUNC" | "TLS") {
                symbols.push((value, size, name, matches!(*kind, "FUNC" | "IFUNC")));
            }
        }
        symbols.sort();
        symbols.dedup_by(|a, b| a.0 == b.0 && a.2 == b.2);
        let mut dynrel = BTreeMap::new();
        let mut relative = BTreeMap::new();
        let mut plt_index = BTreeMap::new();
        let mut in_rela_plt = false;
        for line in run_ok(dir, &tools.readelf, &["-rW", file]).lines() {
            if line.starts_with("Relocation section") {
                in_rela_plt = line.contains(".rela.plt");
            }
            let fields: Vec<&str> = line.split_whitespace().collect();
            let [place, _, kind, rest @ ..] = fields.as_slice() else {
                continue;
            };
            // GNU ld leaves a spare `R_X86_64_NONE` entry at address 0.
            if place.len() != 8 || !kind.starts_with("R_X86_64_") || *kind == "R_X86_64_NONE" {
                continue;
            }
            let Some(place) = hex(place) else { continue };
            let symbol = rest.get(1).map_or("", |s| s.split('@').next().unwrap_or(s));
            if *kind == "R_X86_64_RELATIVE"
                && let Some(addend) = rest.last().and_then(|a| hex(a))
            {
                relative.insert(place, addend);
            }
            if in_rela_plt {
                let index = plt_index.len() as u64;
                plt_index.insert(place, index);
            }
            dynrel.insert(place, format!("{kind}({symbol})"));
        }
        Self {
            data,
            sections,
            symbols,
            dynrel,
            relative,
            plt_index,
        }
    }

    fn section_of(&self, address: u64) -> Option<&Section> {
        self.sections
            .iter()
            .find(|s| s.addr <= address && address < s.addr.saturating_add(s.size))
    }

    fn section(&self, name: &str) -> Option<&Section> {
        self.sections.iter().find(|s| s.name == name)
    }

    /// The 8 bytes at `address`, if they are in the file.
    fn quad(&self, address: u64) -> Option<u64> {
        let section = self.section_of(address)?;
        if section.kind == "NOBITS" {
            return Some(0);
        }
        let at = usize::try_from(section.offset + (address - section.addr)).ok()?;
        let bytes = self.data.get(at..at + 8)?;
        Some(u64::from_le_bytes(bytes.try_into().ok()?))
    }

    /// `address` as the nearest symbol at or below it, or a section offset.
    fn name(&self, address: u64) -> String {
        let at = self.symbols.partition_point(|s| s.0 <= address);
        if let Some((value, size, name, _)) = at.checked_sub(1).and_then(|i| self.symbols.get(i))
            && (address - value < (*size).max(1) || *size == 0 && address - value < 0x100)
            && self
                .section_of(*value)
                .is_some_and(|s| self.section_of(address) == Some(s))
        {
            let offset = address - value;
            return if offset == 0 {
                name.clone()
            } else {
                format!("{name}+{offset:#x}")
            };
        }
        match self.section_of(address) {
            Some(s) => format!("{}+{:#x}", s.name, address - s.addr),
            None => format!("{address:#x}"),
        }
    }

    /// `address` symbolically: a GOT slot by what fills it (the dynamic
    /// relocation, or all 64 bits of its contents), a PLT entry by its
    /// section, anything else by [`Self::name`].
    fn symbolize(&self, address: u64) -> String {
        if let Some(reloc) = self.dynrel.get(&address)
            && !reloc.starts_with("R_X86_64_RELATIVE")
            && !reloc.starts_with("R_X86_64_IRELATIVE")
        {
            return format!("GOT[{reloc}]");
        }
        match self.section_of(address) {
            Some(s) if s.name == ".got" || s.name == ".got.plt" => {
                // A relative relocation says what the word will hold; the
                // linkers differ in whether they also write it there.
                let value = match self.relative.get(&address) {
                    Some(&addend) => addend,
                    None => self.quad(address).unwrap_or(0),
                };
                let (low, high) = (value & 0xffff_ffff, value >> 32);
                let what = if self.section_of(low).is_some() {
                    format!("&{}", self.name(low))
                } else {
                    format!("{low:#x}")
                };
                if high == 0 {
                    format!("GOT[{what}]")
                } else {
                    format!("GOT[{what}, high {high:#x}]")
                }
            }
            Some(s) if s.name.starts_with(".plt") || s.name == ".iplt" => s.name.clone(),
            // A string constant by its contents: the two linkers merge
            // `.rodata.str1.1` into different offsets.
            Some(s) if s.name.starts_with(".rodata") && s.kind != "NOBITS" => {
                let at = usize::try_from(s.offset + (address - s.addr)).unwrap_or(0);
                let end = usize::try_from(s.offset + s.size).unwrap_or(0);
                let bytes = self.data.get(at..end).unwrap_or_default();
                match bytes.iter().position(|&b| b == 0) {
                    Some(n) if n > 0 && n < 64 && bytes[..n].iter().all(u8::is_ascii) => {
                        format!("{:?}", String::from_utf8_lossy(&bytes[..n]))
                    }
                    _ => self.name(address),
                }
            }
            _ => self.name(address),
        }
    }

    /// The instructions from `start` to `stop`, symbolized.
    fn disassemble(
        &self,
        tools: &Tools,
        dir: &Path,
        file: &str,
        start: u64,
        stop: u64,
    ) -> Vec<String> {
        let start = format!("--start-address={start:#x}");
        let stop = format!("--stop-address={stop:#x}");
        let listing = run_ok(
            dir,
            &tools.objdump,
            &[
                "-d",
                "--no-show-raw-insn",
                "-M",
                "suffix",
                &start,
                &stop,
                file,
            ],
        );
        let mut out = Vec::new();
        for line in listing.lines() {
            let Some((addr, text)) = line.trim_start().split_once(":\t") else {
                continue;
            };
            if hex(addr).is_none() {
                continue;
            }
            // `# 404000 <data>`: the address of a `%rip`-relative operand.
            let (text, rip) = match text.split_once(" #") {
                Some((text, comment)) => (text, comment.split_whitespace().next().and_then(hex)),
                None => (text, None),
            };
            let text = text.split(" <").next().unwrap_or(text);
            out.push(self.rewrite(text.trim(), rip));
        }
        out
    }

    /// The instructions of function `name`, symbolized.
    fn function(&self, tools: &Tools, dir: &Path, file: &str, name: &str) -> Vec<String> {
        let (value, size, ..) = self
            .symbols
            .iter()
            .find(|s| s.2 == name && s.3)
            .unwrap_or_else(|| panic!("{file} has no function {name}"));
        self.disassemble(tools, dir, file, *value, value + size.max(&1))
    }

    fn rewrite(&self, text: &str, rip: Option<u64>) -> String {
        let (mnemonic, operands) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
        let operands = operands.trim();
        // Branches: the target, whatever prefixes precede the mnemonic
        // (`data16 data16 rex.W callq 1040`).
        let words: Vec<&str> = text.split_whitespace().collect();
        if let [head @ .., target] = words.as_slice()
            && head
                .iter()
                .any(|w| w.starts_with('j') || w.starts_with("call"))
            && let Some(target) = hex(target)
        {
            return format!("{} {}", head.join(" "), self.symbolize(target));
        }
        let mut rewritten = String::new();
        let mut rest = operands;
        while let Some(at) = rest.find("0x") {
            let (before, after) = rest.split_at(at);
            let digits = after[2..]
                .find(|c: char| !c.is_ascii_hexdigit())
                .unwrap_or(after.len() - 2);
            let value = u64::from_str_radix(&after[2..2 + digits], 16).unwrap_or(0);
            let tail = &after[2 + digits..];
            let negative = before.ends_with('-');
            let before = if negative {
                &before[..before.len() - 1]
            } else {
                before
            };
            rewritten.push_str(before);
            if tail.starts_with("(%rip)")
                && let Some(target) = rip
            {
                rewritten.push_str(&format!("[{}]", self.symbolize(target)));
            } else if !negative && value >= 0x1000 && self.section_of(value).is_some() {
                rewritten.push_str(&self.symbolize(value));
            } else {
                if negative {
                    rewritten.push('-');
                }
                rewritten.push_str(&format!("0x{value:x}"));
            }
            rest = tail;
        }
        rewritten.push_str(rest);
        format!("{mnemonic} {rewritten}").trim().to_string()
    }

    /// The dynamic relocations as sorted `TYPE(symbol)` strings.
    fn dynamic_relocations(&self) -> Vec<String> {
        let mut all: Vec<String> = self.dynrel.values().cloned().collect();
        all.sort();
        all
    }
}

/// The `.dynamic` tags of `file`, sorted, without values.
fn dynamic_tags(tools: &Tools, dir: &Path, file: &str) -> Vec<String> {
    let mut tags: Vec<String> = run_ok(dir, &tools.readelf, &["-dW", file])
        .lines()
        .filter_map(|l| {
            let open = l.find('(')?;
            let close = l.find(')')?;
            l.trim_start()
                .starts_with("0x")
                .then(|| l[open + 1..close].to_string())
        })
        .collect();
    tags.sort();
    tags
}

/// The header lines that identify an x32 output.
fn check_header(tools: &Tools, dir: &Path, file: &str) {
    let header = run_ok(dir, &tools.readelf, &["-hW", file]);
    assert!(header.contains("ELF32"), "{file}: {header}");
    assert!(header.contains("X86-64"), "{file}: {header}");
}

/// Compares `functions` of `file` between GNU ld's output and `other`'s
/// (`qld` or `lld`), the PLT sections, and the dynamic relocations and tags.
fn compare_with(tools: &Tools, dir: &Path, other: &str, file: &str, functions: &[&str]) {
    let gnu_file = format!("gnu/{file}");
    let other_file = format!("{other}/{file}");
    let gnu = Image::load(tools, dir, &gnu_file);
    let ours = Image::load(tools, dir, &other_file);
    for function in functions {
        let expected = gnu.function(tools, dir, &gnu_file, function);
        let actual = ours.function(tools, dir, &other_file, function);
        assert_eq!(
            actual, expected,
            "{file}: `{function}` differs from GNU ld's (left: {other}, right: GNU ld)"
        );
    }
    for plt in [".plt", ".plt.sec", ".plt.got", ".iplt"] {
        let (Some(a), Some(b)) = (gnu.section(plt), ours.section(plt)) else {
            continue;
        };
        if a.size != b.size {
            continue;
        }
        let expected = plt_entries(&gnu, tools, dir, &gnu_file, a);
        let actual = plt_entries(&ours, tools, dir, &other_file, b);
        assert_eq!(
            actual, expected,
            "{file}: {plt} differs from GNU ld's (left: {other}, right: GNU ld)"
        );
    }
    for got in [".got", ".got.plt"] {
        let (Some(a), Some(b)) = (gnu.section(got), ours.section(got)) else {
            continue;
        };
        if a.size != b.size {
            continue;
        }
        assert_eq!(
            got_words(&ours, b),
            got_words(&gnu, a),
            "{file}: {got} differs from GNU ld's (left: {other}, right: GNU ld)"
        );
    }
    // GNU ld keeps the PLT entry of a `__tls_get_addr` call a relaxation
    // removed even when nothing defines the symbol — here only the dynamic
    // linker would, and these programs have none. qld keeps it when the
    // symbol is preemptible, as it is in a program linked against a C
    // library, and drops it otherwise.
    let undefined_tls_get_addr = |r: &String| r == "R_X86_64_JUMP_SLOT(__tls_get_addr)";
    let mut expected = gnu.dynamic_relocations();
    expected.retain(|r| !undefined_tls_get_addr(r));
    let mut actual = ours.dynamic_relocations();
    actual.retain(|r| !undefined_tls_get_addr(r));
    assert_eq!(
        actual, expected,
        "{file}: dynamic relocations (left: {other}, right: GNU ld)"
    );
    let mut expected = dynamic_tags(tools, dir, &gnu_file);
    // GNU ld resolves TLS descriptors lazily, through a PLT entry of its
    // own; qld, like lld, leaves them to the dynamic linker's eager pass
    // (`docs/compatibility.md`).
    expected.retain(|tag| !tag.starts_with("TLSDESC"));
    assert_eq!(
        dynamic_tags(tools, dir, &other_file),
        expected,
        "{file}: .dynamic tags (left: {other}, right: GNU ld)"
    );
    check_header(tools, dir, &other_file);
}

/// The words of a GOT section, each as the dynamic relocation that fills
/// it and the 64-bit value the linker left there — a lazy `.got.plt` slot
/// holds the address of its PLT entry's `push`, and the reserved words the
/// address of `_DYNAMIC` and two zeros. Sorted, because the two linkers
/// give the entries different indexes.
fn got_words(image: &Image, section: &Section) -> Vec<String> {
    // The three reserved words of `.got.plt` come first, then one word per
    // PLT entry, in the order the linker chose.
    let plt = image.section(".plt");
    let lazy_target = |at: u64, value: u64| -> Option<String> {
        let plt = plt?;
        let entry = plt.entsize.max(1);
        let offset = value.checked_sub(plt.addr)?;
        if offset >= plt.size {
            return None;
        }
        // The slot of the PLT relocation with index `n` points at PLT
        // entry `n`: at its `push` without IBT, at its start with it.
        let own = *image.plt_index.get(&at)? == offset.checked_div(entry)?.checked_sub(1)?;
        let within = offset % entry;
        own.then(|| format!("its own PLT entry+{within:#x}"))
    };
    let mut words: Vec<String> = Vec::new();
    let mut at = section.addr;
    while at < section.addr + section.size {
        let value = image.quad(at).unwrap_or(0);
        let (low, high) = (value & 0xffff_ffff, value >> 32);
        let target = if low == 0 {
            "0".to_string()
        } else if let Some(own) = lazy_target(at, low) {
            own
        } else {
            image.name(low)
        };
        let reloc = image
            .dynrel
            .get(&at)
            .cloned()
            .or_else(|| {
                image
                    .relative
                    .get(&at)
                    .map(|a| format!("RELATIVE({})", image.name(*a)))
            })
            .unwrap_or_else(|| "-".to_string());
        words.push(format!("{reloc} = {target} high {high:#x}"));
        at += 8;
    }
    words.sort();
    words
}

/// The entries of a PLT section, symbolized, sorted: the two linkers give
/// the same entries different indexes, so an entry's relocation index (the
/// `push` of a lazy entry) is checked to be its position here and left out
/// of the comparison.
fn plt_entries(
    image: &Image,
    tools: &Tools,
    dir: &Path,
    file: &str,
    section: &Section,
) -> Vec<Vec<String>> {
    let step = section.entsize.max(1);
    let mut entries: Vec<Vec<String>> = Vec::new();
    let mut index = 0u64;
    let mut at = section.addr;
    while at < section.addr + section.size {
        let mut lines = image.disassemble(tools, dir, file, at, at + step);
        if let Some(position) = lines.iter().position(|l| l.starts_with("pushq $0x")) {
            let pushed = lines[position].trim_start_matches("pushq $").to_string();
            assert_eq!(
                hex(&pushed),
                Some(index),
                "{file}: {} entry {index} pushes {pushed}",
                section.name
            );
            lines.remove(position);
            index += 1;
        }
        entries.push(lines);
        at += step;
    }
    // The header, if there is one, stays first; the entries are sorted.
    let head = usize::from(section.name == ".plt" && entries.len() > 1 && index > 0);
    entries[head..].sort();
    entries
}

fn compare(tools: &Tools, dir: &Path, file: &str, functions: &[&str]) {
    compare_with(tools, dir, "qld", file, functions);
}

/// Every `GOTPCRELX` relaxation GNU ld makes for x32: position-dependent,
/// in a PIE and in a shared object.
#[test]
fn gotpcrelx_relaxations() {
    let tools = require!();
    let dir = scratch("gotpcrelx");
    assemble(tools, &dir, "relax.s", "relax.o", &[]);
    ld_both(tools, &dir, "static", &["-static", "relax.o"]);
    compare(tools, &dir, "static", &["_start"]);
    ld_both(tools, &dir, "pie", &["-pie", "relax.o"]);
    compare(tools, &dir, "pie", &["_start"]);
    ld_both(tools, &dir, "relax.so", &["-shared", "relax.o"]);
    compare(tools, &dir, "relax.so", &["_start"]);
}

/// The TLS sequences: relaxed to local-exec in a static executable, kept
/// dynamic in a shared object, and relaxed to initial-exec in a PIE whose
/// variables are in that shared object.
#[test]
fn tls_sequences() {
    let tools = require!();
    let dir = scratch("tls-sequences");
    assemble(tools, &dir, "tls.s", "abs.o", &[]);
    assemble(tools, &dir, "tls.s", "pic.o", &["PIC"]);
    assemble(tools, &dir, "tls.s", "ext.o", &["PIC", "EXTERN"]);
    ld_both(tools, &dir, "static", &["-static", "abs.o"]);
    compare(tools, &dir, "static", &["_start"]);
    ld_both(tools, &dir, "libtls.so", &["-shared", "pic.o"]);
    compare(tools, &dir, "libtls.so", &["_start"]);
    ld_both(
        tools,
        &dir,
        "pie",
        &["-pie", "--dynamic-linker", INTERPRETER, "ext.o", "-ltls"],
    );
    compare(tools, &dir, "pie", &["_start"]);
}

/// The local-dynamic sequence with an indirect `__tls_get_addr` call,
/// which is 13 bytes rather than 12. GNU ld 2.46 relaxes it with the
/// 12-byte sequence of the direct form — it converts the call to
/// `addr32 call` first and then looks for the `ff` of an indirect one — and
/// leaves the last byte of the call behind, so its output is not
/// executable. qld writes the 13-byte sequence GNU ld means to
/// (`docs/compatibility.md`).
#[test]
fn local_dynamic_indirect_call() {
    let tools = require!();
    let dir = scratch("tls-ld-indirect");
    assemble(tools, &dir, "tls.s", "abs.o", &["LD_INDIRECT"]);
    let qld = PathBuf::from(env!("CARGO_BIN_EXE_qld"));
    run_ok(
        &dir,
        &qld,
        &["-m", "elf32_x86_64", "-static", "abs.o", "-o", "qld/static"],
    );
    let image = Image::load(tools, &dir, "qld/static");
    let code = image.function(tools, &dir, "qld/static", "_start");
    let direct = code
        .windows(3)
        .find(|w| w[0] == "nopl 0x0(%rax)")
        .unwrap_or_else(|| panic!("no relaxed local-dynamic sequence in {code:?}"));
    assert_eq!(
        direct,
        [
            "nopl 0x0(%rax)",
            "movl %fs:0x0,%eax",
            "movl -0x10(%rax),%ecx",
        ]
    );
    let indirect = code
        .windows(3)
        .find(|w| w[0] == "nopw 0x0(%rax)")
        .unwrap_or_else(|| panic!("no relaxed indirect local-dynamic call in {code:?}"));
    assert_eq!(
        indirect,
        [
            "nopw 0x0(%rax)",
            "movl %fs:0x0,%eax",
            "leal -0x10(%rax),%edx",
        ]
    );
}

/// What the freestanding test programs print.
const PROGRAM_OUTPUT: &str = "tls: 11 22 33 44 55 66\n\
    data: 7 8 9 3\n\
    calls: 12 13 42\n\
    ifunc: 99\n\
    wide: 1234567890123 -5\n";

/// A freestanding C program with TLS in every model (general-dynamic,
/// local-dynamic, initial-exec, local-exec and descriptors), GOT loads,
/// function pointers in data, IFUNCs and 64-bit arithmetic, linked as a
/// static executable and as a static PIE, which relocates itself.
#[test]
fn freestanding_programs() {
    let tools = require!();
    let dir = scratch("programs");
    for (suffix, flags) in [
        ("abs", &["-fno-pic"][..]),
        ("pie", &["-fPIE", "-DSELF_RELOC"][..]),
    ] {
        let start = format!("start-{suffix}.o");
        let main = format!("main-{suffix}.o");
        compile_freestanding(tools, &dir, "start.c", &start, flags);
        compile_freestanding(tools, &dir, "main.c", &main, flags);
        // The dynamic TLS models need PIC.
        compile_freestanding(
            tools,
            &dir,
            "tls_gd.c",
            &format!("gd-{suffix}.o"),
            &["-fPIC"],
        );
        compile_freestanding(
            tools,
            &dir,
            "tls_gd.c",
            &format!("desc-{suffix}.o"),
            &["-fPIC", "-mtls-dialect=gnu2", "-DDESC"],
        );
    }
    let static_args = [
        "-static",
        "start-abs.o",
        "main-abs.o",
        "gd-abs.o",
        "desc-abs.o",
    ];
    ld_both(tools, &dir, "static", &static_args);
    let pie_args = [
        "-static",
        "-pie",
        "--no-dynamic-linker",
        "-z",
        "text",
        "start-pie.o",
        "main-pie.o",
        "gd-pie.o",
        "desc-pie.o",
    ];
    ld_both(tools, &dir, "static-pie", &pie_args);
    let functions = [
        "_start",
        "start_c",
        "main",
        "relocate",
        "tls_setup",
        "tls_values",
        "tls_gd",
        "tls_ld",
        "tls_desc",
        "tls_desc_local",
        "get_a",
        "call_answer",
        "answer_ptr",
        "pick_impl",
    ];
    for output in ["static", "static-pie"] {
        compare(tools, &dir, output, &functions);
        for linker in ["gnu", "qld"] {
            if let Some(out) = execute(tools, &dir, &format!("{linker}/{output}"), linker) {
                assert_eq!(out, PROGRAM_OUTPUT, "{linker}/{output}");
            }
        }
    }
}

/// lld binds an undefined weak symbol to zero in a PIE, where GNU ld and
/// qld keep a `GLOB_DAT` against it (`relax.s` has one, `ext`): both forms
/// print the same here.
fn undefined_weak(lines: Vec<String>) -> Vec<String> {
    lines
        .into_iter()
        .map(|l| {
            l.replace("GOT[R_X86_64_GLOB_DAT(ext)]", "GOT[ext]")
                .replace("GOT[0x0]", "GOT[ext]")
        })
        .collect()
}

/// The same objects linked with `ld.lld`, where lld links x32 at all: it
/// rejects the `GOTTPOFF` and `TLSDESC` instruction forms an x32 compiler
/// emits ("must be used in MOVQ or ADDQ instructions only"), so only
/// position-independent output, where no TLS relaxation happens, can be
/// compared. There lld makes the same `GOTPCRELX` relaxations as GNU ld,
/// and so must qld.
#[test]
fn compared_with_lld() {
    let tools = require!();
    if !lld_available(tools) {
        return;
    }
    let dir = scratch("lld");
    assemble(tools, &dir, "relax.s", "relax.o", &[]);
    compile_freestanding(tools, &dir, "lib.c", "lib.o", &["-fPIC"]);
    ld_all(tools, &dir, "pie", &["-pie", "relax.o"], true);
    ld_all(tools, &dir, "relax.so", &["-shared", "relax.o"], true);
    ld_all(
        tools,
        &dir,
        "libfree.so",
        &["-shared", "-soname", "libfree.so", "lib.o"],
        true,
    );
    for (file, functions) in [
        ("pie", &["_start"][..]),
        ("relax.so", &["_start"][..]),
        ("libfree.so", &["lib_get", "lib_ld", "lib_call"][..]),
    ] {
        let lld_file = format!("lld/{file}");
        let qld_file = format!("qld/{file}");
        let lld = Image::load(tools, &dir, &lld_file);
        let ours = Image::load(tools, &dir, &qld_file);
        for function in functions {
            let expected = undefined_weak(lld.function(tools, &dir, &lld_file, function));
            let actual = undefined_weak(ours.function(tools, &dir, &qld_file, function));
            assert_eq!(
                actual, expected,
                "{file}: `{function}` differs from lld's (left: qld, right: lld)"
            );
        }
        // GNU ld's output is compared instruction for instruction
        // elsewhere; here the point is that lld agrees.
        compare(tools, &dir, file, functions);
    }
}

/// A freestanding shared library and the executables that use it: TLS
/// through the library (general-dynamic, local-dynamic and descriptors in
/// the library; initial-exec, and general-dynamic relaxed to it, in the
/// executables), PLT calls with lazy binding, copy relocations, canonical
/// PLT entries, IFUNCs, and the IBT PLT. They run with the x32 dynamic
/// linker when there is one.
#[test]
fn shared_libraries() {
    let tools = require!();
    let expected = "lib: 3 7 45 10\nmain: 5 6 1 2 42 1 10\n";
    for (name, dialect, ldflags) in [
        ("lazy", "-mtls-dialect=gnu", &["-z", "lazy"][..]),
        ("now", "-mtls-dialect=gnu2", &["-z", "now"][..]),
        (
            "ibt",
            "-mtls-dialect=gnu",
            &["-z", "lazy", "-z", "ibtplt"][..],
        ),
    ] {
        let dir = scratch(&format!("shared-{name}"));
        let ibt = if name == "ibt" {
            "-fcf-protection"
        } else {
            "-fcf-protection=none"
        };
        compile_freestanding(tools, &dir, "lib.c", "lib.o", &["-fPIC", dialect, ibt]);
        compile_freestanding(
            tools,
            &dir,
            "start.c",
            "start.o",
            &["-fPIE", "-DDYNAMIC", ibt],
        );
        compile_freestanding(
            tools,
            &dir,
            "use_lib.c",
            "exe.o",
            &["-fno-pic", dialect, ibt],
        );
        compile_freestanding(tools, &dir, "use_lib.c", "pie.o", &["-fPIE", dialect, ibt]);
        compile_freestanding(
            tools,
            &dir,
            "use_lib.c",
            "gd.o",
            &["-fPIC", dialect, ibt, "-DPIC_MAIN"],
        );
        let mut lib = vec!["-shared", "-soname", "libfree.so", "lib.o"];
        lib.extend_from_slice(ldflags);
        ld_both(tools, &dir, "libfree.so", &lib);
        compare(
            tools,
            &dir,
            "libfree.so",
            &["lib_get", "lib_ld", "lib_call", "lib_pick"],
        );
        for (output, mode, object) in [
            ("exe", "-no-pie", "exe.o"),
            ("pie", "-pie", "pie.o"),
            ("pic", "-pie", "gd.o"),
        ] {
            let mut args = vec![
                mode,
                "--dynamic-linker",
                INTERPRETER,
                "--allow-shlib-undefined",
                "start.o",
                object,
                "-lfree",
            ];
            args.extend_from_slice(ldflags);
            ld_both(tools, &dir, output, &args);
            compare(tools, &dir, output, &["main"]);
            if Path::new(INTERPRETER).exists() {
                run_all(tools, &dir, output, expected);
            }
        }
    }
}

/// A program against the x32 C library, statically and dynamically linked.
#[test]
fn hello() {
    let Some(tools) = require_libc() else {
        return;
    };
    let dir = scratch("hello");
    compile(tools, &dir, "hello.c", "hello.o", &[]);
    for (output, mode) in [
        ("static", "-static"),
        ("pie", "-pie"),
        ("nopie", "-no-pie"),
        ("static-pie", "-static-pie"),
    ] {
        drive_both(tools, &dir, false, output, &[mode, "hello.o"]);
        if let Some(out) = execute(tools, &dir, &format!("qld/{output}"), "qld") {
            assert_eq!(out, "hello 6 x32\n", "{output}");
        }
    }
    compare(tools, &dir, "pie", &["main"]);
    compare(tools, &dir, "nopie", &["main"]);
}

/// C++ with exceptions, virtual calls, the standard library and threads,
/// against the x32 C and C++ libraries.
#[test]
fn cxx_exceptions() {
    let Some(tools) = require_libc() else {
        return;
    };
    if tools.cxx.is_none() {
        assert!(
            !required("QLD_REQUIRE_X32_CXX"),
            "QLD_REQUIRE_X32_CXX is set but g++ -mx32 does not link"
        );
        println!("SKIPPED: no g++ -mx32 (the x32 C++ library)");
        return;
    }
    let expected = "derived:0=1\nderived:1=1\nderived:2=1\n\
                    caught: bottom reached after 6 calls\nthread calls: 3\n";
    let dir = scratch("cxx");
    compile(tools, &dir, "cxx.cpp", "cxx.o", &[]);
    for (output, mode) in [("static", "-static"), ("pie", "-pie")] {
        drive_both(tools, &dir, true, output, &[mode, "cxx.o", "-pthread"]);
        if let Some(out) = execute(tools, &dir, &format!("qld/{output}"), "qld") {
            assert_eq!(out, expected, "{output}");
        }
    }
}

/// `--emit-relocs` keeps the input relocations, with the `GOTPCRELX`
/// conversions GNU ld records: `R_X86_64_PC32` for a `lea`, and
/// `R_X86_64_32` (never `32S`, x32 having cleared REX.W) for a load turned
/// into an immediate.
#[test]
fn emit_relocs() {
    let tools = require!();
    let dir = scratch("emit-relocs");
    assemble(tools, &dir, "relax.s", "relax.o", &[]);
    ld_both(
        tools,
        &dir,
        "static",
        &["-static", "--emit-relocs", "relax.o"],
    );
    let kinds = |linker: &str| -> Vec<String> {
        let mut all: Vec<String> =
            run_ok(&dir, &tools.readelf, &["-rW", &format!("{linker}/static")])
                .lines()
                .filter_map(|l| {
                    let fields: Vec<&str> = l.split_whitespace().collect();
                    let [place, _, kind, rest @ ..] = fields.as_slice() else {
                        return None;
                    };
                    if place.len() != 8 || !kind.starts_with("R_X86_64_") {
                        return None;
                    }
                    let symbol = rest.get(1).map_or("", |s| s.split('@').next().unwrap_or(s));
                    Some(format!("{kind}({symbol})"))
                })
                .collect();
        all.sort();
        all
    };
    assert_eq!(
        kinds("qld"),
        kinds("gnu"),
        "--emit-relocs types (left: qld, right: GNU ld)"
    );
    assert!(kinds("qld").iter().any(|k| k.starts_with("R_X86_64_32(")));
}

/// Objects for another x86 ABI are rejected, not linked as x32: i386
/// objects have the same class, x86-64 ones the same machine.
#[test]
fn foreign_objects_rejected() {
    let tools = require!();
    let dir = scratch("foreign");
    assemble(tools, &dir, "relax.s", "x32.o", &[]);
    let source = format!("{DATA}/relax.s");
    let qld = PathBuf::from(env!("CARGO_BIN_EXE_qld"));
    for (flag, object) in [("-m32", "i386.o"), ("-m64", "x86_64.o")] {
        let built = run(&dir, &tools.cc, &[flag, "-c", &source, "-o", object]);
        if !built.status.success() {
            continue;
        }
        for args in [
            ["-m", "elf32_x86_64", "-shared", object, "-o", "out.so"].as_slice(),
            ["-shared", "x32.o", object, "-o", "out.so"].as_slice(),
        ] {
            let output = run(&dir, &qld, args);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                !output.status.success() && stderr.contains("is incompatible with X86_64X32 "),
                "{args:?}: {stderr}"
            );
        }
    }
}

/// Without `-m`, an x32 object makes the link an x32 one.
#[test]
fn target_from_first_object() {
    let tools = require!();
    let dir = scratch("infer");
    assemble(tools, &dir, "relax.s", "relax.o", &[]);
    let qld = PathBuf::from(env!("CARGO_BIN_EXE_qld"));
    run_ok(&dir, &qld, &["-static", "relax.o", "-o", "qld/implicit"]);
    run_ok(
        &dir,
        &qld,
        &[
            "-m",
            "elf32_x86_64",
            "-static",
            "relax.o",
            "-o",
            "qld/explicit",
        ],
    );
    assert_eq!(
        fs::read(dir.join("qld/implicit")).unwrap(),
        fs::read(dir.join("qld/explicit")).unwrap()
    );
    check_header(tools, &dir, "qld/implicit");
}
