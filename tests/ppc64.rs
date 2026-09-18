//! PowerPC64 little-endian (ELFv2) tests (workstream W31).
//!
//! These run on any host with a compiler that targets `powerpc64le-linux-gnu`
//! (`powerpc64le-linux-gnu-gcc`, or a clang built with the PowerPC target)
//! and a reference linker (`ld.lld`, or `powerpc64le-linux-gnu-ld`),
//! without being able to *run* PowerPC binaries: every test links the same
//! inputs with qld and with each reference linker and compares what they
//! produced, function by function.
//!
//! The linkers lay out their outputs differently (qld follows GNU ld's
//! section order, lld sorts by permissions), put their call stubs in
//! different places, and decide the layout-dependent TOC optimizations
//! differently, so the instruction streams are compared *semantically*
//! ([`canon`]): every TOC-relative, GOT-indirect or PC-relative address is
//! resolved to the symbol it names, a load of an address from a GOT or
//! `.toc` entry counts the same as computing that address, branches are
//! followed through PLT stubs and range-extension thunks to their final
//! destination, and `nop`s are dropped. What is left must match exactly:
//! relocated immediates, TLS relaxations, local entry points, the TOC
//! restores after calls through stubs. The dynamic metadata is compared
//! too: every dynamic relocation by type, target and kind of place, and the
//! `.glink` lazy-binding code and `DT_PPC64_GLINK`.
//!
//! Programs that need a C library are fixtures (`tests/fixtures/ppc64le-*`),
//! which CI runs under `qemu-ppc64le`.
//!
//! Tools come from `QLD_PPC64_CC` (a compiler driver), `QLD_TEST_LLD` and
//! `QLD_PPC64_LD` (reference linkers), or `PATH`. A test prints `SKIPPED:`
//! and passes when a tool is missing, unless `QLD_REQUIRE_TOOLS` is set.

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

/// The toolchain a test needs.
struct Tools {
    /// The compiler driver.
    cc: PathBuf,
    /// Arguments that make it target PowerPC64 LE (`--target=` for clang).
    cc_args: Vec<String>,
    /// Reference linkers: `(name, path)`.
    references: Vec<(&'static str, PathBuf)>,
}

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("ppc64-tests")
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

/// Whether `cc` with `args` compiles for PowerPC64 LE.
fn compiles(cc: &Path, args: &[String]) -> bool {
    let dir = scratch("probe");
    fs::write(dir.join("probe.c"), "int probe(void) { return 1; }\n").unwrap();
    let mut all: Vec<&str> = args.iter().map(String::as_str).collect();
    all.extend(["-c", "probe.c", "-o", "probe.o"]);
    run(&dir, cc, &all).status.success()
}

fn discover() -> Result<Tools, String> {
    let mut candidates: Vec<(PathBuf, Vec<String>)> = Vec::new();
    if let Some(cc) = tool("QLD_PPC64_CC", &["powerpc64le-linux-gnu-gcc"]) {
        let args = if cc.to_string_lossy().contains("clang") {
            vec!["--target=powerpc64le-linux-gnu".to_string()]
        } else {
            Vec::new()
        };
        candidates.push((cc, args));
    }
    if std::env::var_os("QLD_PPC64_CC").is_none()
        && let Some(clang) = find("clang")
    {
        candidates.push((clang, vec!["--target=powerpc64le-linux-gnu".to_string()]));
    }
    let (cc, cc_args) = candidates
        .into_iter()
        .find(|(cc, args)| compiles(cc, args))
        .ok_or("no compiler for powerpc64le-linux-gnu")?;
    let mut references = Vec::new();
    if let Some(lld) = tool("QLD_TEST_LLD", &["ld.lld"]) {
        references.push(("lld", lld));
    }
    if let Some(ld) = tool(
        "QLD_PPC64_LD",
        &["powerpc64le-linux-gnu-ld.bfd", "powerpc64le-linux-gnu-ld"],
    ) {
        references.push(("GNU ld", ld));
    }
    if references.is_empty() {
        return Err(
            "no reference linker for powerpc64le (ld.lld or powerpc64le-linux-gnu-ld)".into(),
        );
    }
    Ok(Tools {
        cc,
        cc_args,
        references,
    })
}

/// The tools, discovered once per test binary.
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

/// Compiles `source` (C, or assembly when `name` ends in `.s`) into
/// `<stem>.o`.
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

/// Links `args` into `out` with qld and into `out.<n>` with each reference
/// linker, and compares each reference output with qld's.
fn link_and_compare(tools: &Tools, dir: &Path, out: &str, args: &[&str]) -> elf::Elf {
    let mut ours: Vec<&str> = vec!["-o", out];
    ours.extend_from_slice(args);
    qld_ok(dir, &ours);
    let our_elf = elf::Elf::read(&dir.join(out));
    assert_eq!(our_elf.e_flags, 2, "qld must mark the output ELFv2");
    for (index, (name, linker)) in tools.references.iter().enumerate() {
        let reference = format!("{out}.ref{index}");
        let mut theirs: Vec<&str> = vec!["-o", &reference];
        theirs.extend_from_slice(args);
        run_ok(dir, linker, &theirs);
        let their_elf = elf::Elf::read(&dir.join(&reference));
        canon::assert_equivalent(name, &their_elf, &our_elf);
    }
    our_elf
}

/// A minimal ELF64 little-endian reader for the parts the comparison needs.
mod elf {
    use std::path::Path;

    #[derive(Clone, Debug)]
    pub struct Section {
        pub name: String,
        pub sh_type: u32,
        pub flags: u64,
        pub addr: u64,
        pub offset: u64,
        pub size: u64,
        pub link: u32,
    }

    #[derive(Clone, Debug)]
    pub struct Symbol {
        pub name: String,
        pub value: u64,
        pub size: u64,
        pub kind: u8,
        pub other: u8,
        pub shndx: u16,
    }

    #[derive(Clone, Debug)]
    pub struct Reloc {
        pub offset: u64,
        pub r_type: u32,
        pub symbol: Option<String>,
        pub addend: i64,
    }

    pub struct Elf {
        pub data: Vec<u8>,
        pub e_flags: u32,
        pub sections: Vec<Section>,
        pub symbols: Vec<Symbol>,
        pub relocs: Vec<Reloc>,
        pub dynamic: Vec<(i64, u64)>,
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
        let end = data[at..].iter().position(|&b| b == 0).unwrap_or(0) + at;
        String::from_utf8_lossy(&data[at..end]).into_owned()
    }

