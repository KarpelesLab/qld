//! Integration tests for the ELF reader (`qld::elf::read`, workstream W7).
//!
//! Most tests compile small fixtures with the host toolchain at test time and
//! compare what qld parses with `readelf` output. They skip (pass with a
//! message) when a tool is missing. Prebuilt fixtures under
//! `tests/data/elf_read/` keep the parsing and corruption tests meaningful
//! on machines without a toolchain.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use qld::Error;
use qld::elf::read::consts::{
    self, ELFCOMPRESS_ZLIB, ET_DYN, ET_REL, SHT_RELR, SHT_SYMTAB, STB_GLOBAL, STB_GNU_UNIQUE,
    STB_LOCAL, STB_WEAK, STT_COMMON, STT_FILE, STT_FUNC, STT_GNU_IFUNC, STT_NOTYPE, STT_OBJECT,
    STT_SECTION, STT_TLS, STV_DEFAULT, STV_HIDDEN, STV_INTERNAL, STV_PROTECTED,
};
use qld::elf::read::{
    Elf32Be, Elf32Le, Elf64Be, Elf64Le, ElfFile, ElfFormat, ElfKind, GnuProperties, ObjectFile,
    SectionIndex, SharedObject, Source, VersionKind,
};

// ---------------------------------------------------------------------------
// Inventory: everything qld can read from a file, in plain owned form.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
struct Sec {
    name: String,
    sh_type: u32,
    flags: u64,
    offset: u64,
    size: u64,
    entsize: u64,
    link: u32,
    info: u32,
    align: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Sym {
    name: String,
    value: u64,
    size: u64,
    kind: u8,
    bind: u8,
    vis: u8,
    section: SectionIndex,
    /// `readelf`-style version suffix for dynamic symbols (`@V`, `@@V`,
    /// `@V (n)`), empty otherwise.
    version: String,
    version_name: String,
}

#[derive(Debug, Clone)]
struct Reloc {
    offset: u64,
    symbol: u32,
    r_type: u32,
    addend: i64,
}

#[derive(Debug, Clone)]
struct RelocSec {
    name: String,
    rela: bool,
    entries: Vec<Reloc>,
}

#[derive(Debug, Clone)]
struct GroupInfo {
    signature: String,
    comdat: bool,
    members: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FrameRecord {
    offset: usize,
    cie: Option<usize>,
    pc_begin_reloc: Option<usize>,
}

#[derive(Debug, Default)]
struct Inventory {
    machine: u16,
    e_type: u16,
    word_size: usize,
    sections: Vec<Sec>,
    symtabs: BTreeMap<String, Vec<Sym>>,
    relocs: Vec<RelocSec>,
    relr: Vec<(String, usize)>,
    groups: Vec<GroupInfo>,
    eh_frames: Vec<Vec<FrameRecord>>,
    compressed: Vec<(String, u32, u64)>,
    properties: GnuProperties,
    lto: bool,
    soname: Option<String>,
    needed: Vec<String>,
    gnu_hash: bool,
}

/// Collects errors instead of stopping, so corrupted inputs exercise as
/// much of the reader as possible.
#[derive(Default)]
struct Walker {
    errors: Vec<Error>,
}

impl Walker {
    fn ok<T>(&mut self, r: qld::Result<T>) -> Option<T> {
        match r {
            Ok(v) => Some(v),
            Err(e) => {
                self.errors.push(e);
                None
            }
        }
    }
}

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn sections_of<F: ElfFormat>(elf: &ElfFile<'_, F>, w: &mut Walker) -> Vec<Sec> {
    elf.enumerate_sections()
        .map(|(_, h)| Sec {
            name: w.ok(elf.section_name(&h)).map(lossy).unwrap_or_default(),
            sh_type: h.sh_type,
            flags: h.sh_flags,
            offset: h.sh_offset,
            size: h.sh_size,
            entsize: h.sh_entsize,
            link: h.sh_link,
            info: h.sh_info,
            align: h.sh_addralign,
        })
        .collect()
}

fn common_tables<F: ElfFormat>(elf: &ElfFile<'_, F>, inv: &mut Inventory, w: &mut Walker) {
    for (index, hdr) in elf.enumerate_sections() {
        let name = elf.section_name(&hdr).map(lossy).unwrap_or_default();
        if let Some(Some(rs)) = w.ok(elf.relocation_section(index, &hdr)) {
            let count = rs.relocations.len();
            let entries = (0..count)
                .filter_map(|i| rs.relocations.get(i))
                .map(|r| Reloc {
                    offset: r.offset,
                    symbol: r.symbol,
                    r_type: r.r_type,
                    addend: r.addend,
                })
                .collect();
            inv.relocs.push(RelocSec {
                name: name.clone(),
                rela: rs.relocations.is_rela(),
                entries,
            });
        }
        if hdr.sh_type == SHT_RELR
            && let Some(Some(relr)) = w.ok(elf.relr_section(&hdr))
        {
            inv.relr
                .push((name.clone(), relr.iter().take(1 << 24).count()));
        }
        if hdr.sh_type == SHT_SYMTAB && inv.e_type != ET_REL {
            // Objects read their symbol table through ObjectFile instead.
            if let Some(table) = w.ok(elf.symbol_table(index)) {
                let mut syms = Vec::new();
                for s in table.iter() {
                    if let Some(s) = w.ok(s) {
                        syms.push(plain_symbol(&s));
                    }
                }
                inv.symtabs.insert(name, syms);
            }
        }
    }
}

fn plain_symbol(s: &qld::elf::read::Symbol<'_>) -> Sym {
    Sym {
        name: lossy(s.name),
        value: s.value,
        size: s.size,
        kind: s.kind(),
        bind: s.binding(),
        vis: s.visibility(),
        section: s.section,
        version: String::new(),
        version_name: String::new(),
    }
}

fn object_inventory<F: ElfFormat>(obj: &ObjectFile<'_, F>, w: &mut Walker) -> Inventory {
    let elf = obj.elf();
    let mut inv = Inventory {
        machine: elf.header().e_machine,
        e_type: elf.header().e_type,
        word_size: F::WORD_SIZE,
        sections: sections_of(elf, w),
        ..Inventory::default()
    };
    common_tables(elf, &mut inv, w);

    let symtab_name = obj
        .section_header(obj.symbols().section_index())
        .and_then(|h| obj.section_name(&h))
        .map(lossy)
        .unwrap_or_else(|_| ".symtab".into());
    let mut syms = Vec::with_capacity(obj.symbol_count());
    for s in obj.symbols().iter() {
        if let Some(s) = w.ok(s) {
            syms.push(plain_symbol(&s));
        }
    }
    // Globals iterator agrees with the full one.
    assert_eq!(
        obj.symbols().globals().len(),
        obj.symbol_count() - obj.symbols().first_global()
    );
    if !obj.symbols().is_empty() {
        inv.symtabs.insert(symtab_name, syms);
    }

    for group in obj.groups() {
        if let Some(g) = w.ok(group) {
            let signature = w.ok(obj.group_signature(&g)).map(lossy).unwrap_or_default();
            inv.groups.push(GroupInfo {
                signature,
                comdat: g.is_comdat(),
                members: g.members().collect(),
            });
        }
    }

    let reloc_map = w.ok(obj.relocation_map());
    for (index, hdr) in elf.enumerate_sections() {
        if let Some(Some((chdr, rest))) = w.ok(obj.compressed_data(&hdr)) {
            assert!(rest.len() as u64 <= hdr.sh_size);
            let name = elf.section_name(&hdr).map(lossy).unwrap_or_default();
            inv.compressed.push((name, chdr.ch_type, chdr.ch_size));
        }
        if w.ok(obj.is_eh_frame(&hdr)) == Some(true) {
            let rel_index = reloc_map
                .as_ref()
                .and_then(|m| m.get(index as usize).copied())
                .unwrap_or(0);
            let relocs = if rel_index != 0 {
                obj.section_header(rel_index)
                    .and_then(|h| obj.relocation_section(rel_index, &h))
                    .ok()
                    .flatten()
            } else {
                None
            };
            if let Some(entries) = w.ok(obj.eh_frame(&hdr, relocs.as_ref())) {
                inv.eh_frames.push(
                    entries
                        .iter()
                        .map(|e| {
                            let _ = e.record.augmentation();
                            FrameRecord {
                                offset: e.record.offset,
                                cie: match e.record.kind {
                                    qld::elf::read::EhFrameRecordKind::Cie => None,
                                    qld::elf::read::EhFrameRecordKind::Fde { cie_offset } => {
                                        Some(cie_offset)
                                    }
                                },
                                pc_begin_reloc: e.pc_begin_relocation,
                            }
                        })
                        .collect(),
                );
            }
        }
    }
    if let Some(p) = w.ok(obj.gnu_properties()) {
        inv.properties = p;
    }
    inv.lto = w.ok(obj.has_gcc_lto_ir()).unwrap_or(false);
    inv
}

fn shared_inventory<F: ElfFormat>(so: &SharedObject<'_, F>, w: &mut Walker) -> Inventory {
    let elf = so.elf();
    let mut inv = Inventory {
        machine: elf.header().e_machine,
        e_type: elf.header().e_type,
        word_size: F::WORD_SIZE,
        sections: sections_of(elf, w),
        soname: so.soname().map(lossy),
        gnu_hash: so.has_gnu_hash(),
        ..Inventory::default()
    };
    let _ = so.has_sysv_hash();
    common_tables(elf, &mut inv, w);
    for needed in so.needed() {
        if let Some(n) = w.ok(needed) {
            inv.needed.push(lossy(n));
        }
    }
    let dynsym_name = so
        .elf()
        .section_header(so.symbols().section_index())
        .and_then(|h| so.elf().section_name(&h))
        .map(lossy)
        .unwrap_or_else(|_| ".dynsym".into());
    let mut syms = Vec::new();
    for ds in so.dynamic_symbols() {
        let Some(ds) = w.ok(ds) else { continue };
        let mut sym = plain_symbol(&ds.symbol);
        if let Some(info) = ds.version.info {
            sym.version_name = lossy(info.name);
            sym.version = match info.kind {
                VersionKind::Needed { .. } => {
                    format!("@{} ({})", lossy(info.name), ds.version.index)
                }
                VersionKind::Defined if ds.version.hidden => format!("@{}", lossy(info.name)),
                VersionKind::Defined => format!("@@{}", lossy(info.name)),
            };
        }
        syms.push(sym);
    }
    if !so.symbols().is_empty() {
        inv.symtabs.insert(dynsym_name, syms);
    }
    inv
}

/// Parses `data` with the reader matching its class, byte order and type.
fn inventory(data: &[u8], path: &Path) -> (Option<Inventory>, Vec<Error>) {
    let mut w = Walker::default();
    let source = Source::new(path);
    let Some(kind) = ElfKind::identify(data) else {
        return (None, w.errors);
    };
    macro_rules! go {
        ($f:ty) => {{
            let inv = match w.ok(ElfFile::<$f>::parse(data, source)) {
                None => None,
                Some(elf) => match elf.header().e_type {
                    ET_REL => w
                        .ok(ObjectFile::<$f>::parse(data, source))
                        .map(|o| object_inventory(&o, &mut w)),
                    ET_DYN => w
                        .ok(SharedObject::<$f>::parse(data, source))
                        .map(|s| shared_inventory(&s, &mut w)),
                    _ => {
                        let mut inv = Inventory {
                            machine: elf.header().e_machine,
                            e_type: elf.header().e_type,
                            word_size: <$f as ElfFormat>::WORD_SIZE,
                            sections: sections_of(&elf, &mut w),
                            ..Inventory::default()
                        };
                        common_tables(&elf, &mut inv, &mut w);
                        Some(inv)
                    }
                },
            };
            (inv, w.errors)
        }};
    }
    match kind {
        ElfKind::Elf64Le => go!(Elf64Le),
        ElfKind::Elf64Be => go!(Elf64Be),
        ElfKind::Elf32Le => go!(Elf32Le),
        ElfKind::Elf32Be => go!(Elf32Be),
    }
}

fn parse_ok(path: &Path) -> Inventory {
    let data = std::fs::read(path).unwrap();
    let (inv, errors) = inventory(&data, path);
    assert!(errors.is_empty(), "{}: {errors:?}", path.display());
    inv.unwrap_or_else(|| panic!("{}: not ELF", path.display()))
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

fn data_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/elf_read")
}

fn scratch_dir() -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("elf_read");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn tool_works(tool: &str) -> bool {
    Command::new(tool)
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Runs a compiler command producing `out`; returns `None` (and says why) if
/// the tool is missing, fails, or does not produce an ELF file.
fn build(tool: &str, args: &[&str], out: &Path) -> Option<PathBuf> {
    let result = Command::new(tool).args(args).arg("-o").arg(out).output();
    match result {
        Ok(o) if o.status.success() => {}
        Ok(o) => {
            eprintln!(
                "skipping: `{tool} {}` failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&o.stderr)
            );
            return None;
        }
        Err(e) => {
            eprintln!("skipping: {tool} unavailable: {e}");
            return None;
        }
    }
    let data = std::fs::read(out).ok()?;
    if ElfKind::identify(&data).is_none() {
        eprintln!("skipping: {tool} does not produce ELF on this host");
        return None;
    }
    Some(out.to_path_buf())
}

fn readelf(args: &[&str], path: &Path) -> Option<String> {
    let o = Command::new("readelf").args(args).arg(path).output().ok()?;
    if !o.status.success() {
        eprintln!(
            "readelf {args:?} failed: {}",
            String::from_utf8_lossy(&o.stderr)
        );
        return None;
    }
    Some(String::from_utf8_lossy(&o.stdout).into_owned())
}

// ---------------------------------------------------------------------------
// readelf output parsing
// ---------------------------------------------------------------------------

fn hex(s: &str) -> u64 {
    u64::from_str_radix(s.trim_start_matches("0x"), 16).unwrap_or_else(|_| panic!("bad hex {s:?}"))
}

#[derive(Debug)]
struct RSec {
    name: String,
    offset: u64,
    size: u64,
    entsize: u64,
    link: u32,
    info: u32,
    align: u64,
}

fn parse_readelf_sections(out: &str) -> (usize, Vec<RSec>) {
    let mut count = 0;
    let mut sections = Vec::new();
    for line in out.lines() {
        if let Some(rest) = line.strip_prefix("There are ") {
            count = rest.split_whitespace().next().unwrap().parse().unwrap();
        }
        let t = line.trim_start();
        if !t.starts_with('[') || t.starts_with("[Nr]") {
            continue;
        }
        let Some(close) = t.find(']') else { continue };
        let Ok(index) = t[1..close].trim().parse::<usize>() else {
            continue;
        };
        // name, type (may contain spaces), address, off, size, es, [flags],
        // lk, inf, al. The address is the first fixed-width hex token.
        let tokens: Vec<&str> = t[close + 1..].split_whitespace().collect();
        let is_addr =
            |s: &&str| (s.len() == 8 || s.len() == 16) && s.chars().all(|c| c.is_ascii_hexdigit());
        let addr = tokens
            .iter()
            .skip(1)
            .position(is_addr)
            .map(|p| p + 1)
            .unwrap_or_else(|| panic!("unexpected section line {line:?}"));
        let name = if index == 0 {
            String::new()
        } else {
            tokens[0].to_owned()
        };
        let n = tokens.len();
        sections.push(RSec {
            name,
            offset: hex(tokens[addr + 1]),
            size: hex(tokens[addr + 2]),
            entsize: hex(tokens[addr + 3]),
            link: tokens[n - 3].parse().unwrap(),
            info: tokens[n - 2].parse().unwrap(),
            align: tokens[n - 1].parse().unwrap(),
        });
    }
    (count, sections)
}

#[derive(Debug)]
struct RSym {
    value: u64,
    size: u64,
    kind: String,
    bind: String,
    vis: String,
    ndx: String,
    name: String,
}

fn parse_readelf_symbols(out: &str) -> BTreeMap<String, (usize, Vec<RSym>)> {
    let mut tables = BTreeMap::new();
    let mut current: Option<String> = None;
    for line in out.lines() {
        if let Some(rest) = line.strip_prefix("Symbol table '") {
            let (name, tail) = rest.split_once('\'').unwrap();
            let count = tail
                .split_whitespace()
                .find_map(|t| t.parse::<usize>().ok())
                .unwrap();
            tables.insert(name.to_owned(), (count, Vec::new()));
            current = Some(name.to_owned());
            continue;
        }
        let Some(table) = current.as_ref() else {
            continue;
        };
        let t = line.trim_start();
        let Some((num, rest)) = t.split_once(':') else {
            continue;
        };
        if num.parse::<usize>().is_err() {
            continue;
        }
        let mut rest = rest.trim_start();
        let mut fields = Vec::new();
        for _ in 0..5 {
            let end = rest.find(' ').unwrap_or(rest.len());
            fields.push(&rest[..end]);
            rest = rest[end..].trim_start();
        }
        if rest.starts_with('[') {
            let end = rest.find(']').unwrap();
            rest = rest[end + 1..].trim_start();
        }
        let (ndx, name) = rest.split_once(' ').unwrap_or((rest, ""));
        let size = if let Some(h) = fields[1].strip_prefix("0x") {
            hex(h)
        } else {
            fields[1].parse().unwrap()
        };
        tables.get_mut(table).unwrap().1.push(RSym {
            value: hex(fields[0]),
            size,
            kind: fields[2].to_owned(),
            bind: fields[3].to_owned(),
            vis: fields[4].to_owned(),
            ndx: ndx.to_owned(),
            name: name.to_owned(),
        });
    }
    tables
}

#[derive(Debug)]
struct RRel {
    offset: u64,
    info: u64,
    type_name: String,
    rest: String,
}

enum RRelSec {
    Rel(Vec<RRel>),
    Relr(usize),
}

fn parse_readelf_relocs(out: &str) -> Vec<(String, usize, RRelSec)> {
    let mut sections = Vec::new();
    for line in out.lines() {
        if let Some(rest) = line.strip_prefix("Relocation section '") {
            let (name, tail) = rest.split_once('\'').unwrap();
            let words: Vec<&str> = tail.split_whitespace().collect();
            let pos = words.iter().position(|w| *w == "contains").unwrap();
            let count: usize = words[pos + 1].parse().unwrap();
            let kind = if let Some(p) = words.iter().position(|w| *w == "relocate") {
                RRelSec::Relr(words[p + 1].parse().unwrap())
            } else {
                RRelSec::Rel(Vec::new())
            };
            sections.push((name.to_owned(), count, kind));
            continue;
        }
        let Some((_, _, RRelSec::Rel(entries))) = sections.last_mut() else {
            continue;
        };
        let mut parts = line.split_whitespace();
        let (Some(off), Some(info), Some(ty)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        if !off.chars().all(|c| c.is_ascii_hexdigit())
            || !info.chars().all(|c| c.is_ascii_hexdigit())
        {
            continue;
        }
        entries.push(RRel {
            offset: hex(off),
            info: hex(info),
            type_name: ty.to_owned(),
            rest: parts.collect::<Vec<_>>().join(" "),
        });
    }
    sections
}

fn readelf_type(kind: u8) -> Option<&'static str> {
    Some(match kind {
        STT_NOTYPE => "NOTYPE",
        STT_OBJECT => "OBJECT",
        STT_FUNC => "FUNC",
        STT_SECTION => "SECTION",
        STT_FILE => "FILE",
        STT_COMMON => "COMMON",
        STT_TLS => "TLS",
        STT_GNU_IFUNC => "IFUNC",
        _ => return None,
    })
}

fn readelf_bind(bind: u8) -> Option<&'static str> {
    Some(match bind {
        STB_LOCAL => "LOCAL",
        STB_GLOBAL => "GLOBAL",
        STB_WEAK => "WEAK",
        STB_GNU_UNIQUE => "UNIQUE",
        _ => return None,
    })
}

fn readelf_vis(vis: u8) -> &'static str {
    match vis {
        STV_DEFAULT => "DEFAULT",
        STV_INTERNAL => "INTERNAL",
        STV_HIDDEN => "HIDDEN",
        STV_PROTECTED => "PROTECTED",
        _ => unreachable!(),
    }
}

/// Compares qld's inventory of `path` with `readelf -W -S -s -r`.
fn compare_with_readelf(path: &Path, inv: &Inventory) {
    let Some(out) = readelf(&["-W", "-S"], path) else {
        eprintln!("skipping readelf comparison for {}", path.display());
        return;
    };
    let mut report = String::new();
    let (count, rsecs) = parse_readelf_sections(&out);
    if count != 0 || !inv.sections.is_empty() {
        assert_eq!(
            count,
            inv.sections.len(),
            "{}: section count",
            path.display()
        );
        assert_eq!(
            rsecs.len(),
            inv.sections.len(),
            "{}: section lines",
            path.display()
        );
    }
    for (i, (r, s)) in rsecs.iter().zip(&inv.sections).enumerate() {
        let ours = (
            &s.name, s.offset, s.size, s.entsize, s.link, s.info, s.align,
        );
        let theirs = (
            &r.name, r.offset, r.size, r.entsize, r.link, r.info, r.align,
        );
        if ours != theirs {
            writeln!(report, "section {i}: qld {ours:?} readelf {theirs:?}").unwrap();
        }
    }

    let out = readelf(&["-W", "-s"], path).unwrap();
    let rtables = parse_readelf_symbols(&out);
    let section_names: Vec<&str> = inv.sections.iter().map(|s| s.name.as_str()).collect();
    for (table, syms) in &inv.symtabs {
        let Some((rcount, rsyms)) = rtables.get(table) else {
            panic!("{}: readelf has no symbol table {table}", path.display());
        };
        assert_eq!(*rcount, syms.len(), "{}: {table} count", path.display());
        assert_eq!(rsyms.len(), syms.len(), "{}: {table} lines", path.display());
        for (i, (r, s)) in rsyms.iter().zip(syms).enumerate() {
            let ndx = match s.section {
                SectionIndex::Undefined => "UND".to_owned(),
                SectionIndex::Absolute => "ABS".to_owned(),
                SectionIndex::Common => "COM".to_owned(),
                // readelf flags indices past the section table (for example
                // symbols of sections removed by strip) as BAD[n].
                SectionIndex::Section(n) if n as usize >= inv.sections.len() => {
                    format!("BAD[{n:#x}]")
                }
                SectionIndex::Section(n) => n.to_string(),
                SectionIndex::Reserved(_) => r.ndx.clone(),
            };
            let mut ok = r.value == s.value
                && r.size == s.size
                && readelf_type(s.kind).is_none_or(|t| t == r.kind)
                && readelf_bind(s.bind).is_none_or(|b| b == r.bind)
                && readelf_vis(s.vis) == r.vis
                && ndx == r.ndx;
            let full = format!("{}{}", s.name, s.version);
            let name_ok = r.name == full
                // readelf names section symbols after their section.
                || (s.name.is_empty()
                    && s.kind == STT_SECTION
                    && s.section.section().and_then(|n| section_names.get(n as usize))
                        == Some(&r.name.as_str()))
                // ... and omits the version of version-definition symbols.
                || (!s.version.is_empty() && s.name == s.version_name && r.name == s.name);
            ok &= name_ok;
            if !ok {
                writeln!(report, "{table}[{i}]: qld {s:?} readelf {r:?}").unwrap();
            }
        }
    }

    let out = readelf(&["-W", "-r"], path).unwrap();
    let rrels = parse_readelf_relocs(&out);
    let ours_count = inv.relocs.len() + inv.relr.len();
    assert_eq!(
        rrels.len(),
        ours_count,
        "{}: relocation sections",
        path.display()
    );
    for (name, count, kind) in &rrels {
        match kind {
            RRelSec::Relr(places) => {
                let ours = inv.relr.iter().find(|(n, _)| n == name).unwrap();
                assert_eq!(ours.1, *places, "{}: {name} RELR places", path.display());
            }
            RRelSec::Rel(entries) => {
                let ours = inv.relocs.iter().find(|r| &r.name == name).unwrap();
                assert_eq!(ours.entries.len(), *count, "{}: {name}", path.display());
                assert_eq!(entries.len(), *count, "{}: {name} lines", path.display());
                for (i, (r, o)) in entries.iter().zip(&ours.entries).enumerate() {
                    let info = if inv.word_size == 8 {
                        (u64::from(o.symbol) << 32) | u64::from(o.r_type)
                    } else {
                        (u64::from(o.symbol) << 8) | u64::from(o.r_type)
                    };
                    let mut ok = r.offset == o.offset && r.info == info;
                    if let Some(n) = consts::reloc_name(inv.machine, o.r_type) {
                        ok &= r.type_name == n;
                    }
                    if ours.rela {
                        let addend = if let Some(a) = r.rest.rsplit_once(" + ") {
                            hex(a.1) as i64
                        } else if let Some(a) = r.rest.rsplit_once(" - ") {
                            -(hex(a.1) as i64)
                        } else if r.rest.is_empty() {
                            0
                        } else {
                            hex(r.rest.split_whitespace().last().unwrap()) as i64
                        };
                        ok &= addend == o.addend;
                    }
                    if !ok {
                        writeln!(report, "{name}[{i}]: qld {o:?} readelf {r:?}").unwrap();
                    }
                }
            }
        }
    }

    assert!(
        report.is_empty(),
        "{}: mismatches:\n{report}",
        path.display()
    );
}

fn compare_frames_with_readelf(path: &Path, inv: &Inventory) {
    let Some(out) = readelf(&["-W", "--debug-dump=frames"], path) else {
        return;
    };
    let mut records = Vec::new();
    let mut in_eh_frame = false;
    for line in out.lines() {
        if let Some(rest) = line.strip_prefix("Contents of the ") {
            in_eh_frame = rest.starts_with(".eh_frame ");
            continue;
        }
        let parts: Vec<&str> = line.split_whitespace().collect();
        if in_eh_frame && parts.len() >= 4 && (parts[3] == "CIE" || parts[3] == "FDE") {
            let offset = hex(parts[0]) as usize;
            let cie = parts
                .iter()
                .find_map(|p| p.strip_prefix("cie="))
                .map(|c| hex(c) as usize);
            records.push((offset, cie));
        }
    }
    let ours: Vec<(usize, Option<usize>)> = inv
        .eh_frames
        .iter()
        .flatten()
        .map(|r| (r.offset, r.cie))
        .collect();
    assert_eq!(ours, records, "{}: .eh_frame records", path.display());
}

fn compare_groups_with_readelf(path: &Path, inv: &Inventory) {
    let Some(out) = readelf(&["-W", "-g"], path) else {
        return;
    };
    let mut groups: Vec<(String, Vec<u32>)> = Vec::new();
    for line in out.lines() {
        let t = line.trim();
        if t.starts_with("COMDAT group section") || t.starts_with("group section") {
            let sig = t.split('[').nth(2).unwrap().split(']').next().unwrap();
            groups.push((sig.to_owned(), Vec::new()));
        } else if let Some(rest) = t.strip_prefix('[')
            && let Some((num, _)) = rest.split_once(']')
            && let Ok(n) = num.trim().parse::<u32>()
            && let Some(g) = groups.last_mut()
        {
            g.1.push(n);
        }
    }
    let ours: Vec<(String, Vec<u32>)> = inv
        .groups
        .iter()
        .map(|g| (g.signature.clone(), g.members.clone()))
        .collect();
    assert_eq!(ours, groups, "{}: groups", path.display());
}

fn compare_dynamic_with_readelf(path: &Path, inv: &Inventory) {
    let Some(out) = readelf(&["-W", "-d"], path) else {
        return;
    };
    let mut needed = Vec::new();
    let mut soname = None;
    for line in out.lines() {
        let value = line
            .split_once('[')
            .and_then(|(_, v)| v.rsplit_once(']'))
            .map(|v| v.0);
        if line.contains("(NEEDED)") {
            needed.push(value.unwrap().to_owned());
        } else if line.contains("(SONAME)") {
            soname = value.map(str::to_owned);
        }
    }
    assert_eq!(inv.needed, needed, "{}: DT_NEEDED", path.display());
    assert_eq!(inv.soname, soname, "{}: DT_SONAME", path.display());
}

fn full_readelf_check(path: &Path) -> Inventory {
    let inv = parse_ok(path);
    compare_with_readelf(path, &inv);
    if inv.e_type == ET_REL {
        compare_frames_with_readelf(path, &inv);
        compare_groups_with_readelf(path, &inv);
    }
    if inv.e_type == ET_DYN {
        compare_dynamic_with_readelf(path, &inv);
    }
    inv
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

const PREBUILT: &[&str] = &[
    "basic-x86_64.o",
    "comdat-x86_64.o",
    "libqldtest-x86_64.so",
    "basic-i386.o",
    "basic-s390x.o",
    "basic-ppc.o",
];

fn fixture_source(name: &str) -> String {
    data_dir().join(name).to_str().unwrap().to_owned()
}

fn build_basic() -> Option<PathBuf> {
    let src = fixture_source("basic.c");
    build(
        "gcc",
        &[
            "-c",
            "-O1",
            "-fcommon",
            "-ffunction-sections",
            "-fdata-sections",
            "-g",
            "-gz=zlib",
            "-fcf-protection=full",
            &src,
        ],
        &scratch_dir().join("basic.o"),
    )
}

fn find_symbol<'a>(inv: &'a Inventory, table: &str, name: &str) -> &'a Sym {
    inv.symtabs[table]
        .iter()
        .find(|s| s.name == name)
        .unwrap_or_else(|| panic!("no symbol {name} in {table}"))
}

