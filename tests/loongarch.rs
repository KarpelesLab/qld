//! LoongArch64 ELF tests (workstream W32).
//!
//! These run on any host that has a clang with the LoongArch backend and
//! `ld.lld`, without being able to *run* LoongArch binaries: every test
//! links the same objects with qld and with lld and compares what the two
//! produced. Since the two linkers lay out sections differently, the code
//! is compared by meaning rather than by bytes: each function is decoded,
//! PC-relative sequences (`pcalau12i` + `addi.d`/`ld.d`, `pcaddu18i` +
//! `jirl`, branches) are evaluated to the address they reach, and that
//! address is described by what is there (`counter+4`, a GOT slot holding
//! `R_LARCH_64 lib_data`, the PLT entry of `lib_func`, a string). The
//! dynamic metadata (`e_flags`, interpreter, `DT_NEEDED`, dynamic symbols
//! and relocations) is compared the same way.
//!
//! Objects built with `-mno-relax` are compared with `ld.lld --no-relax`
//! instruction for instruction. Objects built with linker relaxation are
//! compared with lld's relaxed output with `nop`s ignored: qld performs the
//! same rewrites but does not delete the bytes they free (see
//! `src/elf/arch/loongarch.rs`).
//!
//! Tests that need libc and qemu are fixtures (`tests/fixtures/loongarch64-*`).
//!
//! Tools: `QLD_TEST_LOONGARCH_CC` (default `clang`) and `QLD_TEST_LLD`
//! (default `ld.lld`). A test prints `SKIPPED:` and passes when one is
//! missing, unless `QLD_REQUIRE_LOONGARCH` is set.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Finds `name` in `PATH` (or takes it as a path).
fn find(name: &str) -> Option<PathBuf> {
    let candidate = Path::new(name);
    if candidate.components().count() > 1 {
        return candidate.is_file().then(|| candidate.to_path_buf());
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

fn tool(var: &str, default: &str) -> Option<PathBuf> {
    match std::env::var(var) {
        Ok(value) if value.is_empty() => None,
        Ok(value) => find(&value),
        Err(_) => find(default),
    }
}

/// The toolchain a test needs.
struct Tools {
    clang: PathBuf,
    lld: PathBuf,
}

fn tools() -> Result<Tools, String> {
    let clang = tool("QLD_TEST_LOONGARCH_CC", "clang").ok_or("no clang")?;
    let lld = tool("QLD_TEST_LLD", "ld.lld").ok_or("no ld.lld")?;
    // The clang must have the LoongArch backend.
    let probe = Command::new(&clang)
        .args([
            "--target=loongarch64-linux-gnu",
            "-x",
            "c",
            "-c",
            "-o",
            "/dev/null",
            "-",
        ])
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("cannot run {}: {e}", clang.display()))?;
    if !probe.status.success() {
        return Err(format!("{} has no LoongArch backend", clang.display()));
    }
    Ok(Tools { clang, lld })
}

macro_rules! require {
    () => {
        match tools() {
            Ok(tools) => tools,
            Err(why) => {
                assert!(
                    std::env::var_os("QLD_REQUIRE_LOONGARCH").is_none(),
                    "QLD_REQUIRE_LOONGARCH is set but the tools are missing: {why}"
                );
                println!("SKIPPED: {why} (set QLD_TEST_LOONGARCH_CC and QLD_TEST_LLD)");
                return;
            }
        }
    };
}

/// A fresh, empty directory for one test.
fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("loongarch-tests")
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
        "`{} {}` failed:\n{}{}",
        program.display(),
        args.join(" "),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn qld(dir: &Path, args: &[&str]) -> Output {
    run(dir, Path::new(env!("CARGO_BIN_EXE_qld")), args)
}

/// Compiles `source` (assembly when it starts with a directive, C
/// otherwise) into `name.o`.
fn compile(tools: &Tools, dir: &Path, name: &str, source: &str, flags: &[&str]) {
    let suffix = if source.trim_start().starts_with('.') {
        "s"
    } else {
        "c"
    };
    let file = format!("{name}.{suffix}");
    fs::write(dir.join(&file), source).unwrap();
    let object = format!("{name}.o");
    let mut args = vec!["--target=loongarch64-linux-gnu", "-c", &file, "-o", &object];
    args.extend_from_slice(flags);
    run_ok(dir, &tools.clang, &args);
}

/// Links with lld (adding `lld_extra`) and with qld, into `name.lld` and
/// `name.qld`.
fn link_both(tools: &Tools, dir: &Path, name: &str, args: &[&str], lld_extra: &[&str]) {
    let lld_out = format!("{name}.lld");
    let our_out = format!("{name}.qld");
    let mut lld_args: Vec<&str> = vec!["-o", &lld_out];
    lld_args.extend_from_slice(lld_extra);
    lld_args.extend_from_slice(args);
    run_ok(dir, &tools.lld, &lld_args);
    let mut our_args: Vec<&str> = vec!["-o", &our_out];
    our_args.extend_from_slice(args);
    let output = qld(dir, &our_args);
    assert!(
        output.status.success(),
        "qld {} failed:\n{}",
        our_args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
}

// ---------------------------------------------------------------------------
// A small ELF64 reader: just what the comparison needs.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct Section {
    name: String,
    kind: u32,
    flags: u64,
    addr: u64,
    offset: u64,
    size: u64,
    link: u32,
}

#[derive(Clone, Debug)]
struct Symbol {
    name: String,
    value: u64,
    size: u64,
    kind: u8,
    shndx: u16,
}

#[derive(Clone, Debug)]
struct Rela {
    offset: u64,
    kind: u32,
    symbol: String,
    addend: i64,
}

struct Elf {
    data: Vec<u8>,
    e_type: u16,
    e_flags: u32,
    sections: Vec<Section>,
    symbols: Vec<Symbol>,
    dynsyms: Vec<Symbol>,
    relas: Vec<Rela>,
}

fn u16_at(data: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(data[at..at + 2].try_into().unwrap())
}

fn u32_at(data: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(data[at..at + 4].try_into().unwrap())
}

fn u64_at(data: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(data[at..at + 8].try_into().unwrap())
}

fn c_string(data: &[u8], at: usize) -> String {
    let end = data[at..]
        .iter()
        .position(|&b| b == 0)
        .map_or(data.len(), |p| at + p);
    String::from_utf8_lossy(&data[at..end]).into_owned()
}

const SHT_SYMTAB: u32 = 2;
const SHT_RELA: u32 = 4;
const SHT_NOBITS: u32 = 8;
const SHT_DYNSYM: u32 = 11;
const SHF_ALLOC: u64 = 2;
const SHF_WRITE: u64 = 1;
const SHF_EXECINSTR: u64 = 4;
const SHF_TLS: u64 = 0x400;
const STT_FUNC: u8 = 2;
const STT_SECTION: u8 = 3;
const STT_FILE: u8 = 4;
const STT_TLS: u8 = 6;
/// `andi $zero, $zero, 0`.
const NOP: u32 = 0x0340_0000;

impl Elf {
    fn read(path: &Path) -> Self {
        let data = fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        assert_eq!(&data[..4], b"\x7fELF", "{}", path.display());
        assert_eq!(u16_at(&data, 18), 258, "{}: not LoongArch", path.display());
        let e_type = u16_at(&data, 16);
        let e_flags = u32_at(&data, 48);
        let shoff = u64_at(&data, 40) as usize;
        let shnum = usize::from(u16_at(&data, 60));
        let shstrndx = usize::from(u16_at(&data, 62));
        let mut sections: Vec<Section> = (0..shnum)
            .map(|i| {
                let at = shoff + i * 64;
                Section {
                    name: u32_at(&data, at).to_string(),
                    kind: u32_at(&data, at + 4),
                    flags: u64_at(&data, at + 8),
                    addr: u64_at(&data, at + 16),
                    offset: u64_at(&data, at + 24),
                    size: u64_at(&data, at + 32),
                    link: u32_at(&data, at + 40),
                }
            })
            .collect();
        let names_at = sections[shstrndx].offset as usize;
        for section in &mut sections {
            let offset: usize = section.name.parse().unwrap();
            section.name = c_string(&data, names_at + offset);
        }
        let read_symbols = |kind: u32| -> Vec<Symbol> {
            let Some(table) = sections.iter().find(|s| s.kind == kind) else {
                return Vec::new();
            };
            let strings = sections[table.link as usize].offset as usize;
            (0..table.size as usize / 24)
                .map(|i| {
                    let at = table.offset as usize + i * 24;
                    Symbol {
                        name: c_string(&data, strings + u32_at(&data, at) as usize),
                        kind: data[at + 4] & 0xf,
                        shndx: u16_at(&data, at + 6),
                        value: u64_at(&data, at + 8),
                        size: u64_at(&data, at + 16),
                    }
                })
                .collect()
        };
        let symbols = read_symbols(SHT_SYMTAB);
        let dynsyms = read_symbols(SHT_DYNSYM);
        let mut relas = Vec::new();
        for section in sections
            .iter()
            .filter(|s| s.kind == SHT_RELA && s.flags & SHF_ALLOC != 0)
        {
            for i in 0..section.size as usize / 24 {
                let at = section.offset as usize + i * 24;
                let info = u64_at(&data, at + 8);
                let symbol = (info >> 32) as usize;
                relas.push(Rela {
                    offset: u64_at(&data, at),
                    kind: info as u32,
                    symbol: dynsyms
                        .get(symbol)
                        .map(|s| s.name.clone())
                        .unwrap_or_default(),
                    addend: u64_at(&data, at + 16) as i64,
                });
            }
        }
        Self {
            data,
            e_type,
            e_flags,
            sections,
            symbols,
            dynsyms,
            relas,
        }
    }

    fn section(&self, name: &str) -> Option<&Section> {
        self.sections.iter().find(|s| s.name == name)
    }

    /// The section at `address`. `.tbss` takes no address space outside
    /// the TLS template, so it never is.
    fn section_at(&self, address: u64) -> Option<&Section> {
        self.sections.iter().find(|s| {
            s.flags & SHF_ALLOC != 0
                && !(s.kind == SHT_NOBITS && s.flags & SHF_TLS != 0)
                && s.addr <= address
                && address < s.addr + s.size.max(1)
        })
    }

    /// The bytes at `address`, up to `len`.
    fn bytes(&self, address: u64, len: u64) -> Option<&[u8]> {
        let section = self.section_at(address)?;
        if section.kind == SHT_NOBITS {
            return None;
        }
        let start = (section.offset + address - section.addr) as usize;
        let end = (section.offset + section.size) as usize;
        self.data.get(start..end.min(start + len as usize))
    }

    fn word(&self, address: u64) -> Option<u64> {
        self.bytes(address, 8)
            .filter(|b| b.len() == 8)
            .map(|b| u64::from_le_bytes(b.try_into().unwrap()))
    }

    fn interpreter(&self) -> Option<String> {
        let section = self.section(".interp")?;
        Some(c_string(&self.data, section.offset as usize))
    }

    fn dynamic(&self, tag: u64) -> Vec<String> {
        let Some(section) = self.section(".dynamic") else {
            return Vec::new();
        };
        let strings = self.sections[section.link as usize].offset as usize;
        (0..section.size as usize / 16)
            .map(|i| section.offset as usize + i * 16)
            .filter(|&at| u64_at(&self.data, at) == tag)
            .map(|at| c_string(&self.data, strings + u64_at(&self.data, at + 8) as usize))
            .collect()
    }

    /// The value of the first dynamic entry with `tag`.
    fn dynamic_value(&self, tag: u64) -> Option<u64> {
        let section = self.section(".dynamic")?;
        (0..section.size as usize / 16)
            .map(|i| section.offset as usize + i * 16)
            .find(|&at| u64_at(&self.data, at) == tag)
            .map(|at| u64_at(&self.data, at + 8))
    }

    /// The symbols that name code or data (not sections, files or TLS
    /// offsets), by address.
    fn named(&self) -> impl Iterator<Item = &Symbol> {
        self.symbols.iter().filter(|s| {
            !s.name.is_empty()
                && !matches!(s.kind, STT_SECTION | STT_FILE | STT_TLS)
                && s.shndx != 0
                && s.shndx < 0xff00
                && !s.name.starts_with(".L")
                && !s.name.starts_with('$')
        })
    }

    fn rela_at(&self, address: u64) -> Vec<&Rela> {
        self.relas.iter().filter(|r| r.offset == address).collect()
    }
}

fn reloc_name(kind: u32) -> String {
    match kind {
        0 => "NONE".into(),
        2 => "64".into(),
        3 => "RELATIVE".into(),
        4 => "COPY".into(),
        5 => "JUMP_SLOT".into(),
        7 => "DTPMOD64".into(),
        9 => "DTPREL64".into(),
        11 => "TPREL64".into(),
        12 => "IRELATIVE".into(),
        14 => "DESC64".into(),
        other => format!("type{other}"),
    }
}

/// Describes what is at `address` in `elf`, independently of where the
/// linker put it.
fn describe(elf: &Elf, address: u64) -> String {
    describe_depth(elf, address, 0)
}

fn describe_depth(elf: &Elf, address: u64, depth: u32) -> String {
    // A named symbol at, or containing, the address.
    let mut exact: Vec<&str> = elf
        .named()
        .filter(|s| s.value == address)
        .map(|s| s.name.as_str())
        .collect();
    exact.sort_unstable();
    if let Some(name) = exact.first() {
        return (*name).to_string();
    }
    let Some(section) = elf.section_at(address) else {
        return format!("{address:#x}");
    };
    let offset = address - section.addr;
    match section.name.as_str() {
        // `.got.plt` may be part of `.got` (GNU ld and qld merge them with
        // `-z now`): both are "the GOT", and the words reserved for the
        // dynamic linker are found from `DT_PLTGOT`.
        ".got" | ".got.plt" => {
            if let Some(pltgot) = elf.dynamic_value(3)
                && (pltgot..pltgot + 16).contains(&address)
            {
                return format!("PLTGOT+{}", address - pltgot);
            }
            return format!("GOT[{}]", slot(elf, address, depth));
        }
        ".plt" => {
            if offset < 32 {
                return format!("PLT0+{offset}");
            }
            // Evaluate the entry: pcaddu12i $t3, hi; ld.d $t3, $t3, lo.
            let entry = address - (offset - 32) % 16;
            let hi = elf
                .bytes(entry, 4)
                .map(|b| u32::from_le_bytes(b.try_into().unwrap()));
            let lo = elf
                .bytes(entry + 4, 4)
                .map(|b| u32::from_le_bytes(b.try_into().unwrap()));
            if let (Some(hi), Some(lo)) = (hi, lo) {
                let slot_address = entry
                    .wrapping_add((sext(hi >> 5, 20) << 12) as u64)
                    .wrapping_add(sext(lo >> 10, 12) as u64);
                return format!(
                    "PLT[{}]+{}",
                    slot(elf, slot_address, depth),
                    address - entry
                );
            }
            return format!(".plt+{offset}");
        }
        _ => {}
    }
    if let Some(symbol) = elf
        .named()
        .filter(|s| s.value < address && address < s.value + s.size)
        .min_by_key(|s| (&s.name, s.value))
    {
        return format!("{}+{}", symbol.name, address - symbol.value);
    }
    // Strings and other constants without a symbol: by content.
    if section.flags & (SHF_WRITE | SHF_EXECINSTR) == 0
        && let Some(bytes) = elf.bytes(address, 32)
    {
        let text: Vec<u8> = bytes.iter().copied().take_while(|&b| b != 0).collect();
        if !text.is_empty() && text.iter().all(|b| b.is_ascii_graphic() || *b == b' ') {
            return format!("{}:{:?}", section.name, String::from_utf8_lossy(&text));
        }
    }
    format!("{}+{offset}", section.name)
}

/// What a GOT (or `.got.plt`) word at `address` holds: its dynamic
/// relocations, or its static value.
fn slot(elf: &Elf, address: u64, depth: u32) -> String {
    let relocs = elf.rela_at(address);
    if !relocs.is_empty() {
        return relocs
            .iter()
            .map(|r| {
                if r.kind == 3 && depth < 2 {
                    format!(
                        "RELATIVE &{}",
                        describe_depth(elf, r.addend as u64, depth + 1)
                    )
                } else {
                    format!("{} {}{:+}", reloc_name(r.kind), r.symbol, r.addend)
                }
            })
            .collect::<Vec<_>>()
            .join(",");
    }
    // A TLS pair's second word may carry the relocation.
    if let Some(next) = elf.rela_at(address + 8).first()
        && matches!(next.kind, 9 | 14)
    {
        return format!("pair-of {}", next.symbol);
    }
    match elf.word(address) {
        Some(value) if depth < 2 && elf.section_at(value).is_some() && value != 0 => {
            format!("&{}", describe_depth(elf, value, depth + 1))
        }
        Some(value) => format!("{value:#x}"),
        None => "?".into(),
    }
}

fn sext(value: u32, bits: u32) -> i64 {
    let shift = 64 - bits;
    ((u64::from(value) << shift) as i64) >> shift
}

// ---------------------------------------------------------------------------
// The instruction evaluator.
// ---------------------------------------------------------------------------

/// A register value the evaluator knows, and the instructions (by token
/// index) that built it.
#[derive(Clone, Debug)]
struct Known {
    value: u64,
    from: Vec<usize>,
}

/// One decoded instruction: its token with immediates, and without them
/// (used when the value it builds ends up described as an address, since
/// the immediates then depend on the layout).
struct Token {
    full: String,
    bare: String,
    resolved: bool,
}

/// Decodes the code of `[start, end)` into tokens: the instruction with its
/// immediates, or, where it uses an address it computed (PC-relatively, or
/// from absolute parts), with the description of that address instead. A
/// branch within the function is described by the index of its target
/// instruction. With `skip_nops`, `nop`s are left out (and not counted).
fn tokens(elf: &Elf, start: u64, end: u64, skip_nops: bool) -> Vec<String> {
    let word_at = |pc: u64| {
        elf.bytes(pc, 4)
            .filter(|b| b.len() == 4)
            .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
    };
    // The token index of every instruction address, for local branches.
    let mut index_of: BTreeMap<u64, usize> = BTreeMap::new();
    let mut count = 0usize;
    let mut pc = start;
    while pc + 4 <= end {
        let Some(w) = word_at(pc) else { break };
        index_of.insert(pc, count);
        if !(skip_nops && w == NOP) {
            count += 1;
        }
        pc += 4;
    }
    let target = |address: u64| match index_of.get(&address) {
        Some(index) => format!("@{index}"),
        None => format!("&{}", describe(elf, address)),
    };

    let mut regs: Vec<Option<Known>> = vec![None; 32];
    let mut out: Vec<Token> = Vec::new();
    let mut pc = start;
    while pc + 4 <= end {
        let Some(w) = word_at(pc) else { break };
        if skip_nops && w == NOP {
            pc += 4;
            continue;
        }
        let me = out.len();
        let rd = (w & 0x1f) as usize;
        let rj = ((w >> 5) & 0x1f) as usize;
        let rk = ((w >> 10) & 0x1f) as usize;
        let reg = |r: usize| -> Option<Known> {
            if r == 0 {
                Some(Known {
                    value: 0,
                    from: Vec::new(),
                })
            } else {
                regs[r].clone()
            }
        };
        let si20 = sext(w >> 5, 20);
        let si12 = sext(w >> 10, 12);
        let ui12 = u64::from((w >> 10) & 0xfff);
        // A value built by this instruction from `inputs`.
        let built = |value: u64, inputs: &[&Known]| -> Known {
            let mut from: Vec<usize> = inputs.iter().flat_map(|k| k.from.clone()).collect();
            from.push(me);
            Known { value, from }
        };
        // What the instruction does to `rd`: `Some(value)` writes it (a
        // known value or not), `None` leaves it.
        let mut write: Option<Option<Known>> = None;
        // Instructions whose values the token describes as an address.
        let mut uses: Vec<Known> = Vec::new();
        let (full, bare) = match w & 0xfe00_0000 {
            0x1800_0000 => {
                let value = pc.wrapping_add((si20 << 2) as u64);
                write = Some(Some(built(value, &[])));
                let text = format!("pcaddi r{rd}, {}", target(value));
                (text.clone(), text)
            }
            op @ (0x1a00_0000 | 0x1c00_0000 | 0x1e00_0000) => {
                let (name, value) = match op {
                    0x1a00_0000 => ("pcalau12i", (pc & !0xfff).wrapping_add((si20 << 12) as u64)),
                    0x1c00_0000 => ("pcaddu12i", pc.wrapping_add((si20 << 12) as u64)),
                    _ => ("pcaddu18i", pc.wrapping_add((si20 << 18) as u64)),
                };
                write = Some(Some(built(value, &[])));
                let text = format!("{name} r{rd}");
                (text.clone(), text)
            }
            0x1400_0000 => {
                write = Some(Some(built((si20 << 12) as u64, &[])));
                (
                    format!("lu12i.w r{rd}, {:#x}", (w >> 5) & 0xfffff),
                    format!("lu12i.w r{rd}"),
                )
            }
            0x1600_0000 => {
                let old = reg(rd);
                write = Some(
                    old.as_ref()
                        .map(|k| built((k.value & 0xffff_ffff) | ((si20 << 32) as u64), &[k])),
                );
                (
                    format!("lu32i.d r{rd}, {:#x}", (w >> 5) & 0xfffff),
                    format!("lu32i.d r{rd}"),
                )
            }
            _ => match w & 0xffc0_0000 {
                // addi.w, addi.d
                0x0280_0000 | 0x02c0_0000 => match reg(rj) {
                    Some(base) if rj != 0 => {
                        let value = base.value.wrapping_add(si12 as u64);
                        uses.push(base.clone());
                        write = Some(Some(built(value, &[&base])));
                        let text = format!("addi r{rd}, r{rj}, {}", target(value));
                        (text.clone(), text)
                    }
                    Some(base) => {
                        write = Some(Some(built(si12 as u64, &[&base])));
                        (format!("addi r{rd}, r0, {si12}"), format!("addi r{rd}, r0"))
                    }
                    None => {
                        write = Some(None);
                        let text = format!("addi r{rd}, r{rj}, {si12}");
                        (text.clone(), text)
                    }
                },
                // lu52i.d
                0x0300_0000 => {
                    let base = reg(rj);
                    write = Some(
                        base.as_ref()
                            .map(|k| built((k.value & ((1 << 52) - 1)) | (ui12 << 52), &[k])),
                    );
                    (
                        format!("lu52i.d r{rd}, r{rj}, {ui12:#x}"),
                        format!("lu52i.d r{rd}, r{rj}"),
                    )
                }
                // ori
                0x0380_0000 => {
                    let base = reg(rj);
                    write = Some(base.as_ref().map(|k| built(k.value | ui12, &[k])));
                    (
                        format!("ori r{rd}, r{rj}, {ui12:#x}"),
                        format!("ori r{rd}, r{rj}"),
                    )
                }
                // Loads and stores.
                op @ (0x2800_0000..=0x2bc0_0000) => {
                    let name = format!("mem{:x}", op >> 22);
                    let is_store = matches!(
                        op,
                        0x2900_0000
                            | 0x2940_0000
                            | 0x2980_0000
                            | 0x29c0_0000
                            | 0x2b40_0000
                            | 0x2bc0_0000
                    );
                    if !is_store {
                        write = Some(None);
                    }
                    let text = match reg(rj) {
                        Some(base) if rj != 0 => {
                            let address = base.value.wrapping_add(si12 as u64);
                            uses.push(base);
                            format!("{name} r{rd}, r{rj}, {}", target(address))
                        }
                        _ => format!("{name} r{rd}, r{rj}, {si12}"),
                    };
                    (text.clone(), text)
                }
                _ => match w & 0xfc00_0000 {
                    // jirl
                    0x4c00_0000 => {
                        let offs = sext((w >> 10) & 0xffff, 16) << 2;
                        let text = match reg(rj) {
                            Some(base) if rj != 0 => {
                                let address = base.value.wrapping_add(offs as u64);
                                uses.push(base);
                                format!("jirl r{rd}, {}", target(address))
                            }
                            _ => format!("jirl r{rd}, r{rj}, {offs}"),
                        };
                        write = Some(None);
                        (text.clone(), text)
                    }
                    // b, bl
                    op @ (0x5000_0000 | 0x5400_0000) => {
                        let offs = sext(((w >> 10) & 0xffff) | ((w & 0x3ff) << 16), 26) << 2;
                        let name = if op == 0x5000_0000 { "b" } else { "bl" };
                        let text = format!("{name} {}", target(pc.wrapping_add(offs as u64)));
                        (text.clone(), text)
                    }
                    // beq … bgeu
                    op @ 0x5800_0000..=0x6c00_0000 => {
                        let offs = sext((w >> 10) & 0xffff, 16) << 2;
                        let text = format!(
                            "b{:x} r{rd}, r{rj}, {}",
                            op >> 26,
                            target(pc.wrapping_add(offs as u64))
                        );
                        (text.clone(), text)
                    }
                    // beqz, bnez, bceqz/bcnez
                    op @ 0x4000_0000..=0x4800_0000 => {
                        let offs = sext(((w >> 10) & 0xffff) | ((w & 0x1f) << 16), 21) << 2;
                        let text = format!(
                            "b{:x} r{rj}, {}",
                            op >> 26,
                            target(pc.wrapping_add(offs as u64))
                        );
                        (text.clone(), text)
                    }
                    _ => match w & 0xffff_8000 {
                        // add.d, and ldx.d: the extreme code model adds
                        // two known halves.
                        op @ (0x0010_8000 | 0x380c_0000) => {
                            let known = match (reg(rj), reg(rk)) {
                                (Some(a), Some(b)) if rj != 0 && rk != 0 => Some((a, b)),
                                _ => None,
                            };
                            let name = if op == 0x0010_8000 { "add.d" } else { "ldx.d" };
                            let text = match known {
                                Some((a, b)) => {
                                    let value = a.value.wrapping_add(b.value);
                                    if op == 0x0010_8000 {
                                        write = Some(Some(built(value, &[&a, &b])));
                                    } else {
                                        write = Some(None);
                                    }
                                    uses.push(a);
                                    uses.push(b);
                                    format!("{name} r{rd}, {}", target(value))
                                }
                                None => {
                                    write = Some(None);
                                    format!("{name} r{rd}, r{rj}, r{rk}")
                                }
                            };
                            (text.clone(), text)
                        }
                        _ => {
                            // Anything else is compared as is and assumed
                            // to write `rd`.
                            write = Some(None);
                            let text = format!("{w:08x}");
                            (text.clone(), text)
                        }
                    },
                },
            },
        };
        for known in &uses {
            for &index in &known.from {
                if let Some(token) = out.get_mut(index) {
                    token.resolved = true;
                }
            }
        }
        out.push(Token {
            full,
            bare,
            resolved: false,
        });
        if let Some(value) = write
            && rd != 0
        {
            regs[rd] = value;
        }
        // A call clobbers the caller-saved registers; assume all.
        if matches!(w & 0xfc00_0000, 0x4c00_0000 | 0x5400_0000) {
            regs = vec![None; 32];
        }
        pc += 4;
    }
    out.into_iter()
        .map(|t| if t.resolved { t.bare } else { t.full })
        .collect()
}

/// The functions of `elf` and their extents.
fn functions(elf: &Elf) -> BTreeMap<String, (u64, u64)> {
    elf.symbols
        .iter()
        .filter(|s| s.kind == STT_FUNC && s.size > 0 && !s.name.is_empty())
        .map(|s| (s.name.clone(), (s.value, s.value + s.size)))
        .collect()
}

/// Asserts that qld and lld relocated every function they both have the
/// same way, and wrote the same PLT. With `skip_nops`, `nop`s are ignored
/// (for comparing with lld's relaxed, shrunk code).
fn assert_same_code(dir: &Path, name: &str, skip_nops: bool) {
    let theirs = Elf::read(&dir.join(format!("{name}.lld")));
    let ours = Elf::read(&dir.join(format!("{name}.qld")));
    let their_functions = functions(&theirs);
    let our_functions = functions(&ours);
    let mut compared = 0usize;
    let mut report = String::new();
    for (function, &(start, end)) in &their_functions {
        let Some(&(our_start, our_end)) = our_functions.get(function) else {
            continue;
        };
        let expected = tokens(&theirs, start, end, skip_nops);
        let actual = tokens(&ours, our_start, our_end, skip_nops);
        if expected != actual {
            let _ = writeln!(
                report,
                "{function} was relocated differently\nlld: {expected:#?}\nqld: {actual:#?}"
            );
        }
        compared += 1;
    }
    // The PLT, entry by entry.
    if let (Some(plt), Some(our_plt)) = (theirs.section(".plt"), ours.section(".plt")) {
        let expected = tokens(&theirs, plt.addr, plt.addr + plt.size, false);
        let actual = tokens(&ours, our_plt.addr, our_plt.addr + our_plt.size, false);
        if expected != actual {
            let _ = writeln!(
                report,
                "the PLT differs\nlld: {expected:#?}\nqld: {actual:#?}"
            );
        }
    }
    assert!(report.is_empty(), "{name}:\n{report}");
    assert!(compared > 0, "{name}: no function was compared");
}

/// The dynamic relocations of `elf`, described by what they apply to and
/// what they compute.
fn dynamic_relocations(elf: &Elf) -> Vec<String> {
    let mut out: Vec<String> = elf
        .relas
        .iter()
        .map(|r| {
            let place = match elf.section_at(r.offset).map(|s| s.name.as_str()) {
                Some(".got" | ".got.plt") => "got".to_string(),
                _ => describe(elf, r.offset),
            };
            let value = if r.kind == 3 {
                format!("&{}", describe(elf, r.addend as u64))
            } else {
                format!("{}{:+}", r.symbol, r.addend)
            };
            format!("{place}: {} {value}", reloc_name(r.kind))
        })
        .collect();
    out.sort();
    out
}

/// Asserts that the two outputs agree on everything the dynamic linker
/// sees: type, flags, interpreter, needed libraries, exported symbols and
/// dynamic relocations. lld keeps the GOT entries of relaxed GOT loads,
/// qld drops them, so lld may have more `RELATIVE` relocations in `.got`.
fn assert_same_dynamic(dir: &Path, name: &str) {
    let theirs = Elf::read(&dir.join(format!("{name}.lld")));
    let ours = Elf::read(&dir.join(format!("{name}.qld")));
    assert_eq!(theirs.e_type, ours.e_type, "{name}: e_type");
    assert_eq!(theirs.e_flags, ours.e_flags, "{name}: e_flags");
    assert_eq!(
        theirs.interpreter(),
        ours.interpreter(),
        "{name}: interpreter"
    );
    for tag in [1u64, 14] {
        assert_eq!(
            theirs.dynamic(tag),
            ours.dynamic(tag),
            "{name}: dynamic tag {tag}"
        );
    }
    let dynsyms = |elf: &Elf| {
        let mut names: Vec<(String, bool)> = elf
            .dynsyms
            .iter()
            .filter(|s| !s.name.is_empty())
            .map(|s| (s.name.clone(), s.shndx != 0))
            .collect();
        names.sort();
        names
    };
    assert_eq!(dynsyms(&theirs), dynsyms(&ours), "{name}: dynamic symbols");
    let their_relocs = dynamic_relocations(&theirs);
    let mut our_relocs = dynamic_relocations(&ours);
    let mut extra: Vec<String> = Vec::new();
    for reloc in their_relocs {
        if let Some(at) = our_relocs.iter().position(|r| *r == reloc) {
            our_relocs.remove(at);
        } else if !reloc.starts_with("got: RELATIVE") {
            extra.push(reloc);
        }
    }
    assert!(
        extra.is_empty() && our_relocs.is_empty(),
        "{name}: dynamic relocations differ\nonly lld: {extra:#?}\nonly qld: {our_relocs:#?}"
    );
}

// ---------------------------------------------------------------------------
// The tests.
// ---------------------------------------------------------------------------

/// A static program: calls, globals, a string, TLS (local-exec) and a GOT
/// access the linker relaxes.
const STATIC_MAIN: &str = r#"
int counter = 5;
static int table[4] = {1, 2, 3, 4};
extern int other(int);
extern int weak_missing(void) __attribute__((weak));
__thread int tls_a = 7;
__thread int tls_b;
static __thread long tls_c = 3;
const char *msg = "hello";

__attribute__((noinline)) int helper(int x) { return x + counter + table[x & 3]; }
__attribute__((noinline)) int get_tls(void) { return tls_a + tls_b + (int)tls_c; }
__attribute__((noinline)) long *tls_addr(void) { return &tls_c; }

void _start(void) {
    int r = helper(3) + other(2) + get_tls() + (int)*tls_addr();
#ifndef NO_WEAK
    if (weak_missing)
        r += weak_missing();
#endif
    counter = r + msg[0];
    for (;;) {}
}
"#;

const STATIC_OTHER: &str = r#"
extern int counter;
extern const char *msg;
int other(int x) { return x * counter + msg[1]; }
"#;

#[test]
fn static_executable_matches_lld() {
    let tools = require!();
    for (variant, flags) in [
        ("nopic", &["-O2", "-fno-pic", "-mno-relax"][..]),
        ("pie", &["-O2", "-fPIE", "-mno-relax"][..]),
        (
            "medium",
            &["-O2", "-fPIE", "-mno-relax", "-mcmodel=medium"][..],
        ),
        ("O0", &["-O0", "-fPIE", "-mno-relax"][..]),
    ] {
        let dir = scratch(&format!("static-{variant}"));
        compile(&tools, &dir, "main", STATIC_MAIN, flags);
        compile(&tools, &dir, "other", STATIC_OTHER, flags);
        let args = ["-static", "main.o", "other.o"];
        link_both(&tools, &dir, "out", &args, &["--no-relax"]);
        assert_same_code(&dir, "out", false);
        let ours = Elf::read(&dir.join("out.qld"));
        assert!(ours.relas.is_empty(), "{variant}: dynamic relocations left");
        assert_eq!(ours.e_flags, 0x43, "{variant}: e_flags");
    }
}

#[test]
fn static_pie_matches_lld() {
    let tools = require!();
    let dir = scratch("static-pie-output");
    let flags = ["-O2", "-fPIE", "-mno-relax"];
    compile(&tools, &dir, "main", STATIC_MAIN, &flags);
    compile(&tools, &dir, "other", STATIC_OTHER, &flags);
    let args = [
        "-static",
        "-pie",
        "--no-dynamic-linker",
        "main.o",
        "other.o",
    ];
    link_both(&tools, &dir, "out", &args, &["--no-relax"]);
    assert_same_code(&dir, "out", false);
    assert_same_dynamic(&dir, "out");
}

/// A shared library with every kind of GOT, PLT and TLS access clang
/// emits for PIC code.
const LIBRARY: &str = r#"
__thread int lib_tls = 11;
__thread int lib_tls2;
static __thread int lib_local_tls = 5;
int lib_data = 42;
int lib_counter;
extern int exe_data;
extern int imported(int);

int lib_func(int x) { return x + lib_data + lib_counter++ + imported(x); }
static int local_helper(int x) { return x * 3; }
int (*lib_fp)(int) = lib_func;
int *lib_data_ptr = &lib_data;
static int *local_ptr = &lib_counter;

int lib_tls_sum(void) { return lib_tls + lib_tls2 + lib_local_tls + *local_ptr; }
int *lib_tls_addr(void) { return &lib_local_tls; }
int lib_calls(int x) { return lib_func(x) + local_helper(x) + lib_fp(x) + exe_data; }
"#;

#[test]
fn shared_library_matches_lld() {
    let tools = require!();
    for (variant, flags) in [
        ("trad", &["-O2", "-fPIC", "-mno-relax"][..]),
        (
            "desc",
            &["-O2", "-fPIC", "-mno-relax", "-mtls-dialect=desc"][..],
        ),
        (
            "ie",
            &["-O2", "-fPIC", "-mno-relax", "-ftls-model=initial-exec"][..],
        ),
        (
            "medium",
            &["-O2", "-fPIC", "-mno-relax", "-mcmodel=medium"][..],
        ),
    ] {
        let dir = scratch(&format!("shared-{variant}"));
        compile(&tools, &dir, "lib", LIBRARY, flags);
        let args = [
            "-shared",
            "-soname",
            "libt.so",
            "--allow-shlib-undefined",
            "lib.o",
        ];
        link_both(&tools, &dir, "libt", &args, &["--no-relax"]);
        assert_same_code(&dir, "libt", false);
        assert_same_dynamic(&dir, "libt");
    }
}

const EXECUTABLE: &str = r#"
extern __thread int lib_tls;
extern int lib_data;
extern int lib_func(int);
extern int lib_calls(int);
extern int (*lib_fp)(int);
__thread int exe_tls = 3;
int exe_data = 9;
int (*volatile fp)(int) = lib_func;
int imported(int x) { return x - 1; }

int main(void) {
    return lib_func(1) + lib_calls(2) + lib_data + lib_tls + exe_tls + fp(3) + lib_fp(4)
        + exe_data;
}
"#;

#[test]
fn executables_against_a_library_match_lld() {
    let tools = require!();
    for (variant, flags, link) in [
        ("pie", &["-O2", "-fPIE", "-mno-relax"][..], &["-pie"][..]),
        (
            "nopie",
            &["-O2", "-fno-pic", "-mno-relax"][..],
            &["-no-pie"][..],
        ),
        (
            "desc",
            &["-O2", "-fPIC", "-mno-relax", "-mtls-dialect=desc"][..],
            &["-pie"][..],
        ),
        (
            "now",
            &["-O2", "-fPIE", "-mno-relax"][..],
            &["-pie", "-z", "now"][..],
        ),
    ] {
        let dir = scratch(&format!("exe-{variant}"));
        compile(
            &tools,
            &dir,
            "lib",
            LIBRARY,
            &["-O2", "-fPIC", "-mno-relax"],
        );
        run_ok(
            &dir,
            &tools.lld,
            &[
                "-shared",
                "-soname",
                "libt.so",
                "--allow-shlib-undefined",
                "lib.o",
                "-o",
                "libt.so",
            ],
        );
        compile(&tools, &dir, "main", EXECUTABLE, flags);
        let mut args: Vec<&str> = link.to_vec();
        args.extend_from_slice(&[
            "--dynamic-linker",
            "/lib64/ld-linux-loongarch-lp64d.so.1",
            "-e",
            "main",
            "--export-dynamic-symbol=imported",
            // The library calls `__tls_get_addr`, which libc would define.
            "--allow-shlib-undefined",
            "main.o",
            "libt.so",
        ]);
        link_both(&tools, &dir, "main", &args, &["--no-relax"]);
        assert_same_code(&dir, "main", false);
        assert_same_dynamic(&dir, "main");
    }
}

/// Every TLS model in an executable, which relaxes descriptors and
/// initial-exec accesses to local-exec, or initial-exec for a library's
/// variable.
const TLS_EXECUTABLE: &str = r#"
__thread int tls_here = 1;
static __thread int tls_static = 2;
extern __thread int tls_there;
int read_here(void) { return tls_here; }
int read_static(void) { return tls_static; }
int read_there(void) { return tls_there; }
int *addr_here(void) { return &tls_here; }
int main(void) { return read_here() + read_static() + read_there() + *addr_here(); }
"#;

#[test]
fn tls_relaxation_in_executables_matches_lld() {
    let tools = require!();
    for (variant, flags) in [
        ("gd", &["-O2", "-fPIC", "-mno-relax"][..]),
        (
            "desc",
            &["-O2", "-fPIC", "-mno-relax", "-mtls-dialect=desc"][..],
        ),
        (
            "ie",
            &["-O2", "-fPIC", "-mno-relax", "-ftls-model=initial-exec"][..],
        ),
        (
            "le",
            &["-O2", "-fPIE", "-mno-relax", "-ftls-model=local-exec"][..],
        ),
    ] {
        let dir = scratch(&format!("tls-exe-{variant}"));
        compile(
            &tools,
            &dir,
            "lib",
            "__thread int tls_there = 3; void *__tls_get_addr(void *p) { return p; }",
            &["-O2", "-fPIC", "-mno-relax"],
        );
        run_ok(
            &dir,
            &tools.lld,
            &[
                "-shared",
                "-soname",
                "libtls.so",
                "lib.o",
                "-o",
                "libtls.so",
            ],
        );
        let source = if variant == "le" {
            // Local-exec cannot reach a library's variable.
            TLS_EXECUTABLE.replace("extern __thread int tls_there;", "__thread int tls_there;")
        } else {
            TLS_EXECUTABLE.to_string()
        };
        compile(&tools, &dir, "main", &source, flags);
        let args = [
            "-pie",
            "--dynamic-linker",
            "/lib64/ld-linux-loongarch-lp64d.so.1",
            "-e",
            "main",
            "main.o",
            "libtls.so",
        ];
        link_both(&tools, &dir, "main", &args, &["--no-relax"]);
        assert_same_code(&dir, "main", false);
        assert_same_dynamic(&dir, "main");
    }
}

/// Assembly for the relocations clang only emits in other code models, or
/// never from C: the extreme code model's 64-bit sequences, absolute
/// addresses, `pcaddi`, the branch forms and data.
const RELOCATIONS: &str = r#"
	.text
	.globl	_start
	.type	_start, @function
_start:
	pcalau12i	$t0, %pc_hi20(data_qld)
	addi.d		$t1, $zero, %pc_lo12(data_qld)
	lu32i.d		$t1, %pc64_lo20(data_qld)
	lu52i.d		$t1, $t1, %pc64_hi12(data_qld)
	add.d		$a0, $t0, $t1
	pcalau12i	$t0, %got_pc_hi20(data_qld)
	addi.d		$t1, $zero, %got_pc_lo12(data_qld)
	lu32i.d		$t1, %got64_pc_lo20(data_qld)
	lu52i.d		$t1, $t1, %got64_pc_hi12(data_qld)
	ldx.d		$a0, $t0, $t1
	lu12i.w		$a1, %abs_hi20(data_qld)
	ori		$a1, $a1, %abs_lo12(data_qld)
	lu32i.d		$a1, %abs64_lo20(data_qld)
	lu52i.d		$a1, $a1, %abs64_hi12(data_qld)
	ld.d		$a2, $a1, 0
	pcaddi		$a3, %pcrel_20(data_qld)
	ld.d		$a3, $a3, 0
	pcalau12i	$a4, %pc_hi20(callee_qld)
	jirl		$ra, $a4, %pc_lo12(callee_qld)
	beq		$a0, $a1, local_qld
	bnez		$a0, local_qld
	b		local_qld
local_qld:
	bl		callee_qld
	pcaddu18i	$ra, %call36(callee_qld)
	jirl		$ra, $ra, 0
	ret
	.size	_start, . - _start

	.globl	callee_qld
	.type	callee_qld, @function
	.p2align 4
callee_qld:
	ret
	.size	callee_qld, . - callee_qld

	.data
	.p2align 3
	.globl	data_qld
data_qld:
	.dword	0x1122334455667788
	.dword	callee_qld
	.word	callee_qld - .
	.dword	data_qld - .
"#;

#[test]
fn every_relocation_form_matches_lld() {
    let tools = require!();
    let dir = scratch("relocations");
    compile(&tools, &dir, "relocs", RELOCATIONS, &["-mno-relax"]);
    link_both(
        &tools,
        &dir,
        "out",
        &["-static", "relocs.o"],
        &["--no-relax"],
    );
    assert_same_code(&dir, "out", false);
    // The data words: an address and two PC-relative differences.
    for name in ["lld", "qld"] {
        let elf = Elf::read(&dir.join(format!("out.{name}")));
        let data = elf.symbols.iter().find(|s| s.name == "data_qld").unwrap();
        let callee = elf.symbols.iter().find(|s| s.name == "callee_qld").unwrap();
        assert_eq!(elf.word(data.value + 8), Some(callee.value), "{name}");
        let rel32 = elf.bytes(data.value + 16, 4).unwrap();
        let rel32 = i32::from_le_bytes(rel32.try_into().unwrap()) as i64;
        assert_eq!(
            rel32,
            callee.value as i64 - (data.value + 16) as i64,
            "{name}"
        );
        assert_eq!(elf.word(data.value + 20), Some((-20i64) as u64), "{name}");
    }
}

/// With linker relaxation (`-mrelax`, clang's default): qld rewrites what
/// lld relaxes, leaving `nop`s where lld deletes bytes.
#[test]
fn relaxation_matches_lld_without_the_deleted_bytes() {
    let tools = require!();
    for (variant, flags, link) in [
        ("static", &["-O2", "-fPIE", "-mrelax"][..], &["-static"][..]),
        (
            "medium",
            &["-O2", "-fPIE", "-mrelax", "-mcmodel=medium"][..],
            &["-static"][..],
        ),
        (
            "desc",
            &["-O2", "-fPIC", "-mrelax", "-mtls-dialect=desc"][..],
            &["-static"][..],
        ),
        (
            "le",
            &["-O2", "-fPIE", "-mrelax", "-ftls-model=local-exec"][..],
            &["-static"][..],
        ),
    ] {
        let dir = scratch(&format!("relax-{variant}"));
        // A call to an undefined weak function goes to address 0, which a
        // `bl` reaches from lld's image base (0x20000) but not from GNU
        // ld's and qld's (0x120000000): leave it out.
        let mut flags = flags.to_vec();
        flags.push("-DNO_WEAK");
        compile(&tools, &dir, "main", STATIC_MAIN, &flags);
        compile(&tools, &dir, "other", STATIC_OTHER, &flags);
        let mut args = link.to_vec();
        args.extend_from_slice(&["main.o", "other.o"]);
        link_both(&tools, &dir, "out", &args, &[]);
        assert_same_code(&dir, "out", true);
    }
}

/// `.eh_frame` and DWARF, whose label differences clang emits as
/// `R_LARCH_ADD*`/`R_LARCH_SUB*` pairs when the code may relax.
#[test]
fn label_differences_are_computed() {
    let tools = require!();
    let dir = scratch("label-differences");
    let flags = [
        "-O2",
        "-g",
        "-fPIE",
        "-mrelax",
        "-funwind-tables",
        "-fasynchronous-unwind-tables",
    ];
    compile(&tools, &dir, "main", STATIC_MAIN, &flags);
    compile(&tools, &dir, "other", STATIC_OTHER, &flags);
    // Without relaxation neither linker moves code, so every difference is
    // the one in the object, and both outputs have the same bytes.
    link_both(
        &tools,
        &dir,
        "out",
        &["-static", "main.o", "other.o"],
        &["--no-relax"],
    );
    let theirs = Elf::read(&dir.join("out.lld"));
    let ours = Elf::read(&dir.join("out.qld"));
    // `.eh_frame`: the call frame programs (after each record's length,
    // CIE pointer and PC range) must match.
    let frames = |elf: &Elf| -> Vec<Vec<u8>> {
        let section = elf.section(".eh_frame").expect(".eh_frame");
        let data = &elf.data[section.offset as usize..(section.offset + section.size) as usize];
        let mut out = Vec::new();
        let mut at = 0usize;
        while at + 8 <= data.len() {
            let length = u32_at(data, at) as usize;
            if length == 0 {
                break;
            }
            let id = u32_at(data, at + 4);
            let body = &data[at + 8..at + 4 + length];
            // An FDE's program starts after its PC begin, range and
            // augmentation length (pcrel sdata4 in both linkers' output).
            out.push(if id == 0 {
                body.to_vec()
            } else {
                body[9..].to_vec()
            });
            at += 4 + length;
        }
        out
    };
    assert_eq!(frames(&theirs), frames(&ours), ".eh_frame programs differ");
    // `.debug_line` and `.debug_info` only hold offsets within the objects
    // (and addresses, which differ): the sizes must match, and so must
    // `.debug_line`, whose addresses are all in `DW_LNE_set_address`
    // operands that both linkers write.
    for name in [".debug_line", ".debug_info", ".debug_frame"] {
        let (Some(a), Some(b)) = (theirs.section(name), ours.section(name)) else {
            continue;
        };
        assert_eq!(a.size, b.size, "{name} size");
    }
}
