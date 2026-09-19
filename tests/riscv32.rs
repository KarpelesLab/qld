//! RISC-V 32 ELF tests (workstream W44).
//!
//! RV32 shares the RV64 backend, so these tests follow `riscv64.rs`: every
//! test links the same objects with qld and with lld and compares what the
//! two produced *symbolically*. Each address an instruction computes — an
//! `auipc`/`lui` pair, a branch, a `tp` offset, a GOT load — is printed as
//! `symbol+offset` (a GOT slot by the dynamic relocation that fills it or
//! the value it holds), so the comparison holds wherever each linker placed
//! things. Dynamic relocations and tags, symbol sizes, line tables,
//! `.eh_frame` ranges and DWARF ranges are compared the same way. qld
//! follows lld's relaxation decisions (including RV32's `c.jal`), so
//! relaxed code must match instruction for instruction, and
//! label-difference tables byte for byte.
//!
//! The objects come from `clang --target=riscv32-linux-gnu` for the three
//! hard-float ABIs (ILP32, ILP32F, ILP32D) and are freestanding:
//! distributions ship no RV32 glibc. [`freestanding_programs_run`] runs
//! static programs that need no C library under `qemu-riscv32`, when it is
//! installed.
//!
//! Tools: `clang` with the RISC-V target, `ld.lld`, `llvm-objdump` with the
//! RISC-V target, `llvm-readelf` and `llvm-dwarfdump`, from `PATH` or from
//! `QLD_RISCV_CC`, `QLD_LLD`, `QLD_LLVM_OBJDUMP`. A test prints `SKIPPED:`
//! and passes when one is missing, unless `QLD_REQUIRE_RISCV_TOOLS=1`.
//! `qemu-riscv32` (or `qemu-riscv32-static`, or `QLD_QEMU_RISCV32`) is
//! required only with `QLD_REQUIRE_RISCV32_QEMU=1`.

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
    dwarfdump: PathBuf,
}

fn probe() -> Result<Tools, String> {
    let cc = tool("QLD_RISCV_CC", "clang").ok_or("no clang")?;
    let lld = tool("QLD_LLD", "ld.lld").ok_or("no ld.lld")?;
    let objdump = tool("QLD_LLVM_OBJDUMP", "llvm-objdump").ok_or("no llvm-objdump")?;
    let readelf = in_path("llvm-readelf").ok_or("no llvm-readelf")?;
    let dwarfdump = in_path("llvm-dwarfdump").ok_or("no llvm-dwarfdump")?;
    let version = Command::new(&objdump)
        .arg("--version")
        .output()
        .map_err(|e| e.to_string())?;
    if !String::from_utf8_lossy(&version.stdout).contains("riscv32") {
        return Err(format!("{} has no RISC-V target", objdump.display()));
    }
    let targets = Command::new(&cc)
        .arg("--print-targets")
        .output()
        .map_err(|e| e.to_string())?;
    if !String::from_utf8_lossy(&targets.stdout).contains("riscv32") {
        return Err(format!("{} has no RISC-V target", cc.display()));
    }
    Ok(Tools {
        cc,
        lld,
        objdump,
        readelf,
        dwarfdump,
    })
}