fn check_basic(inv: &Inventory) {
    assert_eq!(inv.e_type, ET_REL);
    let common = find_symbol(inv, ".symtab", "common_var");
    assert!(common.section == SectionIndex::Common || common.kind == STT_COMMON);
    assert_eq!(find_symbol(inv, ".symtab", "tls_var").kind, STT_TLS);
    assert_eq!(find_symbol(inv, ".symtab", "tls_bss").bind, STB_LOCAL);
    assert_eq!(find_symbol(inv, ".symtab", "weak_fn").bind, STB_WEAK);
    assert_eq!(find_symbol(inv, ".symtab", "hidden_fn").vis, STV_HIDDEN);
    assert_eq!(
        find_symbol(inv, ".symtab", "protected_fn").vis,
        STV_PROTECTED
    );
    assert_eq!(
        find_symbol(inv, ".symtab", "undefined_fn").section,
        SectionIndex::Undefined
    );
    // -ffunction-sections: every function has its own section.
    for f in ["weak_fn", "hidden_fn", "protected_fn", "use_all"] {
        let sym = find_symbol(inv, ".symtab", f);
        let index = sym.section.section().unwrap() as usize;
        // (64-bit PowerPC ELFv1 function symbols point into .opd.)
        let name = &inv.sections[index].name;
        assert!(
            *name == format!(".text.{f}") || name == ".opd",
            "{f}: {name}"
        );
    }
    // .eh_frame (some targets only emit .debug_frame): every FDE has its
    // function relocation.
    if inv.sections.iter().any(|s| s.name == ".eh_frame") {
        assert_eq!(inv.eh_frames.len(), 1);
        let fdes: Vec<_> = inv.eh_frames[0]
            .iter()
            .filter(|r| r.cie.is_some())
            .collect();
        assert!(!fdes.is_empty());
        assert!(fdes.iter().all(|f| f.pc_begin_reloc.is_some()));
    }
    assert!(!inv.lto);
}