    fn read_symbols(data: &[u8], sections: &[Section], table: &Section) -> Vec<Symbol> {
        let strings = &sections[table.link as usize];
        (0..table.size / 24)
            .map(|i| {
                let at = (table.offset + i * 24) as usize;
                Symbol {
                    name: c_string(data, strings.offset as usize + u32_at(data, at) as usize),
                    kind: data[at + 4] & 0xf,
                    other: data[at + 5],
                    shndx: u16_at(data, at + 6),
                    value: u64_at(data, at + 8),
                    size: u64_at(data, at + 16),
                }
            })
            .collect()
    }

    impl Elf {
        pub fn read(path: &Path) -> Self {
            let data = std::fs::read(path).unwrap();
            assert_eq!(&data[..4], b"\x7fELF", "{} is not ELF", path.display());
            assert_eq!(u16_at(&data, 18), 21, "{} is not PowerPC64", path.display());
            let e_flags = u32_at(&data, 48);
            let shoff = u64_at(&data, 40) as usize;
            let shnum = u16_at(&data, 60) as usize;
            let shstrndx = u16_at(&data, 62) as usize;
            let mut sections: Vec<Section> = (0..shnum)
                .map(|i| {
                    let at = shoff + i * 64;
                    Section {
                        name: u32_at(&data, at).to_string(),
                        sh_type: u32_at(&data, at + 4),
                        flags: u64_at(&data, at + 8),
                        addr: u64_at(&data, at + 16),
                        offset: u64_at(&data, at + 24),
                        size: u64_at(&data, at + 32),
                        link: u32_at(&data, at + 40),
                    }
                })
                .collect();
            let names = sections[shstrndx].offset as usize;
            for section in &mut sections {
                let at = section.name.parse::<usize>().unwrap();
                section.name = c_string(&data, names + at);
            }
            let symbols = sections
                .iter()
                .find(|s| s.sh_type == 2)
                .map(|table| read_symbols(&data, &sections, table))
                .unwrap_or_default();
            let dynsyms = sections
                .iter()
                .find(|s| s.sh_type == 11)
                .map(|table| read_symbols(&data, &sections, table))
                .unwrap_or_default();
            let mut relocs = Vec::new();
            for section in sections
                .iter()
                .filter(|s| s.sh_type == 4 && s.flags & 2 != 0)
            {
                for i in 0..section.size / 24 {
                    let at = (section.offset + i * 24) as usize;
                    let info = u64_at(&data, at + 8);
                    let index = (info >> 32) as usize;
                    relocs.push(Reloc {
                        offset: u64_at(&data, at),
                        r_type: info as u32,
                        symbol: (index != 0).then(|| dynsyms[index].name.clone()),
                        addend: u64_at(&data, at + 16) as i64,
                    });
                }
            }
            let dynamic = sections
                .iter()
                .find(|s| s.sh_type == 6)
                .map(|section| {
                    (0..section.size / 16)
                        .map(|i| {
                            let at = (section.offset + i * 16) as usize;
                            (u64_at(&data, at) as i64, u64_at(&data, at + 8))
                        })
                        .take_while(|&(tag, _)| tag != 0)
                        .collect()
                })
                .unwrap_or_default();
            Self {
                data,
                e_flags,
                sections,
                symbols,
                relocs,
                dynamic,
            }
        }

        pub fn section(&self, name: &str) -> Option<&Section> {
            self.sections.iter().find(|s| s.name == name)
        }

        /// The allocated section holding `address`.
        pub fn section_at(&self, address: u64) -> Option<&Section> {
            self.sections
                .iter()
                .find(|s| s.flags & 2 != 0 && s.addr <= address && address < s.addr + s.size.max(1))
        }

        /// The file bytes at `address`, if it is in a section with contents.
        pub fn bytes_at(&self, address: u64, len: usize) -> Option<&[u8]> {
            let section = self.section_at(address)?;
            if section.sh_type == 8 {
                return None;
            }
            let at = (section.offset + (address - section.addr)) as usize;
            self.data.get(at..at + len)
        }

        pub fn word(&self, address: u64) -> Option<u32> {
            self.bytes_at(address, 4)
                .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
        }

        pub fn dword(&self, address: u64) -> Option<u64> {
            self.bytes_at(address, 8)
                .map(|b| u64::from_le_bytes(b.try_into().unwrap()))
        }

        pub fn dynamic_value(&self, tag: i64) -> Option<u64> {
            self.dynamic
                .iter()
                .find(|&&(t, _)| t == tag)
                .map(|&(_, v)| v)
        }

        /// The TOC pointer: `.TOC.`, or `.got + 0x8000` without the symbol.
        pub fn toc(&self) -> u64 {
            self.symbols
                .iter()
                .find(|s| s.name == ".TOC." && s.shndx != 0)
                .map(|s| s.value)
                .or_else(|| self.section(".got").map(|s| s.addr + 0x8000))
                .unwrap_or(0)
        }
    }
}

/// The semantic comparison of two PowerPC64 outputs.
mod canon {
    use super::elf::{Elf, Reloc};
    use super::*;

    const R_PPC64_ADDR64: u32 = 38;
    const R_PPC64_GLOB_DAT: u32 = 20;
    const R_PPC64_JMP_SLOT: u32 = 21;
    const R_PPC64_RELATIVE: u32 = 22;
    const R_PPC64_IRELATIVE: u32 = 248;
    const DT_PPC64_GLINK: i64 = 0x7000_0000;

    fn sext16(v: u32) -> i64 {
        i64::from(v as u16 as i16)
    }

    fn sext34(v: u64) -> i64 {
        ((v << 30) as i64) >> 30
    }

    /// What is known about each register: the address it holds.
    #[derive(Clone, Copy, PartialEq, Eq)]
    struct Known {
        /// The address each register holds, if known.
        regs: [Option<u64>; 32],
        /// Known values spilled to the stack frame: `(offset from r1,
        /// value)`, as compilers spill a hoisted `addis`.
        stack: [Option<(i64, u64)>; 16],
    }

    impl Known {
        const NONE: Self = Self {
            regs: [None; 32],
            stack: [None; 16],
        };

        fn spilled(&self, offset: i64) -> Option<u64> {
            self.stack
                .iter()
                .flatten()
                .find(|(o, _)| *o == offset)
                .map(|&(_, v)| v)
        }

