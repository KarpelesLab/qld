//! i386 ELF tests (workstream W40).
//!
//! Every test links the same objects with qld and with GNU ld
//! (`-m elf_i386`) and compares what the two produced *symbolically*: in
//! each function the test's own sources define, every address an
//! instruction computes — a branch target, an absolute address, a
//! `%ebx`-relative GOT or `GOTOFF` operand, the `GOTPC` addend that loads
//! `%ebx` — is printed as `symbol+offset`, and a GOT slot by the dynamic
//! relocation that fills it or the address it holds, so the comparison holds
//! wherever each linker placed things. qld follows GNU ld's relaxation
//! decisions, so relaxed code must match instruction for instruction.
//! Dynamic relocations are compared as `(type, symbol)` multisets and
//! `.dynamic` by tag.
//!
//! The programs that link against glibc also *run*, when the host can
//! execute i386 binaries (an x86-64 Linux with the 32-bit runtime), and must
//! print what GNU ld's do — including qld executables with GNU ld shared
//! libraries and the reverse.
//!
//! Tools: `gcc` and `g++` with `-m32` (Debian/Ubuntu `gcc-multilib` and
//! `g++-multilib`), GNU `ld` with the `elf_i386` emulation, `objdump` and
//! `readelf`. A test prints `SKIPPED:` and passes when one is missing,
//! unless `QLD_REQUIRE_I386_TOOLS=1`; running is skipped the same way when
//! i386 binaries cannot execute, unless `QLD_REQUIRE_I386_RUN=1`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;

const DATA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/i386");

fn in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

fn required(var: &str) -> bool {
    std::env::var_os(var).is_some_and(|v| !v.is_empty() && v != "0")
}