fn check_basic_x86(inv: &Inventory) {
    check_basic(inv);
    assert!(inv.properties.x86_ibt(), "{:?}", inv.properties);
    assert!(inv.properties.x86_shstk());
    assert!(
        inv.compressed
            .iter()
            .any(|(n, t, size)| n == ".debug_info" && *t == ELFCOMPRESS_ZLIB && *size > 0),
        "{:?}",
        inv.compressed
    );
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn prebuilt_fixtures_parse() {
    for name in PREBUILT {
        let path = data_dir().join(name);
        let inv = parse_ok(&path);
        match *name {
            "basic-x86_64.o" => check_basic_x86(&inv),
            "basic-i386.o" | "basic-s390x.o" | "basic-ppc.o" => check_basic(&inv),
            "comdat-x86_64.o" => {
                assert!(
                    inv.groups
                        .iter()
                        .any(|g| g.comdat && g.signature == "_ZNK7Derived1fEv")
                );
            }
            "libqldtest-x86_64.so" => check_shared(&inv),
            _ => unreachable!(),
        }
    }
}

#[test]
fn prebuilt_fixtures_match_readelf() {
    if !tool_works("readelf") {
        eprintln!("skipping: readelf unavailable");
        return;
    }
    for name in PREBUILT {
        full_readelf_check(&data_dir().join(name));
    }
}

#[test]
fn prebuilt_formats() {
    let kinds = [
        ("basic-x86_64.o", ElfKind::Elf64Le, consts::EM_X86_64),
        ("basic-i386.o", ElfKind::Elf32Le, consts::EM_386),
        ("basic-s390x.o", ElfKind::Elf64Be, consts::EM_S390),
        ("basic-ppc.o", ElfKind::Elf32Be, consts::EM_PPC),
    ];
    for (name, kind, machine) in kinds {
        let data = std::fs::read(data_dir().join(name)).unwrap();
        assert_eq!(ElfKind::identify(&data), Some(kind), "{name}");
        assert_eq!(parse_ok(&data_dir().join(name)).machine, machine, "{name}");
    }
    // Wrong format parameter is an error, not a panic.
    let data = std::fs::read(data_dir().join("basic-x86_64.o")).unwrap();
    let source = Source::new(Path::new("basic-x86_64.o"));
    assert!(ObjectFile::<Elf64Be>::parse(&data, source).is_err());
    assert!(ObjectFile::<Elf32Le>::parse(&data, source).is_err());
    assert!(SharedObject::<Elf64Le>::parse(&data, source).is_err());
    let err = ObjectFile::<Elf64Le>::parse(&data[..40], source).unwrap_err();
    assert!(err.to_string().contains("basic-x86_64.o"), "{err}");
}

#[test]
fn basic_object_matches_readelf() {
    let Some(path) = build_basic() else { return };
    let inv = full_readelf_check(&path);
    check_basic_x86(&inv);
}

#[test]
fn comdat_object_matches_readelf() {
    let src = fixture_source("comdat.cpp");
    let Some(path) = build("g++", &["-c", "-O1", &src], &scratch_dir().join("comdat.o")) else {
        return;
    };
    let inv = full_readelf_check(&path);
    let comdats: Vec<_> = inv.groups.iter().filter(|g| g.comdat).collect();
    assert!(comdats.len() >= 2, "{:?}", inv.groups);
    assert!(comdats.iter().any(|g| g.signature == "_ZNK7Derived1fEv"));
    // Two CIEs (plain and with personality), FDEs pointing at them.
    let frames = &inv.eh_frames[0];
    assert!(frames.iter().filter(|r| r.cie.is_none()).count() >= 2);
}

#[test]
fn lto_object_is_detected() {
    let src = fixture_source("basic.c");
    let Some(path) = build(
        "gcc",
        &["-c", "-O1", "-flto", "-fcommon", &src],
        &scratch_dir().join("basic-lto.o"),
    ) else {
        return;
    };
    let inv = parse_ok(&path);
    assert!(inv.lto);
    compare_with_readelf(&path, &inv);
}

#[test]
fn zstd_compressed_debug() {
    let src = fixture_source("basic.c");
    let Some(path) = build(
        "gcc",
        &["-c", "-g", "-gz=zstd", &src],
        &scratch_dir().join("basic-zstd.o"),
    ) else {
        return;
    };
    let inv = parse_ok(&path);
    assert!(
        inv.compressed
            .iter()
            .any(|(_, t, _)| *t == consts::ELFCOMPRESS_ZSTD),
        "{:?}",
        inv.compressed
    );
}

fn check_shared(inv: &Inventory) {
    assert_eq!(inv.e_type, ET_DYN);
    assert_eq!(inv.soname.as_deref(), Some("libqldtest.so.1"));
    assert!(inv.needed.iter().any(|n| n.starts_with("libc.so")));
    assert!(inv.gnu_hash);
    let dynsym = &inv.symtabs[".dynsym"];
    let find = |name: &str, version: &str| {
        dynsym
            .iter()
            .find(|s| s.name == name && s.version == version)
            .unwrap_or_else(|| panic!("no {name}{version} in {dynsym:#?}"))
    };
    find("v1_fn", "@@VERS_1");
    find("v2_fn", "@@VERS_2");
    find("versioned", "@VERS_1");
    find("versioned", "@@VERS_2");
    assert_eq!(find("shared_tls", "@@VERS_2").kind, STT_TLS);
    assert!(
        dynsym
            .iter()
            .any(|s| s.name == "puts" && s.version.starts_with("@GLIBC_"))
    );
    assert!(inv.relr.iter().any(|(_, n)| *n > 0), "{:?}", inv.relr);
}

#[test]
fn shared_object_matches_readelf() {
    let src = fixture_source("shlib.c");
    let map = format!("-Wl,--version-script={}", fixture_source("shlib.map"));
    let Some(path) = build(
        "gcc",
        &[
            "-shared",
            "-fPIC",
            "-O1",
            &map,
            "-Wl,-soname,libqldtest.so.1",
            "-Wl,-z,pack-relative-relocs",
            &src,
        ],
        &scratch_dir().join("libqldtest.so"),
    ) else {
        return;
    };
    let inv = full_readelf_check(&path);
    check_shared(&inv);
}

#[test]
fn extended_section_numbering() {
    const SECTIONS: usize = 70_000;
    let dir = scratch_dir();
    let asm = dir.join("many.s");
    let mut text = String::new();
    for i in 0..SECTIONS {
        writeln!(
            text,
            ".section .t{i},\"ax\",@progbits\n.globl s{i}\ns{i}: .byte {}",
            i % 256
        )
        .unwrap();
    }
    std::fs::write(&asm, text).unwrap();
    let Some(path) = build("gcc", &["-c", asm.to_str().unwrap()], &dir.join("many.o")) else {
        return;
    };
    let data = std::fs::read(&path).unwrap();
    let obj = ObjectFile::<Elf64Le>::parse(&data, Source::new(&path)).unwrap();
    assert_eq!(obj.elf().header().e_shnum, 0);
    assert!(obj.section_count() > SECTIONS);
    let last = format!("s{}", SECTIONS - 1);
    let sym = obj
        .symbols()
        .iter()
        .map(Result::unwrap)
        .find(|s| s.name == last.as_bytes())
        .unwrap();
    let index = sym.section.section().unwrap();
    assert!(index > 0xff00);
    let hdr = obj.section_header(index).unwrap();
    assert_eq!(
        obj.section_name(&hdr).unwrap(),
        format!(".t{}", SECTIONS - 1).as_bytes()
    );
    full_readelf_check(&path);
}

#[test]
fn cross_targets_match_readelf() {
    if !tool_works("clang") {
        eprintln!("skipping: clang unavailable");
        return;
    }
    let src = fixture_source("basic.c");
    let targets = [
        ("i686-linux-gnu", ElfKind::Elf32Le),
        ("x86_64-linux-gnux32", ElfKind::Elf32Le),
        ("aarch64-linux-gnu", ElfKind::Elf64Le),
        ("riscv64-linux-gnu", ElfKind::Elf64Le),
        ("riscv32-unknown-elf", ElfKind::Elf32Le),
        ("armv7a-linux-gnueabihf", ElfKind::Elf32Le),
        ("s390x-linux-gnu", ElfKind::Elf64Be),
        ("powerpc-linux-gnu", ElfKind::Elf32Be),
        ("powerpc64-linux-gnu", ElfKind::Elf64Be),
    ];
    for (target, kind) in targets {
        let out = scratch_dir().join(format!("basic-{target}.o"));
        let target_arg = format!("--target={target}");
        let Some(path) = build(
            "clang",
            &[
                &target_arg,
                "-c",
                "-O1",
                "-fcommon",
                "-ffunction-sections",
                "-g",
                &src,
            ],
            &out,
        ) else {
            continue;
        };
        let data = std::fs::read(&path).unwrap();
        assert_eq!(ElfKind::identify(&data), Some(kind), "{target}");
        let inv = full_readelf_check(&path);
        check_basic(&inv);
    }
}

// ---------------------------------------------------------------------------
// Corruption
// ---------------------------------------------------------------------------

/// xorshift64*: deterministic, dependency-free.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

fn corrupt_and_parse(name: &str, data: &[u8], rounds: usize, seed: u64) {
    let path = PathBuf::from(name);
    let mut rng = Rng(seed | 1);
    // Truncations.
    for _ in 0..rounds / 4 {
        let len = rng.below(data.len() + 1);
        let _ = inventory(&data[..len], &path);
    }
    // Byte and field corruption, biased towards the headers and tables.
    let mut buf = data.to_vec();
    for _ in 0..rounds {
        buf.copy_from_slice(data);
        for _ in 0..1 + rng.below(8) {
            let at = match rng.below(4) {
                0 => rng.below(64.min(buf.len())),
                1 => buf.len() - 1 - rng.below((buf.len() / 4).max(1)),
                _ => rng.below(buf.len()),
            };
            buf[at] = match rng.below(4) {
                0 => 0,
                1 => 0xff,
                2 => buf[at] ^ (1 << rng.below(8)),
                _ => rng.next() as u8,
            };
        }
        let _ = inventory(&buf, &path);
    }
}

#[test]
fn corrupted_prebuilt_fixtures_do_not_panic() {
    for (i, name) in PREBUILT.iter().enumerate() {
        let data = std::fs::read(data_dir().join(name)).unwrap();
        corrupt_and_parse(name, &data, 3000, 0x9e37_79b9_7f4a_7c15 ^ i as u64);
    }
}

#[test]
fn corrupted_compiled_fixtures_do_not_panic() {
    let Some(path) = build_basic() else { return };
    let data = std::fs::read(&path).unwrap();
    corrupt_and_parse("basic.o", &data, 2000, 42);
}

// ---------------------------------------------------------------------------
// System sweep
// ---------------------------------------------------------------------------

/// Parses every shared object (and object) in `/usr/lib64` and `/usr/lib`.
///
/// Run with `cargo test --test elf_read -- --ignored`. Set
/// `QLD_SWEEP_READELF=1` to also compare every file against `readelf`.
#[test]
#[ignore = "slow: sweeps the system library directories"]
fn sweep_system_libraries() {
    use rayon::prelude::*;
    let with_readelf = std::env::var_os("QLD_SWEEP_READELF").is_some() && tool_works("readelf");
    let mut files = std::collections::BTreeSet::new();
    let mut dirs: Vec<PathBuf> = match std::env::var_os("QLD_SWEEP_DIRS") {
        Some(list) => std::env::split_paths(&list).collect(),
        None => vec!["/usr/lib64".into(), "/usr/lib".into()],
    };
    let recursive = std::env::var_os("QLD_SWEEP_RECURSIVE").is_some();
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if recursive && entry.file_type().is_ok_and(|t| t.is_dir()) {
                dirs.push(path);
                continue;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !(name.contains(".so") || name.ends_with(".o")) || name.ends_with(".debug") {
                continue;
            }
            if let Ok(real) = std::fs::canonicalize(&path)
                && real.is_file()
            {
                files.insert(real);
            }
        }
    }
    let files: Vec<PathBuf> = files.into_iter().collect();
    let parsed = std::sync::atomic::AtomicUsize::new(0);
    let failures: Vec<String> = files
        .par_iter()
        .filter_map(|path| {
            let data = std::fs::read(path).ok()?;
            ElfKind::identify(&data)?;
            parsed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let result = std::panic::catch_unwind(|| {
                let (inv, errors) = inventory(&data, path);
                if !errors.is_empty() {
                    return Some(format!("{}: {errors:?}", path.display()));
                }
                if with_readelf && let Some(inv) = inv {
                    compare_with_readelf(path, &inv);
                }
                None
            });
            match result {
                Ok(r) => r,
                Err(payload) => {
                    let message = payload
                        .downcast_ref::<String>()
                        .map(String::as_str)
                        .or_else(|| payload.downcast_ref::<&str>().copied())
                        .unwrap_or("panicked");
                    Some(format!("{}: {message}", path.display()))
                }
            }
        })
        .collect();
    eprintln!(
        "swept {} files, parsed {} ELF files{}",
        files.len(),
        parsed.load(std::sync::atomic::Ordering::Relaxed),
        if with_readelf {
            ", compared with readelf"
        } else {
            ""
        }
    );
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