        fn spill(&mut self, offset: i64, value: Option<u64>) {
            for slot in &mut self.stack {
                if slot.is_some_and(|(o, _)| o == offset) {
                    *slot = None;
                }
            }
            if let Some(value) = value
                && let Some(slot) = self.stack.iter_mut().find(|s| s.is_none())
            {
                *slot = Some((offset, value));
            }
        }

        /// What both states agree on.
        fn meet(mut self, other: &Self) -> Self {
            for (m, a) in self.regs.iter_mut().zip(other.regs) {
                if *m != a {
                    *m = None;
                }
            }
            for slot in &mut self.stack {
                if let Some((offset, value)) = *slot
                    && other.spilled(offset) != Some(value)
                {
                    *slot = None;
                }
            }
            self
        }
    }

    /// One decoded instruction.
    struct Step {
        /// Its canonical form (`None` for what the comparison drops).
        text: Option<String>,
        /// The register values after it.
        after: Known,
        /// Its size.
        len: u64,
        /// Where it branches, other than a call.
        target: Option<u64>,
        /// Whether execution can continue with the next instruction.
        falls_through: bool,
    }

    /// A GOT, `.toc` or `.branch_lt` word: what it holds.
    enum Entry {
        /// The address of something.
        Address(String),
        /// Something else (a TLS offset, a module ID, a constant).
        Other(String),
    }

    struct Canon<'e> {
        elf: &'e Elf,
        toc: u64,
    }

    impl Canon<'_> {
        fn reloc_at(&self, address: u64) -> Option<&Reloc> {
            self.elf.relocs.iter().find(|r| r.offset == address)
        }

        /// A symbolic name for an address: the symbol holding it, the TOC
        /// pointer, or a section offset.
        fn describe(&self, address: u64) -> String {
            if address == self.toc {
                return ".TOC.".into();
            }
            // GOT entries are ordered differently by each linker: name
            // them by what they hold.
            if self.is_table(address) {
                return match self.entry(address) {
                    Entry::Address(name) | Entry::Other(name) => format!("&[{name}]"),
                };
            }
            let candidates = self.elf.symbols.iter().filter(|s| {
                matches!(s.kind, 0..=2) && s.shndx != 0 && s.shndx < 0xff00 && !s.name.is_empty()
            });
            let best = candidates
                .clone()
                .filter(|s| s.value <= address && address < s.value + s.size)
                .max_by_key(|s| s.value)
                .or_else(|| {
                    candidates
                        .filter(|s| s.value == address)
                        .max_by_key(|s| s.name.clone())
                });
            if let Some(symbol) = best {
                return if symbol.value == address {
                    symbol.name.clone()
                } else {
                    format!("{}+{:#x}", symbol.name, address - symbol.value)
                };
            }
            // Unnamed data: name it by the first relocation that fills it
            // (a table of pointers, say).
            let next = self
                .elf
                .relocs
                .iter()
                .filter(|r| r.offset >= address && r.offset < address + 64)
                .map(|r| r.offset)
                .min();
            match (self.elf.section_at(address), next) {
                (Some(_), Some(next)) => match self.entry(next) {
                    Entry::Address(name) | Entry::Other(name) => {
                        format!("data[{name}]-{:#x}", next - address)
                    }
                },
                // Merged constants land at different offsets in each
                // linker's output: name them by their bytes.
                (Some(section), None) if section.flags & 4 == 0 && section.sh_type != 8 => {
                    let bytes = self.elf.bytes_at(address, 4).unwrap_or_default();
                    format!("const{bytes:02x?}")
                }
                (Some(section), None) => {
                    format!("{}+{:#x}", section.name, address - section.addr)
                }
                // Not an address in the output: a value the analysis
                // mistook for one.
                (None, _) => format!("?{address:#x}"),
            }
        }

        /// What the 8-byte table word at `address` holds.
        fn entry(&self, address: u64) -> Entry {
            if let Some(reloc) = self.reloc_at(address) {
                let target = || {
                    reloc
                        .symbol
                        .clone()
                        .unwrap_or_else(|| self.describe(reloc.addend as u64))
                };
                return match reloc.r_type {
                    R_PPC64_ADDR64 | R_PPC64_GLOB_DAT | R_PPC64_JMP_SLOT => {
                        Entry::Address(match &reloc.symbol {
                            Some(name) if reloc.addend != 0 => {
                                format!("{name}+{:#x}", reloc.addend)
                            }
                            _ => target(),
                        })
                    }
                    R_PPC64_RELATIVE => Entry::Address(self.describe(reloc.addend as u64)),
                    R_PPC64_IRELATIVE => {
                        Entry::Address(format!("ifunc({})", self.describe(reloc.addend as u64)))
                    }
                    other => Entry::Other(format!("dyn{other}({})", target())),
                };
            }
            let value = self.elf.dword(address).unwrap_or(0);
            if value != 0 && self.elf.section_at(value).is_some() {
                Entry::Address(self.describe(value))
            } else {
                Entry::Other(format!("{value:#x}"))
            }
        }

        fn is_table(&self, address: u64) -> bool {
            self.elf.section_at(address).is_some_and(|s| {
                matches!(
                    s.name.as_str(),
                    ".got" | ".toc" | ".got.plt" | ".plt" | ".branch_lt"
                )
            })
        }