fn tools() -> Option<&'static Tools> {
    static TOOLS: OnceLock<Result<Tools, String>> = OnceLock::new();
    match TOOLS.get_or_init(probe) {
        Ok(tools) => Some(tools),
        Err(why) => {
            let required = std::env::var_os("QLD_REQUIRE_RISCV_TOOLS")
                .is_some_and(|v| !v.is_empty() && v != "0");
            assert!(!required, "QLD_REQUIRE_RISCV_TOOLS is set but {why}");
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
        .join("riscv32-tests")
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

/// Compiles `source` (C, or assembly when `name` ends in `.s`) with the
/// RV32GC/ILP32D defaults plus `extra` (which may name another `-march` and
/// `-mabi`), into `object`.
fn compile(tools: &Tools, dir: &Path, name: &str, source: &str, object: &str, extra: &[&str]) {
    fs::write(dir.join(name), source).unwrap();
    let mut args = vec![
        "--target=riscv32-linux-gnu",
        "-O2",
        "-g",
        "-ffreestanding",
        "-fno-stack-protector",
        "-c",
        name,
        "-o",
        object,
    ];
    // Line tables of relaxed assembly point outside the code in both
    // linkers' outputs, so they cannot be compared symbolically.
    if name.ends_with(".s") {
        args.retain(|a| *a != "-g");
    }
    if !extra.iter().any(|a| a.starts_with("-march=")) {
        args.push("-march=rv32gc");
    }
    if !extra.iter().any(|a| a.starts_with("-mabi=")) {
        args.push("-mabi=ilp32d");
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
    u64::from_str_radix(text.trim_start_matches("0x"), 16).ok()
}

/// An immediate as `llvm-objdump` prints it: `0x1f`, `-0x8` or `12`.
fn imm(text: &str) -> Option<i64> {
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
    /// The value of `__global_pointer$`, which `gp` holds.
    gp: Option<u64>,
    symbols: Vec<(u64, u64, String)>,
    tls_symbols: Vec<(u64, u64, String)>,
    plt: BTreeMap<u64, String>,
    dynrel: BTreeMap<u64, String>,
}

/// Dynamic relocations as `(place, type, rest)`.
fn dyn_relocs(tools: &Tools, dir: &Path, file: &str) -> Vec<(u64, String, String)> {
    let mut out = Vec::new();
    let mut dynamic = false;
    for line in run_ok(dir, &tools.readelf, &["-rW", file]).lines() {
        if line.starts_with("Relocation section") {
            dynamic = line.contains("'.rela.dyn'") || line.contains("'.rela.plt'");
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        let [place, _, kind, rest @ ..] = fields.as_slice() else {
            continue;
        };
        if !dynamic || place.len() != 8 || !kind.starts_with("R_RISCV_") {
            continue;
        }
        let Some(place) = hex(place) else { continue };
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
                || name.starts_with('$')
            {
                continue;
            }
            let (Some(value), Some(size)) = (hex(value), imm(size)) else {
                continue;
            };
            let entry = (value, size as u64, (*name).to_string());
            if *kind == "TLS" {
                tls_symbols.push(entry);
            } else {
                symbols.push(entry);
            }
        }
        symbols.sort();
        tls_symbols.sort();
        let mut plt = BTreeMap::new();
        if sections.iter().any(|s| s.name == ".plt") {
            for line in run_ok(dir, &tools.objdump, &["-d", "-j", ".plt", file]).lines() {
                if let Some((address, rest)) = line.split_once(" <")
                    && let Some(name) = rest.strip_suffix("@plt>:")
                    && let Some(address) = hex(address)
                {
                    plt.insert(address, name.to_string());
                }
            }
        }
        let mut dynrel = BTreeMap::new();
        for (place, kind, rest) in dyn_relocs(tools, dir, file) {
            let what = if kind.ends_with("RELATIVE") {
                "addend".to_string()
            } else {
                rest
            };
            dynrel.entry(place).or_insert(format!("{kind} {what}"));
        }
        let gp = symbols
            .iter()
            .find(|(_, _, name)| name == "__global_pointer$")
            .map(|(value, _, _)| *value);
        Self {
            data,
            sections,
            tls,
            gp,
            symbols,
            tls_symbols,
            plt,
            dynrel,
        }
    }

    fn section_of(&self, address: u64) -> Option<&Section> {
        self.sections
            .iter()
            .rfind(|s| s.addr != 0 && s.addr <= address && address < s.addr + s.size.max(1))
    }

    fn word(&self, address: u64) -> Option<u64> {
        let s = self.section_of(address)?;
        if s.nobits {
            return None;
        }
        let at = (s.offset + address - s.addr) as usize;
        Some(u64::from(u32::from_le_bytes(
            self.data.get(at..at + 4)?.try_into().ok()?,
        )))
    }

    fn symbolize(&self, address: u64, deref: bool) -> String {
        // Addresses wrap at 32 bits (an `auipc` or `lui` reaching above
        // 2 GiB reads as negative).
        let address = address & 0xffff_ffff;
        // Tombstones of dead code in debug sections (and their ends) are
        // nowhere in the image.
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
                    Some(word) => format!("GOT[{}]", self.symbolize(word, false)),
                };
            }
        }
        let best = self
            .symbols
            .iter()
            .filter(|(value, size, name)| {
                // `__global_pointer$` lands 0x800 past `.sdata`, wherever
                // each linker puts it: never a name for an address.
                name != "__global_pointer$"
                    && *value <= address
                    && (address < value + size || (*size == 0 && address == *value))
            })
            .max_by_key(|(value, size, _)| (*value, *size));
        if let Some((value, _, name)) = best {
            return if address == *value {
                name.clone()
            } else {
                format!("{name}+{:#x}", address - value)
            };
        }
        // IFUNC stubs of a static link: lld's are in `.iplt`, qld's in
        // `.plt` (as GNU ld's), where the end of `.rela.iplt` may label
        // them.
        if let Some(s) = section
            && (s.name == ".iplt" || (s.name == ".plt" && self.plt.is_empty()))
        {
            return format!("IFUNC({:#x})", address - s.addr);
        }
        if let Some(s) = section {
            // Merged strings are laid out differently by each linker.
            if s.name.starts_with(".rodata")
                && let Some(text) = self.string_at(s, address)
            {
                return format!("STR({text:?})");
            }
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

    /// The printable NUL-terminated string at `address` of `s`, if there
    /// is one.
    fn string_at(&self, s: &Section, address: u64) -> Option<String> {
        if s.nobits {
            return None;
        }
        let at = (s.offset + address - s.addr) as usize;
        let end = (s.offset + s.size) as usize;
        let bytes = self.data.get(at..end.min(self.data.len()))?;
        let len = bytes.iter().position(|&b| b == 0)?;
        let text = bytes.get(..len)?;
        let printable = |b: &u8| (0x20..0x7f).contains(b) || *b == b'\n';
        (!text.is_empty() && text.iter().all(printable))
            .then(|| String::from_utf8_lossy(text).into_owned())
    }

    fn end(&self, address: u64) -> String {
        format!("{}(end)", self.symbolize(address.wrapping_sub(1), false))
    }

    fn tls_symbolize(&self, offset: i64) -> String {
        let offset = offset as u64;
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
}

/// The disassembly of `file` with every computed address symbolized, one
/// block per symbol, blocks sorted by name.
fn listing(tools: &Tools, dir: &Path, file: &str) -> String {
    let image = Image::load(tools, dir, file);
    let text = run_ok(
        dir,
        &tools.objdump,
        &["-d", "--no-show-raw-insn", "-M", "no-aliases", file],
    );
    let mut blocks: Vec<Vec<String>> = Vec::new();
    let mut regs: BTreeMap<String, u64> = BTreeMap::new();
    for line in text.lines() {
        if let Some((address, rest)) = line.split_once(" <")
            && hex(address).is_some()
            && let Some(name) = rest.strip_suffix(">:")
        {
            // `__global_pointer$` lands wherever `.sdata` (or the image) starts.
            if name.starts_with(".L") || name == "__global_pointer$" {
                continue;
            }
            // IFUNC stubs of a static link: lld's are in `.iplt` and
            // carry the IFUNC's name, qld's are in `.plt` (as GNU ld's),
            // where the end of `.rela.iplt` may label them.
            let stub = hex(address)
                .and_then(|address| image.section_of(address))
                .is_some_and(|s| s.name == ".iplt" || (s.name == ".plt" && image.plt.is_empty()));
            let name = if stub { "IFUNC stubs" } else { name };
            regs.clear();
            blocks.push(vec![format!("{name}:")]);
            continue;
        }
        let Some((pc, insn)) = line.split_once(':') else {
            continue;
        };
        let Some(pc) = hex(pc.trim()) else { continue };
        let insn = insn.trim();
        let (op, args) = insn.split_once(char::is_whitespace).unwrap_or((insn, ""));
        // Padding (`.short 0x0000` after its raw bytes) depends on layout.
        if op.len() == 2 && hex(op).is_some() || op.starts_with('.') || op == "unimp" {
            continue;
        }
        let args = match args.find('<') {
            Some(at) => &args[..at],
            None => args,
        };
        let ops: Vec<String> = args
            .split(',')
            .map(|a| a.trim().to_string())
            .filter(|a| !a.is_empty())
            .collect();
        let rest = |n: usize| ops.get(..n).unwrap_or_default().join(",");
        let text = if (op == "auipc" || op == "lui") && ops.len() == 2 {
            let mut upper = imm(&ops[1]).unwrap_or(0) & 0xfffff;
            if upper & 0x80000 != 0 {
                upper -= 0x100000;
            }
            let base = if op == "auipc" { pc } else { 0 };
            regs.insert(ops[0].clone(), base.wrapping_add((upper << 12) as u64));
            format!("{op} {}", ops[0])
        } else if matches!(
            op,
            "jal"
                | "c.j"
                | "c.jal"
                | "beq"
                | "bne"
                | "blt"
                | "bge"
                | "bltu"
                | "bgeu"
                | "c.beqz"
                | "c.bnez"
        ) && !ops.is_empty()
        {
            let target = imm(ops.last().unwrap()).unwrap_or(0) as u64;
            format!(
                "{op} {} {}",
                rest(ops.len() - 1),
                image.symbolize(target, true)
            )
        } else if let Some(last) = ops.last()
            && let Some((offset, base)) = last.strip_suffix(')').and_then(|l| l.split_once('('))
            && let Some(offset) = imm(offset)
        {
            let text = if let Some(value) = regs.get(base) {
                let target = value.wrapping_add(offset as u64);
                format!(
                    "{op} {} {}",
                    rest(ops.len() - 1),
                    image.symbolize(target, true)
                )
            } else if base == "tp" && image.tls {
                format!(
                    "{op} {} {}",
                    rest(ops.len() - 1),
                    image.tls_symbolize(offset)
                )
            } else if base == "gp"
                && let Some(gp) = image.gp
            {
                format!(
                    "{op} {} {}",
                    rest(ops.len() - 1),
                    image.symbolize(gp.wrapping_add(offset as u64), true)
                )
            } else {
                format!("{op} {}", ops.join(", "))
            };
            if !op.starts_with('s') && !op.starts_with("fs") && !op.starts_with("c.s") {
                regs.remove(&ops[0]);
            }
            text
        } else if (op == "addi" || op == "addiw") && ops.len() == 3 {
            let offset = imm(&ops[2]).unwrap_or(0);
            // `lla gp, __global_pointer$`, which lands 0x800 past `.sdata`
            // wherever each linker puts it.
            let text = if ops[0] == "gp" {
                format!("{op} gp __global_pointer$")
            } else if let Some(value) = regs.get(&ops[1]) {
                let target = value.wrapping_add(offset as u64);
                format!("{op} {} {}", ops[0], image.symbolize(target, true))
            } else if ops[1] == "tp" && image.tls {
                format!("{op} {} {}", ops[0], image.tls_symbolize(offset))
            } else if ops[1] == "gp"
                && let Some(gp) = image.gp
            {
                format!(
                    "{op} {} {}",
                    ops[0],
                    image.symbolize(gp.wrapping_add(offset as u64), true)
                )
            } else {
                format!("{op} {}", ops.join(", "))
            };
            regs.remove(&ops[0]);
            text
        } else {
            if let Some(first) = ops.first() {
                regs.remove(first);
            }
            format!("{op} {}", ops.join(", "))
        };
        match blocks.last_mut() {
            Some(block) => block.push(format!("  {text}")),
            None => blocks.push(vec![format!("  {text}")]),
        }
    }
    blocks.sort();
    blocks
        .iter()
        .map(|b| b.join("\n"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Dynamic relocations and tags, symbol sizes, line rows, FDE ranges and
/// DWARF ranges of `file`, symbolized.
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
    // an executable (`_edata`, `__bss_start`, `_end`) are in qld's symbol
    // table whether or not anything refers to them; lld defines them only
    // when something does. Everything else must match.
    let always_defined = ["_DYNAMIC", "_edata", "__bss_start", "_end"];
    let mut sizes: Vec<String> = image
        .symbols
        .iter()
        .filter(|(_, _, name)| !always_defined.contains(&name.as_str()))
        .map(|(_, size, name)| format!("sym {name} size {size}"))
        .collect();
    sizes.sort();
    out.extend(sizes);
    let lines = run(dir, &tools.dwarfdump, &["--debug-line", file]);
    for line in String::from_utf8_lossy(&lines.stdout).lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let Some(address) = fields
            .first()
            .filter(|f| f.len() == 18)
            .and_then(|f| f.strip_prefix("0x"))
            .and_then(hex)
        else {
            continue;
        };
        let flags = fields.get(7..).unwrap_or_default().join(" ");
        let at = if flags.contains("end_sequence") {
            image.end(address)
        } else {
            image.symbolize(address, false)
        };
        out.push(format!(
            "line {at} {}:{} {flags}",
            fields.get(1).unwrap_or(&""),
            fields.get(2).unwrap_or(&"")
        ));
    }
    let frames = run(dir, &tools.dwarfdump, &["--eh-frame", file]);
    for line in String::from_utf8_lossy(&frames.stdout).lines() {
        if let Some((_, range)) = line.split_once(" pc=")
            && let Some((start, end)) = range.trim().split_once("...")
            && let (Some(start), Some(end)) = (hex(start), hex(end))
        {
            out.push(format!(
                "fde {}..{}",
                image.symbolize(start, false),
                image.end(end)
            ));
        }
    }
    let info = run(dir, &tools.dwarfdump, &["--debug-info", file]);
    for line in String::from_utf8_lossy(&info.stdout).lines() {
        for (attr, high) in [("DW_AT_low_pc", false), ("DW_AT_high_pc", true)] {
            if let Some((_, rest)) = line.split_once(attr)
                && let Some(value) = rest
                    .trim()
                    .strip_prefix("(0x")
                    .and_then(|v| v.strip_suffix(')'))
                    .and_then(hex)
            {
                let at = if high {
                    image.end(value)
                } else {
                    image.symbolize(value, false)
                };
                out.push(format!("info {attr} {at}"));
            }
        }
        if let Some((_, rest)) = line.split_once("[0x")
            && let Some((start, rest)) = rest.split_once(", 0x")
            && let Some((end, _)) = rest.split_once(')')
            && let (Some(start), Some(end)) = (hex(start), hex(end))
        {
            out.push(format!(
                "range {}..{}",
                image.symbolize(start, false),
                image.end(end)
            ));
        }
    }
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

/// The bytes of section `name` of `file`.
fn section_bytes(tools: &Tools, dir: &Path, file: &str, name: &str) -> Vec<u8> {
    let image = Image::load(tools, dir, file);
    let s = image
        .sections
        .iter()
        .find(|s| s.name == name)
        .unwrap_or_else(|| panic!("{file} has no {name}"));
    image.data[s.offset as usize..(s.offset + s.size) as usize].to_vec()
}

const MAIN_C: &str = r#"
extern int far_func(int);
extern int near_func(int);
extern __thread int tls_ext;
static __thread int tls_local = 3;
__thread int tls_big[1024] = {1};
int data_var = 5;
int bss_var;
static const char *strs[] = {"a", "bb", "ccc"};
int switchy(int x) {
  switch (x) {
    case 0: return near_func(1);
    case 1: return far_func(2);
    case 2: return 7;
    case 3: return 99;
    case 4: return data_var;
    case 5: return bss_var;
    case 6: return tls_local;
    default: return 0;
  }
}
__attribute__((aligned(64))) int aligned_fn(int x) { return x + tls_big[1000] + tls_ext; }
int tail(int x) { return near_func(x + 1); }
float scale(float x, double y) { return x * (float)y; }
void _start(void) {
  __asm__ volatile(".option push\n.option norelax\nlla gp, __global_pointer$\n.option pop");
  int r = switchy(3) + aligned_fn(2) + tail(4) + (int)strs[1][0] + (int)scale(2.0f, 3.0);
  for (;;) __asm__ volatile("" :: "r"(r));
}
"#;

const OTHER_C: &str = r#"
__thread int tls_ext = 9;
int near_func(int x) { return x * 2; }
"#;

/// More than jal's 1 MiB away from everything before it.
const FAR_S: &str = r#"
    .text
    .space 0x200000
    .globl far_func
    .type far_func,@function
far_func:
    addi a0, a0, -1
    ret
    .size far_func, .-far_func
"#;

/// The RV32 ABIs: `-march`, `-mabi`, and how `readelf -h` names the
/// floating-point ABI.
const ABIS: [(&str, &str, &str); 3] = [
    ("rv32gc", "ilp32d", "RVC, double-float ABI"),
    ("rv32gc", "ilp32f", "RVC, single-float ABI"),
    ("rv32gc", "ilp32", "0x1, RVC"),
];

#[test]
fn static_links_match_lld() {
    let tools = require!();
    let dir = scratch("static");
    compile(
        tools,
        &dir,
        "main.c",
        MAIN_C,
        "main-norelax.o",
        &["-mno-relax"],
    );
    compile(
        tools,
        &dir,
        "main.c",
        MAIN_C,
        "main-align.o",
        &["-falign-functions=16", "-falign-loops=16"],
    );
    compile(
        tools,
        &dir,
        "main.c",
        MAIN_C,
        "main-norvc.o",
        &["-march=rv32g"],
    );
    for (march, mabi, flags) in ABIS {
        let march_arg = format!("-march={march}");
        let mabi_arg = format!("-mabi={mabi}");
        let abi = [march_arg.as_str(), mabi_arg.as_str()];
        let main = format!("main-{mabi}.o");
        let other = format!("other-{mabi}.o");
        let far = format!("far-{mabi}.o");
        compile(tools, &dir, "main.c", MAIN_C, &main, &abi);
        compile(tools, &dir, "other.c", OTHER_C, &other, &abi);
        compile(tools, &dir, "far.s", FAR_S, &far, &abi);
        let name = format!("relaxed-{mabi}");
        let (lld, ours) = link_both(tools, &dir, &name, &["-static", &main, &other, &far]);
        assert_same(tools, &dir, &lld, &ours);
        // The ELF header keeps the objects' ABI flags.
        let header = run_ok(&dir, &tools.readelf, &["-h", &ours]);
        assert!(header.contains(flags), "{header}");
        assert!(header.contains("ELF32"), "{header}");
    }
    for (name, args) in [
        (
            "norelax",
            &[
                "-static",
                "main-norelax.o",
                "other-ilp32d.o",
                "far-ilp32d.o",
            ][..],
        ),
        (
            "ld-norelax",
            &[
                "-static",
                "--no-relax",
                "main-ilp32d.o",
                "other-ilp32d.o",
                "far-ilp32d.o",
            ],
        ),
        (
            "align",
            &["-static", "main-align.o", "other-ilp32d.o", "far-ilp32d.o"],
        ),
        (
            "norvc",
            &["-static", "main-norvc.o", "other-ilp32d.o", "far-ilp32d.o"],
        ),
        // `lui`/`auipc` pairs reaching above 2 GiB: RV32 arithmetic wraps.
        (
            "high",
            &[
                "-static",
                "--image-base=0x80000000",
                "main-ilp32d.o",
                "other-ilp32d.o",
                "far-ilp32d.o",
            ],
        ),
    ] {
        let (lld, ours) = link_both(tools, &dir, name, args);
        assert_same(tools, &dir, &lld, &ours);
    }
    // Relaxation happened: the near tail call is a `c.j`, the near call a
    // `c.jal` (RV32C only), the far tail call kept its `auipc`, and a
    // local-exec address fitting 12 bits is `tp` plus an immediate.
    let code = listing(tools, &dir, "relaxed-ilp32d.qld");
    assert!(code.contains("c.j  near_func"), "{code}");
    assert!(code.contains("  c.jal  "), "{code}");
    assert!(code.contains("auipc t1\n  jalr zero far_func"), "{code}");
    assert!(code.contains("addi a0 TLS(tls_big)"), "{code}");
    let code = listing(tools, &dir, "norvc.qld");
    assert!(code.contains("jal ra near_func"), "{code}");
    let segments = run_ok(&dir, &tools.readelf, &["-lW", "high.qld"]);
    assert!(segments.contains("0x80000000"), "{segments}");
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
int (*fp)(int) = lib_func;
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

const IFUNC_C: &str = r#"
static int impl_one(void) { return 1; }
static int impl_two(void) { return 2; }
static void *resolve_pick(void) { return (void *)impl_two; }
int pick(void) __attribute__((ifunc("resolve_pick")));
int (*pointer)(void) = pick;
static int unused_one(void) { return impl_one(); }
int (*keep)(void) = unused_one;
void _start(void) {
  int r = pick() + pointer();
  for (;;) __asm__ volatile("" :: "r"(r));
}
"#;

#[test]
fn dynamic_links_match_lld() {
    let tools = require!();
    let dir = scratch("dynamic");
    compile(tools, &dir, "lib.c", LIB_C, "lib.o", &["-fPIC"]);
    compile(
        tools,
        &dir,
        "lib.c",
        LIB_C,
        "lib-desc.o",
        &["-fPIC", "-mtls-dialect=desc"],
    );
    compile(tools, &dir, "dep.c", DEP_C, "dep.o", &["-fPIC"]);
    compile(tools, &dir, "exe.c", EXE_C, "exe-pie.o", &["-fPIE"]);
    compile(
        tools,
        &dir,
        "exe.c",
        EXE_C,
        "exe-desc.o",
        &["-fPIE", "-mtls-dialect=desc"],
    );
    compile(tools, &dir, "exe.c", EXE_C, "exe-nopic.o", &["-fno-pic"]);
    compile(tools, &dir, "ifunc.c", IFUNC_C, "ifunc.o", &["-fPIE"]);
    compile(
        tools,
        &dir,
        "tga.c",
        "void *__tls_get_addr(void *p) { return p; }\n",
        "tga.o",
        &[],
    );
    run_ok(
        &dir,
        &tools.lld,
        &[
            "-shared",
            "dep.o",
            "-soname",
            "libdep.so",
            "-o",
            "libdep.so",
        ],
    );
    for (name, args) in [
        (
            "libfoo.so",
            &["-shared", "lib.o", "libdep.so", "-soname", "libfoo.so"][..],
        ),
        (
            "libdesc.so",
            &[
                "-shared",
                "lib-desc.o",
                "libdep.so",
                "-soname",
                "libdesc.so",
            ],
        ),
        (
            "libnow.so",
            &[
                "-shared",
                "-z",
                "now",
                "lib.o",
                "libdep.so",
                "-soname",
                "libnow.so",
            ],
        ),
    ] {
        let (lld, ours) = link_both(tools, &dir, name, args);
        assert_same(tools, &dir, &lld, &ours);
    }
    fs::copy(dir.join("libfoo.so.lld"), dir.join("libfoo.so")).unwrap();
    fs::copy(dir.join("libdesc.so.lld"), dir.join("libdesc.so")).unwrap();
    for (name, args) in [
        (
            "pie",
            &[
                "-pie",
                "--allow-shlib-undefined",
                "exe-pie.o",
                "libfoo.so",
                "libdep.so",
            ][..],
        ),
        (
            "piedesc",
            &["-pie", "exe-desc.o", "libdesc.so", "libdep.so"],
        ),
        (
            "nopie",
            &[
                "--allow-shlib-undefined",
                "exe-nopic.o",
                "libfoo.so",
                "libdep.so",
            ],
        ),
        (
            "static-gd",
            &["-static", "exe-pie.o", "dep.o", "lib.o", "tga.o"],
        ),
        (
            "static-desc",
            &["-static", "exe-desc.o", "dep.o", "lib-desc.o"],
        ),
        ("static-ifunc", &["-static", "ifunc.o"]),
        ("pie-ifunc", &["-pie", "ifunc.o"]),
    ] {
        let (lld, ours) = link_both(tools, &dir, name, args);
        assert_same(tools, &dir, &lld, &ours);
    }
    // Words are 32 bits: GOT slots, `.got.plt` slots and TLS entries take
    // 32-bit dynamic relocations, and nothing 64-bit is left.
    let pie = listing(tools, &dir, "piedesc.qld");
    assert!(pie.contains("GOT[R_RISCV_TLS_TPREL32 lib_tls"), "{pie}");
    assert!(pie.contains("TLS(exe_tls)"), "{pie}");
    let lib = run_ok(&dir, &tools.readelf, &["-rW", "libdesc.so.qld"]);
    assert!(lib.contains("R_RISCV_TLSDESC"), "{lib}");
    for file in ["libfoo.so.qld", "pie.qld", "nopie.qld"] {
        let relocs = run_ok(&dir, &tools.readelf, &["-rW", file]);
        let wide = relocs
            .split_whitespace()
            .any(|w| w.starts_with("R_RISCV_") && w.ends_with("64"));
        assert!(!wide, "{file}:\n{relocs}");
    }
    let lib = run_ok(&dir, &tools.readelf, &["-rW", "libfoo.so.qld"]);
    for kind in [
        "R_RISCV_JUMP_SLOT",
        "R_RISCV_32",
        "R_RISCV_TLS_DTPMOD32",
        "R_RISCV_TLS_DTPREL32",
    ] {
        assert!(lib.contains(kind), "{kind}:\n{lib}");
    }
    // The PLT loads its `.got.plt` words with `lw`.
    let plt = run_ok(&dir, &tools.objdump, &["-d", "-j", ".plt", "libfoo.so.qld"]);
    assert!(plt.contains("lw\tt3"), "{plt}");
    assert!(!plt.contains("\tld\t"), "{plt}");
    // The program interpreter is the ILP32D one.
    let interp = run_ok(&dir, &tools.readelf, &["-lW", "nopie.qld"]);
    assert!(
        interp.contains("/lib/ld-linux-riscv32-ilp32d.so.1"),
        "{interp}"
    );
}

/// A deterministic generator (xorshift) for the stress test.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len() as u64) as usize]
    }
}

/// Assembly with calls and tail calls around the `jal`, `c.jal` and `c.j`
/// limits, alignment directives, local-exec TLS accesses, absolute
/// addresses, branches over relaxed calls and label differences in data.
fn stress_source(seed: u64, count: usize, rvc: bool) -> String {
    let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
    let mut out = String::from(".option relax\n.section .tbss,\"awT\",@nobits\n.p2align 3\n");
    for i in 0..8 {
        out += &format!("tv{i}: .zero {}\n", rng.pick(&[4, 8, 1024, 4096]));
    }
    out += ".text\n";
    let names: Vec<String> = (0..count).map(|i| format!("f{i}")).collect();
    for name in &names {
        out += &format!(".globl {name}\n.type {name},@function\n");
        if rng.below(10) < 3 {
            out += &format!(".p2align {}\n", rng.pick(&[2, 3, 4, 5, 6]));
        }
        out += &format!("{name}:\n");
        for _ in 0..=rng.below(7) {
            let target = rng.pick(&names).clone();
            match rng.below(11) {
                0..=3 => out += &format!("  call {target}\n"),
                4 => out += &format!("  tail {target}\n"),
                5 => {
                    let tv = rng.below(8);
                    out += &format!(
                        "  lui a1, %tprel_hi(tv{tv})\n  add a1, a1, tp, %tprel_add(tv{tv})\n  lw a0, %tprel_lo(tv{tv})(a1)\n"
                    );
                }
                6 => {
                    out += &format!(".p2align {}\n  addi a0, a0, 1\n", rng.pick(&[2, 3, 4]));
                }
                7 => out += &format!("  lla a2, {target}\n"),
                8 if rvc => out += "  c.addi a0, 1\n",
                8 => out += "  addi a0, a0, 1\n",
                9 => out += &format!("  lui a3, %hi({target})\n  addi a3, a3, %lo({target})\n"),
                _ => out += &format!("  beqz a0, 4f\n  call {target}\n4:\n"),
            }
        }
        let gap = *rng.pick(&[0, 0, 0, 16, 2000, 3000, 70000, 300_000]);
        if gap != 0 {
            out += &format!("  .space {gap}\n");
        }
        out += &format!("  ret\n.size {name}, .-{name}\n");
    }
    out += ".section .rodata\ntable:\n";
    for pair in names.chunks(2) {
        if let [a, b] = pair {
            out += &format!("  .word {b} - {a}\n  .uleb128 {b} - {a}\n  .byte 0\n");
        }
    }
    out
}

#[test]
fn relaxation_stress_matches_lld() {
    let tools = require!();
    let dir = scratch("stress");
    compile(
        tools,
        &dir,
        "start.s",
        ".globl _start\n_start: call f0\n j _start\n",
        "start.o",
        &[],
    );
    for seed in 1..=10u64 {
        let march = if seed > 7 {
            "-march=rv32g"
        } else {
            "-march=rv32gc"
        };
        let source = format!("s{seed}.s");
        let object = format!("s{seed}.o");
        compile(
            tools,
            &dir,
            &source,
            &stress_source(seed, 120, seed <= 7),
            &object,
            &[march],
        );
        let (lld, ours) = link_both(
            tools,
            &dir,
            &format!("s{seed}"),
            &["-static", "start.o", &object],
        );
        assert_same(tools, &dir, &lld, &ours);
        // Label differences across relaxed code, byte for byte, and the
        // same amount of code deleted.
        assert_eq!(
            section_bytes(tools, &dir, &lld, ".rodata"),
            section_bytes(tools, &dir, &ours, ".rodata"),
            "seed {seed}: label differences"
        );
        assert_eq!(
            section_bytes(tools, &dir, &lld, ".text").len(),
            section_bytes(tools, &dir, &ours, ".text").len(),
            "seed {seed}: relaxed size"
        );
    }
}

/// The value of `Tag_RISCV_arch` in `file`, as `readelf -A` prints it.
fn arch_attribute(tools: &Tools, dir: &Path, file: &str) -> String {
    run_ok(dir, &tools.readelf, &["-A", file])
        .lines()
        .find(|l| l.trim_start().starts_with("Value: rv"))
        .map(|l| l.trim().to_string())
        .unwrap_or_default()
}

#[test]
fn attributes_are_merged() {
    let tools = require!();
    let dir = scratch("attributes");
    compile(
        tools,
        &dir,
        "a.c",
        "int a(void) { return 1; }\n",
        "a.o",
        &[],
    );
    compile(
        tools,
        &dir,
        "b.c",
        "int b(int x) { return x << 3; }\nvoid _start(void) { for (;;); }\n",
        "b.o",
        &["-march=rv32gc_zba_zbb"],
    );
    compile(
        tools,
        &dir,
        "c.c",
        "int c(int x) { return x + 1; }\n",
        "c.o",
        &["-march=rv32imac_zicsr", "-mabi=ilp32"],
    );
    compile(
        tools,
        &dir,
        "d.c",
        "int d(int x) { return x - 1; }\n",
        "d.o",
        &["-march=rv32imac_zicsr_zbs", "-mabi=ilp32"],
    );
    let (lld, ours) = link_both(tools, &dir, "attrs", &["-static", "a.o", "b.o"]);
    let expected = arch_attribute(tools, &dir, &lld);
    assert!(expected.contains("Value: rv32i"), "{expected}");
    assert!(expected.contains("zba"), "{expected}");
    assert_eq!(arch_attribute(tools, &dir, &ours), expected);
    let (lld, ours) = link_both(tools, &dir, "soft", &["-static", "c.o", "d.o"]);
    let expected = arch_attribute(tools, &dir, &lld);
    assert!(expected.contains("zbs"), "{expected}");
    assert!(!expected.contains("_f2"), "{expected}");
    assert_eq!(arch_attribute(tools, &dir, &ours), expected);
    let segments = run_ok(&dir, &tools.readelf, &["-lW", &ours]);
    assert!(
        segments.contains("RISCV_ATTRIBUTES") || segments.contains("ATTRIBUTES"),
        "{segments}"
    );
}

/// Links `objects` with qld and returns its standard error, asserting that
/// the link failed.
fn link_fails(dir: &Path, objects: &[&str]) -> String {
    let mut args = vec!["-static"];
    args.extend_from_slice(objects);
    args.extend_from_slice(&["-o", "out"]);
    let output = run(dir, Path::new(env!("CARGO_BIN_EXE_qld")), &args);
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(!output.status.success(), "{objects:?} linked:\n{stderr}");
    stderr
}

#[test]
fn incompatible_objects_are_rejected() {
    let tools = require!();
    let dir = scratch("abi");
    let start = "void _start(void) { for (;;); }\n";
    let b = "int b(void) { return 2; }\n";
    compile(tools, &dir, "a.c", start, "a-ilp32d.o", &[]);
    for (march, mabi) in [
        ("rv32imafc", "ilp32f"),
        ("rv32imac", "ilp32"),
        ("rv32gc", "ilp32"),
    ] {
        let object = format!("b-{mabi}-{march}.o");
        compile(
            tools,
            &dir,
            "b.c",
            b,
            &object,
            &[&format!("-march={march}"), &format!("-mabi={mabi}")],
        );
        let stderr = link_fails(&dir, &["a-ilp32d.o", &object]);
        assert!(stderr.contains("floating-point ABI"), "{stderr}");
    }
    // The same ABI with other extensions links.
    compile(
        tools,
        &dir,
        "b.c",
        b,
        "b-ilp32d.o",
        &["-march=rv32imafdc_zba"],
    );
    run_ok(
        &dir,
        Path::new(env!("CARGO_BIN_EXE_qld")),
        &["-static", "a-ilp32d.o", "b-ilp32d.o", "-o", "ok"],
    );
    // RV32E (ILP32E) against RV32I.
    compile(
        tools,
        &dir,
        "a.c",
        start,
        "a-ilp32.o",
        &["-march=rv32imac", "-mabi=ilp32"],
    );
    compile(
        tools,
        &dir,
        "b.c",
        b,
        "b-ilp32e.o",
        &["-march=rv32ec", "-mabi=ilp32e"],
    );
    let stderr = link_fails(&dir, &["a-ilp32.o", "b-ilp32e.o"]);
    assert!(stderr.contains("EF_RISCV_RVE"), "{stderr}");
    // An RV64 object in an RV32 link.
    fs::write(dir.join("c.c"), b).unwrap();
    run_ok(
        &dir,
        &tools.cc,
        &[
            "--target=riscv64-linux-gnu",
            "-march=rv64gc",
            "-mabi=lp64d",
            "-c",
            "c.c",
            "-o",
            "c64.o",
        ],
    );
    let stderr = link_fails(&dir, &["a-ilp32d.o", "c64.o"]);
    assert!(stderr.contains("c64.o"), "{stderr}");
}

/// The relocations of `.rela.text` in `file`, each place symbolized.
fn emitted_relocs(tools: &Tools, dir: &Path, file: &str) -> Vec<String> {
    let image = Image::load(tools, dir, file);
    let text = run_ok(dir, &tools.readelf, &["-rW", file]);
    let mut in_text = false;
    let mut out = Vec::new();
    for line in text.lines() {
        if line.starts_with("Relocation section") {
            in_text = line.contains("'.rela.text'");
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        let [place, _, kind, rest @ ..] = fields.as_slice() else {
            continue;
        };
        let Some(place) = hex(place).filter(|_| in_text && place.len() == 8) else {
            continue;
        };
        let symbol = match rest {
            [_, name, ..] => (*name).to_string(),
            _ => String::new(),
        };
        out.push(format!("{} {kind} {symbol}", image.symbolize(place, false)));
    }
    out
}

#[test]
fn other_output_kinds_match_lld() {
    let tools = require!();
    let dir = scratch("kinds");
    compile(
        tools,
        &dir,
        "main.c",
        MAIN_C,
        "main.o",
        &["-ffunction-sections"],
    );
    compile(tools, &dir, "other.c", OTHER_C, "other.o", &[]);
    compile(tools, &dir, "far.s", FAR_S, "far-ilp32d.o", &[]);
    fs::write(
        dir.join("script.ld"),
        "SECTIONS {\n  . = 0x80000000;\n  .text : { *(.text .text.*) }\n  .rodata : { *(.rodata .rodata.*) }\n  .sdata : { __global_pointer$ = . + 0x800; *(.sdata .sdata.*) }\n  .data : { *(.data .data.*) }\n  .tdata : { *(.tdata .tdata.*) }\n  .tbss : { *(.tbss .tbss.*) }\n  .bss : { *(.bss .bss.*) }\n}\n",
    )
    .unwrap();
    for (name, args) in [
        (
            "gc",
            &[
                "-static",
                "--gc-sections",
                "main.o",
                "other.o",
                "far-ilp32d.o",
            ][..],
        ),
        (
            "script",
            &[
                "-static",
                "-T",
                "script.ld",
                "main.o",
                "other.o",
                "far-ilp32d.o",
            ],
        ),
    ] {
        let (lld, ours) = link_both(tools, &dir, name, args);
        assert_same(tools, &dir, &lld, &ours);
    }

    // `--emit-relocs` writes the relocations at their relaxed places, with
    // the types of the relaxed instructions.
    let (lld, ours) = link_both(
        tools,
        &dir,
        "emit",
        &[
            "-static",
            "--emit-relocs",
            "main.o",
            "other.o",
            "far-ilp32d.o",
        ],
    );
    assert_same(tools, &dir, &lld, &ours);
    let expected = emitted_relocs(tools, &dir, &lld);
    assert!(expected.iter().any(|r| r.contains("R_RISCV_RVC_JUMP")));
    assert_eq!(emitted_relocs(tools, &dir, &ours), expected);
}

const SMALL_DATA_C: &str = r#"
int counter = 1;
short flags = 2;
static int hidden_total;
long long big_table[512];
int small_array[2];
int bump(int x) { counter += x; hidden_total += flags; return counter + small_array[1]; }
long long peek(int i) { return big_table[i]; }
void _start(void) {
  __asm__ volatile(".option push\n.option norelax\nlla gp, __global_pointer$\n.option pop");
  long long r = bump(3) + peek(7) + hidden_total;
  for (;;) __asm__ volatile("" :: "r"((int)r));
}
"#;

#[test]
fn global_pointer_relaxation_matches_lld() {
    let tools = require!();
    let dir = scratch("gp");
    compile(
        tools,
        &dir,
        "small.c",
        SMALL_DATA_C,
        "small.o",
        &["-fno-pic", "-mcmodel=medlow", "-msmall-data-limit=8"],
    );
    for (name, args) in [
        ("default", &["-static", "small.o"][..]),
        (
            "no-relax",
            &["-static", "--relax-gp", "--no-relax", "small.o"],
        ),
    ] {
        let (lld, ours) = link_both(tools, &dir, name, args);
        assert_same(tools, &dir, &lld, &ours);
    }
    // `__global_pointer$` is defined 0x800 past `.sdata`, as on RV64.
    let symbols = run_ok(&dir, &tools.readelf, &["-sW", "default.qld"]);
    assert!(symbols.contains("__global_pointer$"), "{symbols}");
    // With `--relax-gp`, `lui` + `%lo` pairs of data within 2 KiB of
    // `__global_pointer$` become `gp`-relative accesses to the same
    // variables. Which variables are that close depends on where each
    // linker puts `.sbss`, so the relaxed code is checked against the
    // unrelaxed one rather than against lld's.
    let (_, ours) = link_both(
        tools,
        &dir,
        "relax-gp",
        &["-static", "--relax-gp", "small.o"],
    );
    let without_lui = |file: &str| {
        listing(tools, &dir, file)
            .lines()
            .filter(|l| !l.starts_with("  lui ") && !l.starts_with("  c.j"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert_eq!(without_lui(&ours), without_lui("default.qld"));
    let objdump = run_ok(&dir, &tools.objdump, &["-d", &ours]);
    assert!(objdump.contains("(gp)"), "{objdump}");
    let lui = |file: &str| listing(tools, &dir, file).matches("  lui ").count();
    assert!(lui(&ours) < lui("default.qld"));
}

/// A call that relaxes, then a label; `.data` points at the label once by
/// name and once as `.text` plus its offset (a section symbol, as some
/// assemblers write local references).
const SECTION_SYMBOL_S: &str = r#"
    .option norvc
    .text
    .globl _start
_start:
    call callee
    nop
target:
    j _start
callee:
    ret
    .data
    .globl by_name
by_name:
    .word target
    .globl by_section
by_section:
    .reloc ., R_RISCV_32, .text+12
    .word 0
"#;

#[test]
fn section_symbol_offsets_follow_relaxation() {
    let tools = require!();
    let dir = scratch("section-symbol");
    compile(
        tools,
        &dir,
        "s.s",
        SECTION_SYMBOL_S,
        "s.o",
        &["-march=rv32g"],
    );
    let ours = "s.qld";
    run_ok(
        &dir,
        Path::new(env!("CARGO_BIN_EXE_qld")),
        &["--threads=2", "-static", "s.o", "-o", ours],
    );
    let image = Image::load(tools, &dir, ours);
    let word = |name: &str| {
        let (value, _, _) = image
            .symbols
            .iter()
            .find(|(_, _, n)| n == name)
            .unwrap_or_else(|| panic!("no {name}"));
        image.word(*value).unwrap()
    };
    // The call shrank to a `jal`, so `target` moved back four bytes, and
    // both references follow it.
    assert_eq!(word("by_section"), word("by_name"));
    let code = listing(tools, &dir, ours);
    assert!(code.contains("jal ra callee"), "{code}");
}

/// `qemu-riscv32`, if installed (`QLD_QEMU_RISCV32`, then `PATH`). With
/// `QLD_REQUIRE_RISCV32_QEMU=1` a missing one fails the test.
fn qemu() -> Option<PathBuf> {
    let found = tool("QLD_QEMU_RISCV32", "qemu-riscv32").or_else(|| in_path("qemu-riscv32-static"));
    if found.is_none() {
        let required =
            std::env::var_os("QLD_REQUIRE_RISCV32_QEMU").is_some_and(|v| !v.is_empty() && v != "0");
        assert!(
            !required,
            "QLD_REQUIRE_RISCV32_QEMU is set but there is no qemu-riscv32"
        );
        println!("SKIPPED: no qemu-riscv32");
    }
    found
}

/// The entry point: `gp`, then the C start-up with the initial stack.
const CRT_S: &str = r#"
    .text
    .globl _start
    .type _start,@function
_start:
    .option push
    .option norelax
    lla gp, __global_pointer$
    .option pop
    mv a0, sp
    call cstart
1:  j 1b
"#;

/// What a C library would do for a static program: set up the thread
/// pointer from `PT_TLS` (found through the auxiliary vector), apply the
/// `IRELATIVE` relocations of `.rela.iplt`, and exit with `run()`'s
/// status. Output goes through Linux system calls.
const SYS_C: &str = r#"
typedef unsigned int u32;
typedef struct { u32 p_type, p_offset, p_vaddr, p_paddr, p_filesz, p_memsz, p_flags, p_align; } Phdr;
typedef struct { u32 r_offset, r_info; int r_addend; } Rela;
extern int run(void);
#if WITH_IFUNC
extern const Rela __rela_iplt_start[] __attribute__((weak));
extern const Rela __rela_iplt_end[] __attribute__((weak));
#endif

static long sys3(long n, long a, long b, long c) {
  register long a0 __asm__("a0") = a;
  register long a1 __asm__("a1") = b;
  register long a2 __asm__("a2") = c;
  register long a7 __asm__("a7") = n;
  __asm__ volatile("ecall" : "+r"(a0) : "r"(a1), "r"(a2), "r"(a7) : "memory");
  return a0;
}

void *memcpy(void *d, const void *s, unsigned n) {
  volatile unsigned char *dp = d;
  const volatile unsigned char *sp = s;
  while (n--) *dp++ = *sp++;
  return d;
}

void *memset(void *d, int c, unsigned n) {
  volatile unsigned char *dp = d;
  while (n--) *dp++ = (unsigned char)c;
  return d;
}

void out(const char *s) {
  u32 n = 0;
  while (s[n]) n++;
  sys3(64, 1, (long)s, n);
}

__attribute__((noreturn)) void quit(int code) {
  sys3(93, code, 0, 0);
  for (;;) {}
}

/* glibc's convention: the offset is stored minus 0x800. */
void *__tls_get_addr(u32 *ti) {
  char *tp;
  __asm__("mv %0, tp" : "=r"(tp));
  if (ti[0] != 1) quit(90);
  return tp + ti[1] + 0x800;
}

static unsigned char tls_area[8192] __attribute__((aligned(64)));

void cstart(u32 *sp) {
  u32 argc = sp[0];
  u32 *p = sp + 1 + argc + 1;
  while (*p) p++;
  p++;
  const Phdr *phdr = 0;
  u32 phnum = 0;
  for (; p[0]; p += 2) {
    if (p[0] == 3) phdr = (const Phdr *)p[1];
    if (p[0] == 5) phnum = p[1];
  }
  for (u32 i = 0; i < phnum; i++) {
    if (phdr[i].p_type != 7) continue;
    if (phdr[i].p_memsz > sizeof tls_area || phdr[i].p_align > 64) quit(91);
    memcpy(tls_area, (const void *)phdr[i].p_vaddr, phdr[i].p_filesz);
    memset(tls_area + phdr[i].p_filesz, 0, phdr[i].p_memsz - phdr[i].p_filesz);
  }
  __asm__ volatile("mv tp, %0" :: "r"(tls_area));
#if WITH_IFUNC
  for (const Rela *r = __rela_iplt_start; r < __rela_iplt_end; r++) {
    u32 (*resolver)(void) = (u32 (*)(void))r->r_addend;
    *(u32 *)r->r_offset = resolver();
  }
#endif
  quit(run());
}
"#;

/// The checks: relaxed and far calls, a jump table, data, local-exec,
/// general-dynamic and descriptor TLS agreeing on addresses, an IFUNC and
/// floating-point arguments across objects.
const APP_C: &str = r#"
extern void out(const char *);
extern int far_func(int);
extern int near_func(int);
extern __thread int tls_ext;
extern int *gd_addr_of_ext(void);
extern int gd_read_local(void);
extern int *desc_addr_of_ext(void);
extern int desc_read(void);
/* The IFUNC is called through a pointer its own object initializes: lld
   places the stubs after the far code, qld before `.text` (as GNU ld), so
   a direct call would relax differently; and lld gives an IFUNC whose
   address is taken a canonical PLT entry in `.symtab`. Only the
   `WITH_IFUNC` variant has it: `__rela_iplt_end` lands in a different
   output section in each linker, which no symbolic comparison survives. */
#if WITH_IFUNC
extern int (*volatile pick_pointer)(void);
#endif
extern float fscale(float, float);
extern double dmix(double, int);
static volatile float float_in = 1.5f;
static volatile double double_in = 2.5;
static __thread int tls_local = 3;
__thread int tls_big[1024] = {1};
__thread int tls_zero;
int data_var = 5;
int bss_var;
static const char *strs[] = {"a", "bb", "ccc"};
static int (*const table[])(int) = {near_func, far_func};
__attribute__((noinline)) int switchy(int x) {
  switch (x) {
    case 0: return near_func(1);
    case 1: return far_func(2);
    case 2: return 7;
    case 3: return 99;
    case 4: return data_var;
    case 5: return bss_var;
    case 6: return tls_local;
    default: return 0;
  }
}
static int failures;
static void check(int ok, const char *what) {
  if (!ok) { out("FAIL "); out(what); out("\n"); failures++; }
}
int run(void) {
  check(switchy(0) == 2, "near call");
  check(switchy(1) == 1, "far call");
  check(switchy(2) == 7 && switchy(3) == 99, "jump table");
  check(switchy(4) == 5, "data");
  check(switchy(5) == 0, "bss");
  check(switchy(6) == 3, "local-exec TLS");
  check(table[0](4) == 8 && table[1](4) == 3, "function pointers");
  check(tls_big[0] == 1 && tls_big[1000] == 0 && tls_zero == 0, "TLS image");
  tls_big[1000] = 17;
  tls_zero = 4;
  check(tls_big[1000] == 17 && tls_zero == 4, "TLS stores");
  check(tls_ext == 9, "TLS in another object");
  check(gd_addr_of_ext() == &tls_ext, "general-dynamic address");
  check(desc_addr_of_ext() == &tls_ext, "descriptor address");
  tls_ext = 21;
  check(gd_read_local() == 11, "general-dynamic local");
  check(desc_read() == 33, "descriptor read");
#if WITH_IFUNC
  check(pick_pointer() == 2, "IFUNC");
#endif
  check(fscale(float_in, 4.0f) == 6.0f, "float arguments");
  check(dmix(double_in, 3) == 7.5, "double arguments");
  check(strs[2][2] == 'c' && strs[1][0] == 'b', "string pointers");
  if (failures) return 1;
  out("rv32 ok\n");
  return 0;
}
"#;

const LIB_RUN_C: &str = r#"
__thread int tls_ext = 9;
int near_func(int x) { return x * 2; }
float fscale(float x, float y) { return x * y; }
double dmix(double x, int n) { return x * n; }
#if WITH_IFUNC
static int impl_one(void) { return 1; }
static int impl_two(void) { return 2; }
/* Opaque to the compiler, which would otherwise fold the pointer below to
   `impl_two` and leave no IFUNC at all. */
static volatile int choice = 1;
static void *resolve_pick(void) { return choice ? (void *)impl_two : (void *)impl_one; }
int pick(void) __attribute__((ifunc("resolve_pick")));
int (*volatile pick_pointer)(void) = pick;
int (*keep)(void) = impl_one;
#endif
"#;

/// Compiled `-fPIC`: general-dynamic accesses, relaxed to nothing in a
/// static link and resolved through `__tls_get_addr`.
const GD_C: &str = r#"
extern __thread int tls_ext;
static __thread int gd_local = 11;
int *gd_addr_of_ext(void) { return &tls_ext; }
int gd_read_local(void) { return gd_local; }
"#;

/// Compiled `-fPIC -mtls-dialect=desc`: descriptors, relaxed to local-exec.
const DESC_C: &str = r#"
extern __thread int tls_ext;
static __thread int desc_local = 12;
int *desc_addr_of_ext(void) { return &tls_ext; }
int desc_read(void) { return tls_ext + desc_local; }
"#;

#[test]
fn freestanding_programs_run() {
    let tools = require!();
    // The links are compared with lld even where nothing can run them.
    let qemu = qemu();
    let dir = scratch("run");
    // Name, compiler flags, linker flags, and whether the two links are
    // compared as well as run.
    let variants: [(&str, &[&str], &[&str], bool); 7] = [
        ("ilp32d", &[], &[], true),
        ("ilp32f", &["-march=rv32gc", "-mabi=ilp32f"], &[], true),
        ("ilp32", &["-march=rv32gc", "-mabi=ilp32"], &[], true),
        ("norvc", &["-march=rv32g"], &[], true),
        ("norelax", &[], &["--no-relax"], true),
        ("high", &[], &["--image-base=0x80000000"], true),
        ("ifunc", &["-DWITH_IFUNC=1"], &[], false),
    ];
    for (name, cflags, ldflags, compare) in variants {
        let objects: Vec<String> = [
            ("crt.s", CRT_S, &[][..]),
            ("sys.c", SYS_C, &[][..]),
            ("app.c", APP_C, &["-fPIE"][..]),
            ("lib.c", LIB_RUN_C, &["-fPIE"][..]),
            ("gd.c", GD_C, &["-fPIC"][..]),
            ("desc.c", DESC_C, &["-fPIC", "-mtls-dialect=desc"][..]),
            ("far.s", FAR_S, &[][..]),
        ]
        .iter()
        .map(|(source, text, extra)| {
            let object = format!(
                "{name}-{}.o",
                source.trim_end_matches(".c").trim_end_matches(".s")
            );
            let mut flags = cflags.to_vec();
            flags.extend_from_slice(extra);
            compile(tools, &dir, source, text, &object, &flags);
            object
        })
        .collect();
        let mut args: Vec<&str> = vec!["-static"];
        args.extend_from_slice(ldflags);
        args.extend(objects.iter().map(String::as_str));
        let (lld, ours) = link_both(tools, &dir, name, &args);
        if compare {
            assert_same(tools, &dir, &lld, &ours);
        }
        for file in qemu.iter().flat_map(|_| [&lld, &ours]) {
            let qemu = qemu.as_deref().unwrap();
            let output = run(&dir, qemu, &[&format!("./{file}")]);
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                output.status.success() && stdout == "rv32 ok\n",
                "{file}: {:?}\n{stdout}{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
        }
        // The same output with one thread.
        let mut single = vec!["--threads=1"];
        single.extend_from_slice(&args);
        single.extend_from_slice(&["-o", "single.qld"]);
        run_ok(&dir, Path::new(env!("CARGO_BIN_EXE_qld")), &single);
        assert_eq!(
            fs::read(dir.join("single.qld")).unwrap(),
            fs::read(dir.join(&ours)).unwrap(),
            "{name}: output depends on the thread count"
        );
    }
}