struct Tools {
    cc: PathBuf,
    cxx: Option<PathBuf>,
    ld: PathBuf,
    objdump: PathBuf,
    readelf: PathBuf,
    /// A directory whose `ld` is qld, for `gcc -B`.
    shim: PathBuf,
    /// Whether i386 programs run on this host.
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
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("i386-probe");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    fs::write(dir.join("t.c"), "int main(void) { return 0; }\n").map_err(|e| e.to_string())?;
    let compiled = run(&dir, &cc, &["-m32", "t.c", "-o", "t"]);
    if !compiled.status.success() {
        return Err(format!(
            "gcc -m32 cannot link: {}",
            String::from_utf8_lossy(&compiled.stderr)
                .lines()
                .next()
                .unwrap_or("")
        ));
    }
    let emulations = run(&dir, &ld, &["-V"]);
    if !String::from_utf8_lossy(&emulations.stdout).contains("elf_i386") {
        return Err(format!("{} has no elf_i386 emulation", ld.display()));
    }
    let runs = Command::new(dir.join("t"))
        .output()
        .is_ok_and(|o| o.status.success());
    let shim = Path::new(env!("CARGO_TARGET_TMPDIR")).join("i386-ld");
    let _ = fs::remove_dir_all(&shim);
    fs::create_dir_all(&shim).map_err(|e| e.to_string())?;
    link_shim(&shim)?;
    let cxx = in_path("g++").filter(|cxx| {
        fs::write(dir.join("t.cpp"), "int main() { return 0; }\n").is_ok()
            && run(&dir, cxx, &["-m32", "t.cpp", "-o", "tx"])
                .status
                .success()
    });
    Ok(Tools {
        cc,
        cxx,
        ld,
        objdump,
        readelf,
        shim,
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
                !required("QLD_REQUIRE_I386_TOOLS"),
                "QLD_REQUIRE_I386_TOOLS is set but {why}"
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
        .join("i386-tests")
        .join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("gnu")).unwrap();
    fs::create_dir_all(dir.join("qld")).unwrap();
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
    let mut args = vec!["-m32", "-O2", "-c", path.as_str(), "-o", object];
    args.extend_from_slice(extra);
    run_ok(dir, &compiler, &args);
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
        let mut all = vec!["-m32"];
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

/// `ld -m elf_i386` and qld on the same arguments.
///
/// The hash style is explicit: the default is a configure-time choice of
/// GNU ld's (Ubuntu's emits both tables, Gentoo's only `.gnu.hash`). Both
/// also covers qld's ELF32 `.hash`.
fn ld_both(tools: &Tools, dir: &Path, output: &str, args: &[&str]) {
    for linker in ["gnu", "qld"] {
        let out = format!("{linker}/{output}");
        let mut all = vec!["-m", "elf_i386", "--hash-style=both"];
        all.extend_from_slice(args);
        all.extend_from_slice(&["-o", &out]);
        let program = if linker == "gnu" {
            tools.ld.clone()
        } else {
            PathBuf::from(env!("CARGO_BIN_EXE_qld"))
        };
        run_ok(dir, &program, &all);
    }
}

/// Runs `<linker>/<program>` with `LD_LIBRARY_PATH` set to `<libs>/`, and
/// returns its output.
fn execute(tools: &Tools, dir: &Path, program: &str, libs: &str) -> Option<String> {
    if !tools.runs {
        assert!(
            !required("QLD_REQUIRE_I386_RUN"),
            "QLD_REQUIRE_I386_RUN is set but i386 programs do not run here"
        );
        return None;
    }
    let output = Command::new(dir.join(program))
        .current_dir(dir)
        .env("LD_LIBRARY_PATH", dir.join(libs))
        .output()
        .unwrap();
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
            if !index.ends_with(':') || *ndx == "UND" || *ndx == "ABS" {
                continue;
            }
            let (Some(value), Ok(size)) = (hex(value), size.parse::<u64>()) else {
                continue;
            };
            let name = name.split('@').next().unwrap_or(name).to_string();
            if name == "_GLOBAL_OFFSET_TABLE_" {
                got_base = Some(value);
            }
            if matches!(*kind, "FUNC" | "OBJECT" | "NOTYPE" | "IFUNC") {
                symbols.push((value, size, name, matches!(*kind, "FUNC" | "IFUNC")));
            }
        }
        symbols.sort();
        symbols.dedup_by(|a, b| a.0 == b.0 && a.2 == b.2);
        let got_base = got_base.unwrap_or_else(|| {
            sections
                .iter()
                .find(|s| s.3 == ".got.plt")
                .or_else(|| sections.iter().find(|s| s.3 == ".got"))
                .map_or(0, |s| s.0)
        });
        let mut dynrel = BTreeMap::new();
        for line in run_ok(dir, &tools.readelf, &["-rW", file]).lines() {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let [place, _, kind, rest @ ..] = fields.as_slice() else {
                continue;
            };
            if place.len() != 8 || !kind.starts_with("R_386_") {
                continue;
            }
            let Some(place) = hex(place) else { continue };
            let symbol = rest.get(1).map_or("", |s| s.split('@').next().unwrap_or(s));
            dynrel.insert(place, format!("{kind}({symbol})"));
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

    fn word(&self, address: u64) -> Option<u64> {
        let (addr, offset, _, _) = self.section_of(address)?;
        let at = usize::try_from(offset + (address - addr)).ok()?;
        let bytes = self.data.get(at..at + 4)?;
        Some(u64::from(u32::from_le_bytes(bytes.try_into().ok()?)))
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

    /// `address` symbolically: a GOT slot by what fills it, a PLT entry by
    /// its section, anything else by [`Self::name`].
    fn symbolize(&self, address: u64) -> String {
        if let Some(reloc) = self.dynrel.get(&address)
            && !reloc.starts_with("R_386_RELATIVE")
            && !reloc.starts_with("R_386_IRELATIVE")
        {
            return format!("GOT[{reloc}]");
        }
        match self.section_of(address) {
            Some((_, _, _, s)) if s == ".got" || s == ".got.plt" => {
                let value = self.word(address).unwrap_or(0);
                format!("GOT[&{}]", self.name(value))
            }
            Some((_, _, _, s)) if s.starts_with(".plt") || s == ".iplt" => s.to_string(),
            _ => self.name(address),
        }
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
        let mut previous: Option<u64> = None;
        // The register the function loaded the GOT base into.
        let mut got_register: Option<String> = None;
        for line in listing.lines() {
            let Some((addr, text)) = line.trim_start().split_once(":\t") else {
                continue;
            };
            let Some(here) = hex(addr) else { continue };
            let mut text = text.split(" <").next().unwrap_or(text).to_string();
            if let Some(i) = text.find(" #") {
                text.truncate(i);
            }
            let text = self.rewrite(text.trim(), here, previous, &mut got_register);
            previous = Some(here);
            out.push(text);
        }
        out
    }

    fn rewrite(
        &self,
        text: &str,
        here: u64,
        previous: Option<u64>,
        got_register: &mut Option<String>,
    ) -> String {
        let (mnemonic, operands) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
        let operands = operands.trim();
        // Branches: the target.
        if (mnemonic.starts_with('j') || mnemonic.starts_with("call"))
            && let Some(target) = hex(operands)
        {
            return format!("{mnemonic} {}", self.symbolize(target));
        }
        // `addl $x, %reg` completing `%reg = GOT` after a `popl %reg` or a
        // PC thunk call.
        if mnemonic == "addl"
            && let Some((imm, register)) = operands.split_once(',')
            && let Some(imm) = imm
                .strip_prefix("$0x")
                .and_then(|i| u64::from_str_radix(i, 16).ok())
            && [Some(here), previous]
                .iter()
                .flatten()
                .any(|p| p.wrapping_add(imm) & 0xffff_ffff == self.got_base)
        {
            *got_register = Some(register.to_string());
            return format!("addl $GOTPC,{register}");
        }
        // Operands relative to the GOT register, while it holds the GOT.
        let (base, scaled) = match got_register.as_deref() {
            Some(got) => (format!("({got})"), format!("(,{got},1)")),
            None => ("\0".to_string(), "\0".to_string()),
        };
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
            if tail.starts_with(&base) || tail.starts_with(&scaled) {
                let offset = if negative {
                    self.got_base.wrapping_sub(value)
                } else {
                    self.got_base.wrapping_add(value)
                } & 0xffff_ffff;
                rewritten.push_str(&format!("{}@GOTREL", self.symbolize(offset)));
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
        // An instruction that writes the GOT register ends its GOT role.
        if let Some(got) = got_register.as_deref()
            && (operands.ends_with(&format!(",{got}")) || operands == got)
            && !matches!(
                mnemonic,
                "cmpl" | "testl" | "pushl" | "cmpb" | "testb" | "cmpw" | "testw"
            )
        {
            *got_register = None;
        }
        format!("{mnemonic} {rewritten}").trim().to_string()
    }

    /// The dynamic relocations as sorted `TYPE(symbol)` strings.
    fn dynamic_relocations(&self) -> Vec<String> {
        let mut all: Vec<String> = self.dynrel.values().cloned().collect();
        all.sort();
        all
    }
}

/// Whether `file` has a `.rel.plt` holding nothing but `R_386_TLS_DESC`
/// relocations. GNU ld before 2.46 puts TLS descriptors there, so its
/// output has `DT_JMPREL`, `DT_PLTREL` and `DT_PLTRELSZ` without a PLT
/// relocation; 2.46 moved them to `.rel.dyn` (a `.rel.tls` input section),
/// where qld puts them.
fn only_tls_descriptors_in_rel_plt(tools: &Tools, dir: &Path, file: &str) -> bool {
    let listing = run_ok(dir, &tools.readelf, &["-rW", file]);
    let mut lines = listing.lines();
    if !lines.any(|l| l.starts_with("Relocation section '.rel.plt'")) {
        return false;
    }
    lines
        .skip_while(|l| !l.trim_start().starts_with("Offset"))
        .skip(1)
        .take_while(|l| !l.trim().is_empty())
        .all(|l| l.split_whitespace().nth(2) == Some("R_386_TLS_DESC"))
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

/// GNU ld relaxes general-dynamic and descriptor accesses to initial-exec
/// through a second GOT entry holding the *positive* thread pointer offset
/// (`R_386_TLS_TPOFF32`, read with `subl` or negated), where qld, like lld,
/// reads the one negative `R_386_TLS_TPOFF` entry that initial-exec code
/// uses too (`docs/compatibility.md`). This rewrites GNU ld's form into
/// qld's, so the rest compares exactly.
fn negative_initial_exec(lines: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut negate_pending = false;
    for line in lines {
        let line = if line.contains("GOT[R_386_TLS_TPOFF32(") {
            let line = line.replace("GOT[R_386_TLS_TPOFF32(", "GOT[R_386_TLS_TPOFF(");
            if let Some(rest) = line.strip_prefix("subl ") {
                format!("addl {rest}")
            } else {
                negate_pending = line.starts_with("movl ");
                line
            }
        } else if negate_pending && line == "negl %eax" {
            negate_pending = false;
            "xchgw %ax,%ax".to_string()
        } else {
            line
        };
        out.push(line);
    }
    out
}

/// Compares `functions` of `file` between the two linkers, and their
/// dynamic relocations and tags.
fn compare(tools: &Tools, dir: &Path, file: &str, functions: &[&str]) {
    let gnu_file = format!("gnu/{file}");
    let qld_file = format!("qld/{file}");
    let gnu = Image::load(tools, dir, &gnu_file);
    let ours = Image::load(tools, dir, &qld_file);
    for function in functions {
        let expected = negative_initial_exec(gnu.function(tools, dir, &gnu_file, function));
        let actual = ours.function(tools, dir, &qld_file, function);
        assert_eq!(
            actual, expected,
            "{file}: `{function}` differs from GNU ld's (left: qld, right: GNU ld)"
        );
    }
    let mut expected: Vec<String> = gnu
        .dynamic_relocations()
        .into_iter()
        .map(|r| r.replace("R_386_TLS_TPOFF32(", "R_386_TLS_TPOFF("))
        .collect();
    expected.sort();
    expected.dedup();
    let mut actual = ours.dynamic_relocations();
    actual.dedup();
    assert_eq!(
        actual, expected,
        "{file}: dynamic relocations (left: qld, right: GNU ld)"
    );
    let mut expected = dynamic_tags(tools, dir, &gnu_file);
    if only_tls_descriptors_in_rel_plt(tools, dir, &gnu_file) {
        expected.retain(|tag| !matches!(tag.as_str(), "JMPREL" | "PLTREL" | "PLTRELSZ"));
    }
    assert_eq!(
        dynamic_tags(tools, dir, &qld_file),
        expected,
        "{file}: .dynamic tags (left: qld, right: GNU ld)"
    );
    let header = run_ok(dir, &tools.readelf, &["-hW", &qld_file]);
    assert!(header.contains("ELF32"), "{file}: {header}");
    assert!(header.contains("Intel 80386"), "{file}: {header}");
}

/// Every `R_386_GOT32X` relaxation GNU ld makes, position-dependent and in a
/// PIE.
#[test]
fn got32x_relaxations() {
    let tools = require!();
    let dir = scratch("got32x");
    let assembly = format!("{DATA}/relax.s");
    run_ok(
        &dir,
        &tools.cc,
        &[
            "-m32",
            "-c",
            "-Wa,--defsym,BASELESS=1",
            &assembly,
            "-o",
            "abs.o",
        ],
    );
    run_ok(&dir, &tools.cc, &["-m32", "-c", &assembly, "-o", "pic.o"]);
    ld_both(tools, &dir, "static", &["-static", "abs.o"]);
    compare(tools, &dir, "static", &["_start"]);
    ld_both(tools, &dir, "pie", &["-pie", "pic.o"]);
    compare(tools, &dir, "pie", &["_start"]);
}

/// The TLS sequences: relaxed to local-exec in a static executable, kept
/// dynamic in a shared object.
#[test]
fn tls_sequences() {
    let tools = require!();
    let dir = scratch("tls-sequences");
    let assembly = format!("{DATA}/tls.s");
    run_ok(
        &dir,
        &tools.cc,
        &[
            "-m32",
            "-c",
            "-Wa,--defsym,ABSOLUTE=1",
            &assembly,
            "-o",
            "abs.o",
        ],
    );
    run_ok(&dir, &tools.cc, &["-m32", "-c", &assembly, "-o", "pic.o"]);
    ld_both(tools, &dir, "static", &["-static", "abs.o"]);
    compare(tools, &dir, "static", &["_start"]);
    ld_both(tools, &dir, "tls.so", &["-shared", "pic.o"]);
    compare(tools, &dir, "tls.so", &["_start"]);
}

/// Static, PIE and position-dependent C programs against glibc.
#[test]
fn hello() {
    let tools = require!();
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
            assert_eq!(out, "hello 6\n", "{output}");
        }
    }
    compare(tools, &dir, "pie", &["main"]);
    compare(tools, &dir, "nopie", &["main"]);
}

/// TLS through a shared library: general-dynamic and local-dynamic in the
/// library, initial-exec and local-exec in the executable, general-dynamic
/// relaxed to initial-exec when the executable is PIC; in the GNU and the
/// descriptor dialects.
#[test]
fn tls_shared() {
    let tools = require!();
    let expected = "thread: 105 207 311 324\nmain: 6 9 14 28 33\n";
    for (name, dialect) in [
        ("tls-gnu", "-mtls-dialect=gnu"),
        ("tls-gnu2", "-mtls-dialect=gnu2"),
    ] {
        let dir = scratch(name);
        compile(tools, &dir, "tls_lib.c", "lib.o", &["-fPIC", dialect]);
        compile(tools, &dir, "tls_main.c", "main.o", &[dialect]);
        compile(tools, &dir, "tls_main.c", "main-pic.o", &["-fPIC", dialect]);
        // Lazy binding whatever the compiler driver's default (Gentoo's
        // passes `-z now`): GNU ld before 2.46 then puts TLS descriptors in
        // `.rel.plt` (see `only_tls_descriptors_in_rel_plt`).
        drive_both(
            tools,
            &dir,
            false,
            "libtls.so",
            &["-shared", "-Wl,-z,lazy", "lib.o"],
        );
        drive_both(tools, &dir, false, "tls", &["main.o", "-ltls", "-pthread"]);
        drive_both(
            tools,
            &dir,
            false,
            "tls-pic",
            &["main-pic.o", "-ltls", "-pthread"],
        );
        compare(tools, &dir, "libtls.so", &["lib_get", "lib_bump_ld"]);
        compare(tools, &dir, "tls", &["main"]);
        compare(tools, &dir, "tls-pic", &["main"]);
        run_all(tools, &dir, "tls", expected);
        run_all(tools, &dir, "tls-pic", expected);
    }
}

/// IFUNCs in the executable and in a shared library, copy relocations,
/// canonical PLT entries, lazy binding and the IBT PLT.
#[test]
fn ifunc_copy_relocations() {
    let tools = require!();
    let expected = "pick=41\nlib_data=1 2 3 4\nlib_add=43 same_ptr=1 name=the library\nstrlen=11\n";
    for (name, cflags, ldflags) in [
        ("pie", &[][..], &["-Wl,-z,now"][..]),
        ("nopie", &["-fno-pie"][..], &["-no-pie", "-Wl,-z,lazy"][..]),
        (
            "ibt",
            &["-fcf-protection"][..],
            &["-Wl,-z,lazy", "-Wl,-z,ibtplt"][..],
        ),
    ] {
        let dir = scratch(&format!("ifunc-{name}"));
        compile(tools, &dir, "misc_lib.c", "lib.o", &["-fPIC"]);
        compile(tools, &dir, "misc_main.c", "main.o", cflags);
        drive_both(tools, &dir, false, "libmisc.so", &["-shared", "lib.o"]);
        let mut args = vec!["main.o", "-lmisc"];
        args.extend_from_slice(ldflags);
        drive_both(tools, &dir, false, "misc", &args);
        compare(tools, &dir, "libmisc.so", &["lib_add_ptr", "lib_sum"]);
        compare(tools, &dir, "misc", &["main"]);
        run_all(tools, &dir, "misc", expected);
    }
}

/// C++ with exceptions, virtual calls, the standard library and threads.
#[test]
fn cxx_exceptions() {
    let tools = require!();
    if tools.cxx.is_none() {
        assert!(
            !required("QLD_REQUIRE_I386_TOOLS"),
            "QLD_REQUIRE_I386_TOOLS is set but g++ -m32 does not link"
        );
        println!("SKIPPED: no g++ -m32");
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

/// An object for another x86 ABI is rejected, not linked as i386: an x32
/// object has the same class, and its relocation numbers mean other things.
#[test]
fn foreign_objects_rejected() {
    let tools = require!();
    let dir = scratch("foreign");
    // No headers: an x32 compiler usually has no x32 C library.
    let source = format!("{DATA}/misc_lib.c");
    compile(tools, &dir, "misc_lib.c", "i386.o", &[]);
    let qld = PathBuf::from(env!("CARGO_BIN_EXE_qld"));
    for (flag, object) in [("-mx32", "x32.o"), ("-m64", "x86_64.o")] {
        // The host compiler may lack x32 support: skip that ABI then.
        let built = run(&dir, &tools.cc, &[flag, "-c", &source, "-o", object]);
        if !built.status.success() {
            continue;
        }
        for args in [
            ["-m", "elf_i386", "-shared", object, "-o", "out.so"].as_slice(),
            ["-shared", "i386.o", object, "-o", "out.so"].as_slice(),
        ] {
            let output = run(&dir, &qld, args);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                !output.status.success() && stderr.contains("is incompatible with X86 "),
                "{args:?}: {stderr}"
            );
        }
    }
}