        /// Follows a branch through PLT call stubs and range-extension
        /// thunks to where it ends up.
        fn destination(&self, target: u64, depth: u32) -> String {
            let words: Vec<u32> = (0..9)
                .map_while(|i| self.elf.word(target + i * 4))
                .collect();
            let mut i = 0;
            if words.first() == Some(&0xf841_0018) {
                i = 1; // std r2, 24(r1)
            }
            let at = target + 4 * i as u64;
            if depth >= 4 {
                return self.describe(target);
            }
            // b dest (after saving r2)
            if i == 1
                && let Some(&b) = words.get(1)
                && b & 0xfc00_0003 == 0x4800_0000
            {
                return self.destination(at.wrapping_add_signed(sext_branch(b)), depth + 1);
            }
            // pld r12, slot@pcrel / paddi r12, 0, dest@pcrel, 1; mtctr r12; bctr
            if let [prefix, suffix, 0x7d89_03a6, 0x4e80_0420, ..] = words[i..]
                && prefix & 0xfc10_0000 == 0x0410_0000
            {
                let prefixed = (u64::from(prefix) << 32) | u64::from(suffix);
                let d = sext34(((prefixed >> 16) & 0x3_ffff_0000) | (prefixed & 0xffff));
                let ea = at.wrapping_add_signed(d);
                match suffix & 0xffff_0000 {
                    0xe580_0000 => return self.through_slot(ea, depth),
                    0x3980_0000 => return self.destination(ea, depth + 1),
                    _ => {}
                }
            }
            // addis r12, r2, ha; addi r12, r12, lo; mtctr r12; bctr
            if let [addis, addi, 0x7d89_03a6, 0x4e80_0420, ..] = words[i..]
                && addis >> 16 == 0x3d82
                && addi >> 16 == 0x398c
            {
                let dest = self
                    .toc
                    .wrapping_add_signed(sext16(addis) << 16)
                    .wrapping_add_signed(sext16(addi));
                return self.destination(dest, depth + 1);
            }
            // mflr r12; bcl 20,31,.+4; mflr r11; mtlr r12;
            // addis r12, r11, ha; addi r12, r12, lo (or ld r12, lo(r12));
            // mtctr r12; bctr
            if let [
                0x7d88_02a6,
                0x429f_0005,
                0x7d68_02a6,
                0x7d88_03a6,
                addis,
                low,
                0x7d89_03a6,
                0x4e80_0420,
                ..,
            ] = words[i..]
                && addis >> 16 == 0x3d8b
            {
                let ea = (at + 8)
                    .wrapping_add_signed(sext16(addis) << 16)
                    .wrapping_add_signed(sext16(low & !3));
                match low >> 16 {
                    0x398c => {
                        let dest = (at + 8)
                            .wrapping_add_signed(sext16(addis) << 16)
                            .wrapping_add_signed(sext16(low));
                        return self.destination(dest, depth + 1);
                    }
                    0xe98c => return self.through_slot(ea, depth),
                    _ => {}
                }
            }
            // addis rX, r2, ha; ld r12, lo(rX); mtctr r12; bctr
            if let [addis, ld, 0x7d89_03a6, 0x4e80_0420, ..] = words[i..] {
                let base = if addis >> 26 == 15 && (addis >> 16) & 31 == 2 {
                    Some(self.toc.wrapping_add_signed(sext16(addis) << 16))
                } else {
                    None
                };
                if let Some(base) = base
                    && ld >> 26 == 58
                    && (ld >> 16) & 31 == (addis >> 21) & 31
                {
                    let slot = base.wrapping_add_signed(sext16(ld & !3));
                    return self.through_slot(slot, depth);
                }
            }
            // ld r12, lo(r2); mtctr r12; bctr
            if let [ld, 0x7d89_03a6, 0x4e80_0420, ..] = words[i..]
                && ld >> 26 == 58
                && (ld >> 16) & 31 == 2
            {
                let slot = self.toc.wrapping_add_signed(sext16(ld & !3));
                return self.through_slot(slot, depth);
            }
            self.describe(target)
        }

        fn through_slot(&self, slot: u64, depth: u32) -> String {
            if let Some(reloc) = self.reloc_at(slot)
                && reloc.r_type == R_PPC64_JMP_SLOT
            {
                return format!("plt({})", reloc.symbol.clone().unwrap_or_default());
            }
            match self.entry(slot) {
                Entry::Address(name) if name.starts_with("ifunc(") => format!("plt({name})"),
                Entry::Address(_) if depth < 4 => {
                    let value = self
                        .reloc_at(slot)
                        .map_or_else(|| self.elf.dword(slot).unwrap_or(0), |r| r.addend as u64);
                    self.destination(value, depth + 1)
                }
                Entry::Address(name) | Entry::Other(name) => format!("slot({name})"),
            }
        }

        /// Decodes the instruction at `at` with the register values
        /// `known` holds before it.
        fn step(&self, at: u64, known: &Known) -> Option<Step> {
            let insn = self.elf.word(at)?;
            let op = insn >> 26;
            let rt = ((insn >> 21) & 31) as usize;
            let ra = ((insn >> 16) & 31) as usize;
            let base = |r: usize| -> Option<u64> {
                if r == 0 {
                    return None;
                }
                known.regs[r].or_else(|| (r == 2).then_some(self.toc))
            };
            let mut after = *known;
            let mut step = Step {
                text: None,
                after: *known,
                len: 4,
                target: None,
                falls_through: true,
            };
            match op {
                _ if insn == 0x6000_0000 => {}
                // std rS, d(r1): a spill.
                62 if ra == 1 && insn & 3 == 0 => {
                    step.text = Some(format!("{insn:08x}"));
                    after.spill(sext16(insn & !3), known.regs[rt]);
                }
                // ld rT, d(r1): a reload.
                58 if ra == 1 && insn & 3 == 0 => {
                    step.text = Some(format!("{insn:08x}"));
                    after.regs[rt] = known.spilled(sext16(insn & !3));
                }
                // blr, bctr
                _ if insn == 0x4e80_0020 || insn == 0x4e80_0420 => {
                    step.text = Some(format!("{insn:08x}"));
                    step.falls_through = false;
                }
                1 => {
                    let suffix = self.elf.word(at + 4)?;
                    step.len = 8;
                    let prefixed = (u64::from(insn) << 32) | u64::from(suffix);
                    let d = sext34(((prefixed >> 16) & 0x3_ffff_0000) | (prefixed & 0xffff));
                    let srt = ((suffix >> 21) & 31) as usize;
                    after.regs[srt] = None;
                    let ea = at.wrapping_add_signed(d);
                    step.text = Some(if insn & 0x0010_0000 == 0 {
                        format!("{prefixed:016x}")
                    } else {
                        match suffix >> 26 {
                            14 => format!("addr r{srt}, {}", self.describe(ea)),
                            57 if self.is_table(ea) => match self.entry(ea) {
                                Entry::Address(name) => format!("addr r{srt}, {name}"),
                                Entry::Other(name) => format!("pld r{srt}, [{name}]"),
                            },
                            other => format!("p{other} r{srt}, [{}]", self.describe(ea)),
                        }
                    });
                }
                15 if base(ra).is_some() => {
                    after.regs[rt] = base(ra).map(|b| b.wrapping_add_signed(sext16(insn) << 16));
                }
                14 if base(ra).is_some() => {
                    let ea = base(ra)?.wrapping_add_signed(sext16(insn));
                    step.text = Some(format!("addr r{rt}, {}", self.describe(ea)));
                    after.regs[rt] = Some(ea);
                }
                32..=58 | 61 | 62 if base(ra).is_some() => {
                    let mask = form_mask(op, insn);
                    let d = sext16(insn & !mask);
                    let ea = base(ra)?.wrapping_add_signed(d);
                    let form = insn & mask;
                    step.text = Some(if op == 58 && form == 0 && self.is_table(ea) {
                        match self.entry(ea) {
                            Entry::Address(name) => format!("addr r{rt}, {name}"),
                            Entry::Other(name) => format!("ld r{rt}, [{name}]"),
                        }
                    } else {
                        format!("op{op}.{form} r{rt}, [{}]", self.describe(ea))
                    });
                    if !matches!(op, 36..=39 | 44..=55 | 62) {
                        after.regs[rt] = None;
                    }
                    // A link-time address loaded from a table entry.
                    if op == 58 && form == 0 && self.is_table(ea) {
                        after.regs[rt] = match self.reloc_at(ea) {
                            Some(reloc) if reloc.r_type == R_PPC64_RELATIVE => {
                                Some(reloc.addend as u64)
                            }
                            Some(_) => None,
                            None => self
                                .elf
                                .dword(ea)
                                .filter(|&v| self.elf.section_at(v).is_some()),
                        };
                    }
                }
                18 => {
                    let target = if insn & 2 != 0 {
                        sext_branch(insn) as u64
                    } else {
                        at.wrapping_add_signed(sext_branch(insn))
                    };
                    let link = insn & 1 != 0;
                    step.text = Some(format!(
                        "{} {}",
                        if link { "bl" } else { "b" },
                        self.destination(target, 0)
                    ));
                    if link {
                        // A call clobbers the volatile registers.
                        for register in [0, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12] {
                            after.regs[register] = None;
                        }
                    } else {
                        step.target = Some(target);
                        step.falls_through = false;
                    }
                }
                16 => {
                    let target = at.wrapping_add_signed(sext16(insn & 0xfffc));
                    step.text = Some(format!(
                        "bc {:#x} {}",
                        insn & 0x03ff_0003,
                        self.describe(target)
                    ));
                    step.target = Some(target);
                }
                _ => {
                    step.text = Some(format!("{insn:08x}"));
                    // A load or store from a base the analysis cannot
                    // follow (a register the other linker's code may have
                    // made TOC-relative).
                    if matches!(op, 14 | 15 | 32..=58 | 61 | 62) && ra != 0 && ra != 1 {
                        let mask = form_mask(op, insn);
                        let form = insn & mask;
                        let d = sext16(insn & !mask);
                        step.text = Some(format!("op{op}.{form} r{rt}, [?r{ra}{d:+}]"));
                    }
                    // Stores and compares write no register there.
                    // Stores, compares, and vector and floating-point
                    // instructions write no general register there.
                    if !matches!(op, 4 | 10 | 11 | 17 | 19 | 36..=39 | 44 | 45 | 48..=55 | 57 | 59..=63)
                    {
                        after.regs[rt] = None;
                    }
                    // Logical and rotate instructions write rA.
                    if matches!(op, 20..=31) {
                        after.regs[ra] = None;
                    }
                    // bctrl, blrl: an indirect call.
                    if op == 19 && insn & 1 != 0 {
                        for register in [0, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12] {
                            after.regs[register] = None;
                        }
                    }
                }
            }
            if after.regs[1] != known.regs[1]
                || (op == 62 && ra == 1 && insn & 3 == 1)
                || (op == 14 && rt == 1)
            {
                // The stack pointer moved.
                after.stack = [None; 16];
            }
            step.after = after;
            Some(step)
        }

        /// The canonical instructions of the `size` bytes at `start`. The
        /// register values the TOC-relative addressing builds are tracked
        /// along the function's branches (a value is known at a join only
        /// if every path agrees on it), since compilers hoist an `addis`
        /// out of a loop.
        fn body(&self, start: u64, size: u64) -> Vec<String> {
            let end = start + size;
            let mut entry = Known::NONE;
            entry.regs[12] = Some(start);
            let mut states: BTreeMap<u64, Known> = BTreeMap::new();
            states.insert(start, entry);
            let mut work = vec![start];
            while let Some(pc) = work.pop() {
                let Some(state) = states.get(&pc).copied() else {
                    continue;
                };
                let Some(step) = self.step(pc, &state) else {
                    continue;
                };
                let next = step.falls_through.then_some(pc + step.len);
                for successor in [next, step.target].into_iter().flatten() {
                    if successor < start || successor >= end {
                        continue;
                    }
                    let merged = match states.get(&successor) {
                        None => step.after,
                        Some(old) => old.meet(&step.after),
                    };
                    if states.get(&successor) != Some(&merged) {
                        states.insert(successor, merged);
                        work.push(successor);
                    }
                }
            }
            let mut out = Vec::new();
            let mut pc = start;
            while pc < end {
                let state = states.get(&pc).copied().unwrap_or(Known::NONE);
                let Some(step) = self.step(pc, &state) else {
                    break;
                };
                out.extend(step.text);
                pc += step.len;
            }
            out
        }

        /// Every dynamic relocation, as `type target @ place`.
        fn relocations(&self) -> Vec<String> {
            let mut out: Vec<String> = self
                .elf
                .relocs
                .iter()
                .map(|reloc| {
                    let place = if self.is_table(reloc.offset) {
                        "table".to_string()
                    } else {
                        self.describe(reloc.offset)
                    };
                    let target = match (reloc.r_type, &reloc.symbol) {
                        (_, Some(name)) => format!("{name}+{:#x}", reloc.addend),
                        (R_PPC64_RELATIVE | R_PPC64_IRELATIVE, None) => {
                            self.describe(reloc.addend as u64)
                        }
                        (_, None) => format!("{:#x}", reloc.addend),
                    };
                    // Unnamed read-only data is compared by kind only (see
                    // `equivalent`).
                    let target = match target.split_once("const[") {
                        Some((head, _)) => format!("{head}const"),
                        None => target,
                    };
                    format!("{} {target} @ {place}", reloc.r_type)
                })
                .collect();
            out.sort();
            out
        }
    }

    /// The low displacement bits a DS- or DQ-form instruction uses for
    /// its opcode extension.
    fn form_mask(op: u32, insn: u32) -> u32 {
        match op {
            56 => 0xf,
            61 if insn & 3 == 1 => 0xf,
            57 | 58 | 61 | 62 => 3,
            _ => 0,
        }
    }

    fn sext_branch(insn: u32) -> i64 {
        i64::from(((insn & 0x03ff_fffc) << 6) as i32 >> 6)
    }

    /// The functions of `elf` with their extent: sized function symbols,
    /// and unsized ones up to the next symbol.
    fn functions(elf: &Elf) -> BTreeMap<String, (u64, u64, u8)> {
        let mut starts: Vec<u64> = elf
            .symbols
            .iter()
            .filter(|s| s.shndx != 0 && s.shndx < 0xff00 && s.kind != 3 && s.kind != 4)
            .map(|s| s.value)
            .collect();
        starts.sort_unstable();
        starts.dedup();
        let mut out = BTreeMap::new();
        for symbol in &elf.symbols {
            if symbol.kind != 2 || symbol.shndx == 0 || symbol.shndx >= 0xff00 {
                continue;
            }
            let size = if symbol.size != 0 {
                symbol.size
            } else {
                let section = &elf.sections[symbol.shndx as usize];
                let next = starts
                    .iter()
                    .copied()
                    .find(|&s| s > symbol.value)
                    .unwrap_or(section.addr + section.size);
                next.min(section.addr + section.size) - symbol.value
            };
            out.insert(symbol.name.clone(), (symbol.value, size, symbol.other));
        }
        out
    }

    /// Whether two canonical instructions agree: equal, or one is an
    /// access through a base register the analysis could not resolve and
    /// the other the same access resolved to a symbol.
    fn equivalent(x: &str, y: &str) -> bool {
        if x == y {
            return true;
        }
        let unresolved = |a: &str, b: &str| {
            a.split_once(", [?").is_some_and(|(head, _)| {
                b.split_once(", [").is_some_and(|(other, _)| head == other)
                    // An `addi` from an unresolved base, and the address
                    // the other linker's code computes there.
                    || head
                        .strip_prefix("op14.0 ")
                        .is_some_and(|register| b.starts_with(&format!("addr {register}, ")))
            })
        };
        let stray = |a: &str| a.split_once("?0x").map(|(head, _)| head.to_string());
        if let (Some(a), Some(b)) = (stray(x), stray(y)) {
            return a == b;
        }
        // Unnamed read-only data (merged constants, jump tables whose
        // entries are link-time offsets) cannot be told apart reliably.
        let unnamed = |a: &str| a.split_once("const[").map(|(head, _)| head.to_string());
        if let (Some(a), Some(b)) = (unnamed(x), unnamed(y)) {
            return a == b;
        }
        unresolved(x, y) || unresolved(y, x)
    }

    /// Checks `.glink`: `DT_PPC64_GLINK + 32` is the first lazy entry, each
    /// entry branches back to the resolver, and the resolver reaches
    /// `DT_PLTGOT`.
    fn check_glink(label: &str, elf: &Elf) {
        let slots = elf
            .relocs
            .iter()
            .filter(|r| r.r_type == R_PPC64_JMP_SLOT)
            .count() as u64;
        if slots == 0 {
            return;
        }
        let glink = elf
            .dynamic_value(DT_PPC64_GLINK)
            .unwrap_or_else(|| panic!("{label}: no DT_PPC64_GLINK"));
        let pltgot = elf
            .dynamic_value(3)
            .unwrap_or_else(|| panic!("{label}: no DT_PLTGOT"));
        let first = glink + 32;
        let header = first - 60;
        assert_eq!(elf.word(header), Some(0x7c08_02a6), "{label}: resolver");
        let offset = elf.dword(header + 52).unwrap();
        assert_eq!(
            header + 8 + offset,
            pltgot,
            "{label}: resolver reaches .plt"
        );
        for index in 0..slots {
            let entry = first + index * 4;
            let insn = elf.word(entry).unwrap();
            assert_eq!(insn >> 26, 18, "{label}: lazy entry {index} is a branch");
            assert_eq!(
                entry.wrapping_add_signed(sext_branch(insn)),
                header,
                "{label}: lazy entry {index} branches to the resolver"
            );
        }
    }

    /// Asserts that `ours` relocated every function it shares with
    /// `theirs` (linked by `label`) the same way, and has the same dynamic
    /// relocations.
    pub fn assert_equivalent(label: &str, theirs: &Elf, ours: &Elf) {
        let (a, b) = (
            Canon {
                elf: theirs,
                toc: theirs.toc(),
            },
            Canon {
                elf: ours,
                toc: ours.toc(),
            },
        );
        let their_functions = functions(theirs);
        let our_functions = functions(ours);
        let mut compared = 0;
        for (name, &(start, size, other)) in &our_functions {
            let Some(&(their_start, their_size, their_other)) = their_functions.get(name) else {
                continue;
            };
            assert_eq!(other, their_other, "{name}: st_other differs from {label}");
            let size = size.min(their_size);
            let (ours_body, theirs_body) = (b.body(start, size), a.body(their_start, size));
            if let Some(at) = (0..ours_body.len().max(theirs_body.len())).find(|&i| {
                match (ours_body.get(i), theirs_body.get(i)) {
                    (Some(x), Some(y)) => !equivalent(x, y),
                    _ => true,
                }
            }) {
                let window = |body: &[String]| {
                    body.get(at.saturating_sub(3)..(at + 4).min(body.len()))
                        .unwrap_or_default()
                        .to_vec()
                };
                panic!(
                    "{name} was relocated differently from {label} at canonical instruction {at}\n{label}: {:#?}\nqld: {:#?}",
                    window(&theirs_body),
                    window(&ours_body)
                );
            }
            compared += 1;
        }
        assert!(compared > 0, "no function compared with {label}");
        assert_eq!(
            a.relocations(),
            b.relocations(),
            "dynamic relocations differ from {label}"
        );
        for tag in [1i64, 14, 2, 20, 23] {
            // NEEDED, SONAME, PLTRELSZ, PLTREL, JMPREL presence.
            assert_eq!(
                theirs.dynamic_value(tag).is_some(),
                ours.dynamic_value(tag).is_some(),
                "dynamic tag {tag} differs from {label}"
            );
        }
        assert_eq!(
            theirs.dynamic_value(2),
            ours.dynamic_value(2),
            "DT_PLTRELSZ differs from {label}"
        );
        check_glink(label, ours);
        check_glink(label, theirs);
    }
}

const TOC_MAIN: &str = r#"
extern int ext_var;
extern int ext_func(int);
static int local_var = 5;
int global_var = 7;
long table[4] = {1, 2, 3, 4};
long *table_pointer = &table[2];

static int helper(int x) { return x * 3 + local_var; }

__attribute__((noinline)) int caller(int x) {
  return ext_func(x) + helper(x) + ext_var + global_var + (int)table[x & 3];
}

__attribute__((noinline)) int *addr_of(void) { return &ext_var; }

__attribute__((noinline)) long through_pointer(void) { return *table_pointer; }

static void sys_exit(long code) {
  register long r0 __asm__("r0") = 1;
  register long r3 __asm__("r3") = code;
  __asm__ volatile("sc" : "+r"(r0), "+r"(r3) : : "memory", "cr0");
  for (;;) {
  }
}

void _start(void) { sys_exit(caller(1) + *addr_of() + through_pointer()); }
"#;

const TOC_OTHER: &str = r#"
int ext_var = 11;
int ext_func(int x) { return x + ext_var; }
"#;

/// Calls between objects enter at the local entry point, and TOC-indirect
/// loads through `.toc` become TOC-relative, in static and
/// position-independent executables.
#[test]
fn toc_accesses_and_calls_match() {
    let tools = require!();
    let dir = scratch("toc");
    for (name, flags) in [("pic", "-fPIC"), ("nopic", "-fno-PIC")] {
        compile(
            tools,
            &dir,
            &format!("main-{name}.c"),
            TOC_MAIN,
            &["-O2", flags],
        );
        compile(
            tools,
            &dir,
            &format!("other-{name}.c"),
            TOC_OTHER,
            &["-O2", flags],
        );
    }
    link_and_compare(
        tools,
        &dir,
        "static",
        &["-static", "-e", "_start", "main-nopic.o", "other-nopic.o"],
    );
    let pie = link_and_compare(
        tools,
        &dir,
        "pie",
        &["-pie", "-e", "_start", "main-pic.o", "other-pic.o"],
    );
    assert!(
        pie.relocs.iter().all(|r| r.r_type == 22),
        "a PIE with everything local needs only relative relocations"
    );
}

const LIBRARY: &str = r#"
int lib_var = 42;
int lib_func(int x) { return x + lib_var; }
int lib_other(int x) { return lib_func(x) * 2; }
"#;

const DYNAMIC_MAIN: &str = r#"
extern int lib_var;
extern int lib_func(int);
extern int lib_other(int);
int (*pointer)(int) = lib_other;

__attribute__((noinline)) int use_library(int x) {
  return lib_func(x) + lib_other(x) + lib_var + pointer(x);
}

int main(void) { return use_library(1); }
"#;

/// A shared library and an executable using it: PLT call stubs that save
/// and restore `r2`, `.glink`, `JMP_SLOT` and `GLOB_DAT` relocations.
#[test]
fn dynamic_linking_matches() {
    let tools = require!();
    let dir = scratch("dynamic");
    compile(tools, &dir, "lib.c", LIBRARY, &["-O2", "-fPIC"]);
    compile(tools, &dir, "main.c", DYNAMIC_MAIN, &["-O2", "-fPIC"]);
    link_and_compare(
        tools,
        &dir,
        "libdemo.so",
        &["-shared", "-soname", "libdemo.so", "lib.o"],
    );
    for (name, extra) in [("pie", &["-pie"][..]), ("exe", &[][..])] {
        let mut args: Vec<&str> = extra.to_vec();
        args.extend([
            "-e",
            "main",
            "--dynamic-linker",
            "/lib64/ld64.so.2",
            "main.o",
            "libdemo.so",
        ]);
        link_and_compare(tools, &dir, name, &args);
        args.extend(["-z", "now"]);
        link_and_compare(tools, &dir, &format!("{name}-now"), &args);
    }
}

const TLS_DEFS: &str = r#"
__thread int tls_gd = 3;
static __thread int tls_ld_a = 4;
static __thread int tls_ld_b = 5;
__thread int tls_ie __attribute__((tls_model("initial-exec"))) = 6;
__thread long tls_big[3] = {1, 2, 3};

__attribute__((noinline)) int *gd_address(void) { return &tls_gd; }
__attribute__((noinline)) int ld_sum(void) { return tls_ld_a + tls_ld_b; }
__attribute__((noinline)) int ie_value(void) { return tls_ie; }
__attribute__((noinline)) long big_value(int i) { return tls_big[i]; }
"#;

const TLS_LOCAL_EXEC: &str = r#"
__thread int tls_le __attribute__((tls_model("local-exec"))) = 7;
__attribute__((noinline)) int le_value(void) { return tls_le; }
"#;

const TLS_USER: &str = r#"
extern __thread int tls_gd;
extern __thread int shared_tls;
__attribute__((noinline)) int from_other(void) { return tls_gd + shared_tls; }
"#;

const TLS_SHARED: &str = r#"
__thread int shared_tls = 8;
int shared_function(int x) { return x + 1; }
"#;

/// The TLS models: relaxed to local-exec in an executable, to
/// initial-exec for a shared library's variable, and kept dynamic in a
/// shared library.
#[test]
fn tls_models_match() {
    let tools = require!();
    let dir = scratch("tls");
    for (name, source) in [
        ("defs", TLS_DEFS),
        ("user", TLS_USER),
        ("shared", TLS_SHARED),
        ("le", TLS_LOCAL_EXEC),
    ] {
        compile(tools, &dir, &format!("{name}.c"), source, &["-O2", "-fPIC"]);
    }
    link_and_compare(
        tools,
        &dir,
        "libshared.so",
        &["-shared", "-soname", "libshared.so", "shared.o"],
    );
    link_and_compare(
        tools,
        &dir,
        "libtls.so",
        &["-shared", "defs.o", "user.o", "libshared.so"],
    );
    link_and_compare(
        tools,
        &dir,
        "exe",
        &[
            "-pie",
            "-e",
            "le_value",
            "defs.o",
            "user.o",
            "le.o",
            "libshared.so",
        ],
    );
}

const PCREL: &str = r#"
extern int ext_var;
extern int ext_func(int);
static int local_var = 5;
__thread int tls_gd = 3;
static __thread int tls_ld = 4;
extern __thread int tls_ie __attribute__((tls_model("initial-exec")));

__attribute__((noinline)) int pcrel_caller(int x) {
  return ext_func(x) + local_var + ext_var;
}
__attribute__((noinline)) int *pcrel_address(void) { return &ext_var; }
__attribute__((noinline)) int pcrel_tls(void) { return tls_gd + tls_ld + tls_ie; }
"#;

const PCREL_OTHER: &str = r#"
int ext_var = 11;
__thread int tls_ie = 9;
int ext_func(int x) { return x + ext_var; }
"#;

const TOC_CALLER: &str = r#"
extern int pcrel_caller(int);
extern int ext_var;
__attribute__((noinline)) int toc_caller(int x) { return pcrel_caller(x) + ext_var; }
"#;

const PCREL_LIBRARY: &str = r#"
extern int outside_function(int);
extern int lib_data;
__attribute__((noinline)) int calls_outside(int x) { return outside_function(x) + lib_data; }
int lib_data = 5;
"#;

/// Power10 PC-relative code: `R_PPC64_GOT_PCREL34` relaxed to
/// `R_PPC64_PCREL34` (with `R_PPC64_PCREL_OPT`), `R_PPC64_REL24_NOTOC`
/// calls into TOC-based code (through a thunk that sets up `r12`) and
/// through the PLT (a stub that loads the PLT word PC-relatively), calls
/// from TOC-based code into PC-relative code, and the PC-relative TLS
/// sequences.
#[test]
fn power10_pcrel_matches() {
    let tools = require!();
    let dir = scratch("pcrel");
    let power10 = ["-O2", "-fPIC", "-mcpu=power10"];
    compile(tools, &dir, "pcrel.c", PCREL, &power10);
    compile(tools, &dir, "library.c", PCREL_LIBRARY, &power10);
    // ext_func is TOC-based code, called from PC-relative code.
    compile(
        tools,
        &dir,
        "other.c",
        PCREL_OTHER,
        &["-O2", "-fPIC", "-mcpu=power8"],
    );
    compile(
        tools,
        &dir,
        "toc.c",
        TOC_CALLER,
        &["-O2", "-fPIC", "-mcpu=power8"],
    );
    link_and_compare(
        tools,
        &dir,
        "exe",
        &["-pie", "-e", "pcrel_caller", "pcrel.o", "other.o", "toc.o"],
    );
    // Preemptible calls in a shared library go through the PLT.
    link_and_compare(tools, &dir, "libpcrel.so", &["-shared", "library.o"]);
}

const FAR_CALLS: &str = r#"
	.abiversion 2
	.text
	.globl	near_caller
	.type	near_caller, @function
near_caller:
0:	addis	2, 12, .TOC.-0b@ha
	addi	2, 2, .TOC.-0b@l
	.localentry	near_caller, .-near_caller
	mflr	0
	std	0, 16(1)
	stdu	1, -32(1)
	bl	far_function
	nop
	bl	far_function
	nop
	addi	1, 1, 32
	ld	0, 16(1)
	mtlr	0
	blr
	.size	near_caller, .-near_caller

	.section .fartext, "ax", @progbits
	.p2align 4
	.skip	0x2100000
	.globl	far_function
	.type	far_function, @function
far_function:
0:	addis	2, 12, .TOC.-0b@ha
	addi	2, 2, .TOC.-0b@l
	.localentry	far_function, .-far_function
	li	3, 7
	blr
	.size	far_function, .-far_function
"#;

/// Calls farther than `bl` reaches go through a range-extension thunk
/// that enters the callee at its global entry point, shared by callers.
#[test]
fn far_calls_get_thunks() {
    let tools = require!();
    let dir = scratch("thunks");
    compile(tools, &dir, "far.s", FAR_CALLS, &[]);
    let ours = link_and_compare(
        tools,
        &dir,
        "exe",
        &["-static", "-e", "near_caller", "far.o"],
    );
    // Both calls go through one thunk, which is in `.text`.
    let text = ours.section(".text").unwrap();
    let mut thunks = 0;
    let mut address = text.addr;
    while address + 32 <= text.addr + text.size {
        if ours.word(address) == Some(0x7d88_02a6) && ours.word(address + 4) == Some(0x429f_0005) {
            thunks += 1;
        }
        address += 4;
    }
    assert_eq!(thunks, 1, "expected one shared thunk");
}

/// Links the link arguments in `QLD_PPC64_LINK` (objects given by absolute
/// path) with qld and the reference linkers and compares the outputs, for
/// checking larger programs by hand:
/// `QLD_PPC64_LINK="-shared /tmp/sqlite.o" cargo test --test ppc64 -- --ignored`.
#[test]
#[ignore = "links objects named by QLD_PPC64_LINK"]
fn external_link_matches() {
    let tools = require!();
    let Ok(line) = std::env::var("QLD_PPC64_LINK") else {
        println!("SKIPPED: QLD_PPC64_LINK is not set");
        return;
    };
    let dir = scratch("external");
    let args: Vec<&str> = line.split_whitespace().collect();
    link_and_compare(tools, &dir, "out", &args);
}

/// `-r` keeps the machine and the ELFv2 flag.
#[test]
fn relocatable_output_keeps_the_abi() {
    let tools = require!();
    let dir = scratch("relocatable");
    compile(tools, &dir, "other.c", TOC_OTHER, &["-O2"]);
    qld_ok(&dir, &["-r", "-o", "combined.o", "other.o"]);
    let data = fs::read(dir.join("combined.o")).unwrap();
    assert_eq!(u16::from_le_bytes([data[18], data[19]]), 21);
    assert_eq!(
        u32::from_le_bytes([data[48], data[49], data[50], data[51]]),
        2
    );
}

/// The emulation name selects PowerPC64, and a big-endian object is
/// rejected rather than linked as little-endian.
#[test]
fn emulation_selects_the_backend() {
    let tools = require!();
    let dir = scratch("emulation");
    compile(tools, &dir, "other.c", TOC_OTHER, &["-O2"]);
    qld_ok(
        &dir,
        &["-m", "elf64lppc", "-shared", "-o", "lib.so", "other.o"],
    );
    let lib = elf::Elf::read(&dir.join("lib.so"));
    assert_eq!(lib.e_flags, 2);
}
