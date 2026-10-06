//! Debug output tests (workstream W29): `--gdb-index`, `--debug-names` and
//! `--separate-debug-file`.
//!
//! The inputs are small freestanding C and C++ programs compiled with the
//! host compilers, linked without libc. The index is decoded here and
//! checked for its units, address ranges and names. When lld is installed
//! (`QLD_TEST_LLD`), the same objects are linked with `ld.lld --gdb-index`
//! and the two indexes must agree, with addresses compared by the symbol
//! they fall in. When GDB is installed, it must load the index.
//!
//! Tests print `SKIPPED:` on a host whose toolchain does not build
//! x86-64 ELF objects, and when a tool is missing; a missing compiler
//! fails instead when `QLD_REQUIRE_TOOLS` is set.

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;

use common::tools::{driver_is_clang, find_program, skip, tools, tools_required};
use qld::elf::read::{Elf64Be, Elf64Le, ElfFile, ElfFormat, SectionIndex, Source};

fn scratch(name: &str) -> PathBuf {
    common::scratch::scratch_dir("debug-index-tests", name)
}

fn run(dir: &Path, program: &Path, args: &[&str]) -> Output {
    Command::new(program)
        .args(args)
        .current_dir(dir)
        .env("LC_ALL", "C")
        .output()
        .unwrap_or_else(|e| panic!("cannot run {}: {e}", program.display()))
}

fn run_ok(dir: &Path, program: &Path, args: &[&str]) -> Output {
    let output = run(dir, program, args);
    assert!(
        output.status.success(),
        "`{} {}` failed:\n{}{}",
        program.display(),
        args.join(" "),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn qld(dir: &Path, args: &[&str]) -> Output {
    run_ok(dir, Path::new(env!("CARGO_BIN_EXE_qld")), args)
}

/// The host C and C++ compilers (x86-64), or `None` after printing why.
/// Whether the host builds and runs the x86-64 ELF objects these tests
/// assemble, as the other ELF-only suites ask. A compiler that targets
/// something else (the MinGW `cc` of a Windows runner) skips; a missing
/// compiler is the caller's to report, so that `QLD_REQUIRE_TOOLS` still
/// fails on Linux.
fn elf_host() -> bool {
    if !cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        skip("host is not x86-64 Linux");
        return false;
    }
    match tools().host.as_ref() {
        Some(host) if !(host.arch == "x86_64" && host.is_linux()) => {
            skip(format!(
                "the C compiler builds {host} objects, not x86-64 ELF"
            ));
            false
        }
        _ => true,
    }
}

fn compilers() -> Option<(PathBuf, PathBuf)> {
    if !elf_host() {
        return None;
    }
    let t = tools();
    match (&t.cc, &t.cxx) {
        (Some(cc), Some(cxx)) => Some((cc.clone(), cxx.clone())),
        _ => {
            assert!(!tools_required(), "no x86-64 C and C++ compilers");
            skip("no x86-64 C and C++ compilers");
            None
        }
    }
}

const C_SOURCE: &str = r"
struct point { int x, y; };
typedef struct point point_t;
enum color { RED, GREEN, BLUE };
static int counter;
int global_var = 3;
__attribute__((noinline)) static int helper(int v) { return v * 2 + counter; }
int add(int a, int b) { return helper(a) + b; }
point_t make_point(int x, int y) { point_t p = { x, y }; return p; }
enum color favorite(void) { return GREEN; }
";

const CXX_SOURCE: &str = r#"
namespace ns {
  struct Widget { int v; int get() const; static int count; };
  int Widget::get() const { return v; }
  int Widget::count = 0;
  template <typename T> T twice(T t) { return t + t; }
  namespace inner { int deep(int x) { return x - 1; } }
  enum class Mode { On, Off };
}
namespace { int anon_fn(int x) { static int calls; return x * 3 + ++calls; } }
extern "C" int add(int, int);
extern "C" int global_var;
extern ns::Mode mode;
ns::Mode mode = ns::Mode::On;
extern "C" void _start() {
  ns::Widget w{global_var};
  volatile int sink = w.get() + ns::twice(2) + (int)ns::twice(1.5) + ns::inner::deep(3)
    + anon_fn(4) + add(1, 2) + (int)mode;
  (void)sink;
  for (;;) {}
}
"#;

/// Compiles the two sources with `flags` into `a.o` and `b.o`.
fn compile(dir: &Path, cc: &Path, cxx: &Path, flags: &[&str]) {
    fs::write(dir.join("a.c"), C_SOURCE).unwrap();
    fs::write(dir.join("b.cc"), CXX_SOURCE).unwrap();
    let mut args: Vec<&str> = vec!["-c", "a.c", "-o", "a.o"];
    args.extend_from_slice(flags);
    run_ok(dir, cc, &args);
    let mut args: Vec<&str> = vec![
        "-c",
        "b.cc",
        "-o",
        "b.o",
        "-fno-exceptions",
        "-fno-rtti",
        "-fno-asynchronous-unwind-tables",
    ];
    args.extend_from_slice(flags);
    run_ok(dir, cxx, &args);
}

/// A decoded `.gdb_index`: version, units (offset, size), address areas
/// (low, high, unit), and names with their CU vectors.
#[derive(Debug, Default)]
struct GdbIndex {
    version: u32,
    units: Vec<(u64, u64)>,
    areas: Vec<(u64, u64, u32)>,
    names: BTreeMap<String, Vec<u32>>,
}

fn u32_at(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap())
}

fn u64_at(data: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(data[offset..offset + 8].try_into().unwrap())
}

fn section<'a>(elf: &ElfFile<'a, Elf64Le>, name: &[u8]) -> Option<&'a [u8]> {
    let (_, header) = elf.section_by_name(name)?;
    elf.section_data(&header).ok()
}

/// Whether the ELF file at `path` is big-endian (`e_ident[EI_DATA]`).
fn is_big_endian(path: &Path) -> bool {
    fs::read(path).unwrap().get(5) == Some(&2)
}

fn section_in<F: ElfFormat>(data: &[u8], path: &Path, name: &[u8]) -> Option<Vec<u8>> {
    let elf = ElfFile::<F>::parse(data, Source::new(path)).unwrap();
    let (_, header) = elf.section_by_name(name)?;
    Some(elf.section_data(&header).ok()?.to_vec())
}

/// The contents of output section `name`, from either byte order.
fn section_bytes(path: &Path, name: &[u8]) -> Option<Vec<u8>> {
    let data = fs::read(path).unwrap();
    if is_big_endian(path) {
        section_in::<Elf64Be>(&data, path, name)
    } else {
        section_in::<Elf64Le>(&data, path, name)
    }
}

/// Whether the output has a section named `name`.
fn has_section(path: &Path, name: &[u8]) -> bool {
    section_bytes(path, name).is_some()
}

/// `.gdb_index` is little-endian whatever the target's byte order, as GDB
/// defines it and lld writes it.
fn read_gdb_index(path: &Path) -> Option<GdbIndex> {
    let d = &section_bytes(path, b".gdb_index")?[..];
    let word = |i: usize| u32_at(d, i * 4) as usize;
    let (cu_list, types, areas, symtab, pool) = (word(1), word(2), word(3), word(4), word(5));
    let mut index = GdbIndex {
        version: u32_at(d, 0),
        ..GdbIndex::default()
    };
    assert_eq!(types, areas, "qld writes no type unit list");
    for at in (cu_list..types).step_by(16) {
        index.units.push((u64_at(d, at), u64_at(d, at + 8)));
    }
    for at in (areas..symtab).step_by(20) {
        index
            .areas
            .push((u64_at(d, at), u64_at(d, at + 8), u32_at(d, at + 16)));
    }
    for slot in (symtab..pool).step_by(8) {
        let (name, vector) = (u32_at(d, slot) as usize, u32_at(d, slot + 4) as usize);
        if name == 0 && vector == 0 {
            continue;
        }
        let bytes = &d[pool + name..];
        let end = bytes.iter().position(|&b| b == 0).unwrap();
        let count = u32_at(d, pool + vector) as usize;
        let mut cus: Vec<u32> = (0..count)
            .map(|i| u32_at(d, pool + vector + 4 + i * 4))
            .collect();
        cus.sort_unstable();
        index
            .names
            .insert(String::from_utf8_lossy(&bytes[..end]).into_owned(), cus);
    }
    Some(index)
}

fn functions_in<F: ElfFormat>(data: &[u8], path: &Path) -> Vec<(u64, String)> {
    let elf = ElfFile::<F>::parse(data, Source::new(path)).unwrap();
    let (index, _) = elf.section_by_name(b".symtab").unwrap();
    let mut out: Vec<(u64, String)> = elf
        .symbol_table(index)
        .unwrap()
        .iter()
        .filter_map(Result::ok)
        .filter(|s| s.kind() == 2 && matches!(s.section, SectionIndex::Section(_)))
        .map(|s| (s.value, String::from_utf8_lossy(s.name).into_owned()))
        .collect();
    out.sort();
    out
}

/// The function symbols of an output, sorted by address.
fn functions(path: &Path) -> Vec<(u64, String)> {
    let data = fs::read(path).unwrap();
    if is_big_endian(path) {
        functions_in::<Elf64Be>(&data, path)
    } else {
        functions_in::<Elf64Le>(&data, path)
    }
}

/// The index with addresses replaced by `function+offset`, for comparing
/// two linkers' outputs.
fn canonical(path: &Path) -> (Vec<String>, BTreeMap<String, Vec<u32>>, usize) {
    let index = read_gdb_index(path).expect("no .gdb_index");
    let functions = functions(path);
    let name = |address: u64| {
        let at = functions.partition_point(|(a, _)| *a <= address);
        match at.checked_sub(1).and_then(|i| functions.get(i)) {
            Some((start, name)) => format!("{name}+{:#x}", address - start),
            None => format!("{address:#x}"),
        }
    };
    let mut areas: Vec<String> = index
        .areas
        .iter()
        .map(|&(low, high, cu)| format!("{} {} {cu}", name(low), high - low))
        .collect();
    areas.sort();
    (areas, index.names, index.units.len())
}

fn link_args<'a>(objects: &[&'a str]) -> Vec<&'a str> {
    let mut args = objects.to_vec();
    args.extend(["-e", "_start", "--gdb-index"]);
    args
}

#[test]
fn gdb_index_lists_units_ranges_and_names() {
    let Some((cc, cxx)) = compilers() else { return };
    let dir = scratch("pubnames");
    compile(&dir, &cc, &cxx, &["-g", "-O1", "-ggnu-pubnames"]);
    let mut args = link_args(&["a.o", "b.o"]);
    args.extend(["-o", "out"]);
    qld(&dir, &args);
    let index = read_gdb_index(&dir.join("out")).expect("no .gdb_index");
    assert_eq!(index.version, 8);
    assert_eq!(index.units.len(), 2);
    assert_eq!(index.units[0].0, 0, "the first unit starts .debug_info");
    // `add` is in a.c's unit (0), `_start` in b.cc's (1).
    let functions = functions(&dir.join("out"));
    for (function, unit) in [("add", 0), ("_start", 1)] {
        let (address, _) = functions.iter().find(|(_, n)| n == function).unwrap();
        assert!(
            index
                .areas
                .iter()
                .any(|&(low, high, cu)| low <= *address && *address < high && cu == unit),
            "{function} not covered by unit {unit}: {:?}",
            index.areas
        );
    }
    // Kind in bits 28-30 (1 type, 2 variable, 3 function), static in bit 31.
    let kind = |name: &str| index.names.get(name).map(|v| v[0] >> 28);
    assert_eq!(kind("add"), Some(3));
    assert_eq!(kind("helper"), Some(3 | 8));
    assert_eq!(kind("global_var"), Some(2));
    assert!(index.names.contains_key("ns::inner::deep"));
    // The consumed input sections are gone.
    assert!(!has_section(&dir.join("out"), b".debug_gnu_pubnames"));
    assert!(!has_section(&dir.join("out"), b".debug_gnu_pubtypes"));
}

#[test]
fn gdb_index_matches_lld() {
    let Some((cc, cxx)) = compilers() else { return };
    let Some(lld) = &tools().lld else {
        skip("no lld");
        return;
    };
    for (name, flags) in [
        ("dwarf5", &["-g", "-O1", "-ggnu-pubnames"][..]),
        (
            "dwarf4-sections",
            &["-gdwarf-4", "-O2", "-ffunction-sections", "-ggnu-pubnames"][..],
        ),
        (
            "types",
            &["-g", "-O0", "-fdebug-types-section", "-ggnu-pubnames"][..],
        ),
    ] {
        let dir = scratch(&format!("lld-{name}"));
        compile(&dir, &cc, &cxx, flags);
        let mut ours = link_args(&["a.o", "b.o"]);
        ours.extend(["-o", "qld.out"]);
        qld(&dir, &ours);
        let mut theirs = link_args(&["a.o", "b.o"]);
        theirs.extend(["-o", "lld.out"]);
        run_ok(&dir, lld, &theirs);
        assert_eq!(
            canonical(&dir.join("qld.out")),
            canonical(&dir.join("lld.out")),
            "{name}"
        );
    }
}

#[test]
fn gdb_index_scans_dies_without_pubnames() {
    let Some((cc, cxx)) = compilers() else { return };
    let dir = scratch("scan");
    compile(&dir, &cc, &cxx, &["-g", "-O1"]);
    let mut args = link_args(&["a.o", "b.o"]);
    args.extend(["-o", "out"]);
    qld(&dir, &args);
    let index = read_gdb_index(&dir.join("out")).expect("no .gdb_index");
    for name in [
        "add",
        "global_var",
        "ns::Widget",
        "ns::inner::deep",
        "_start",
        "int",
    ] {
        assert!(
            index.names.contains_key(name),
            "{name} missing: {:?}",
            index.names.keys()
        );
    }
    // The names are those Clang would list with -ggnu-pubnames.
    if driver_is_clang(&cxx) && driver_is_clang(&cc) {
        let pub_dir = scratch("scan-pubnames");
        compile(&pub_dir, &cc, &cxx, &["-g", "-O1", "-ggnu-pubnames"]);
        let mut args = link_args(&["a.o", "b.o"]);
        args.extend(["-o", "out"]);
        qld(&pub_dir, &args);
        let with = read_gdb_index(&pub_dir.join("out")).unwrap();
        assert_eq!(index.names, with.names);
    }
}

#[test]
fn gdb_index_is_deterministic_across_threads() {
    let Some((cc, cxx)) = compilers() else { return };
    let dir = scratch("threads");
    compile(&dir, &cc, &cxx, &["-g", "-O1"]);
    let mut first: Option<Vec<u8>> = None;
    for threads in ["1", "2"] {
        let out = format!("t{threads}.out");
        let threads = format!("--threads={threads}");
        let mut args = link_args(&["a.o", "b.o"]);
        args.extend([threads.as_str(), "-o", &out]);
        qld(&dir, &args);
        let bytes = fs::read(dir.join(&out)).unwrap();
        match &first {
            None => first = Some(bytes),
            Some(first) => assert!(*first == bytes, "output differs with {threads}"),
        }
    }
}

#[test]
fn gdb_loads_the_index() {
    let Some((cc, cxx)) = compilers() else { return };
    let Some(gdb) = find_program("gdb") else {
        skip("no gdb");
        return;
    };
    let dir = scratch("gdb");
    compile(&dir, &cc, &cxx, &["-g", "-O1"]);
    let mut args = link_args(&["a.o", "b.o"]);
    args.extend(["-o", "out"]);
    qld(&dir, &args);
    let output = run_ok(
        &dir,
        &gdb,
        &[
            "-nx",
            "-batch",
            "-ex",
            "maint print objfiles",
            "-ex",
            "info address ns::inner::deep",
            "out",
        ],
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains(".gdb_index: version 8"), "{text}");
    assert!(text.contains("is a function at address"), "{text}");
}

#[test]
fn gdb_index_rejects_relocatable_output() {
    let Some((cc, cxx)) = compilers() else { return };
    let dir = scratch("relocatable");
    compile(&dir, &cc, &cxx, &["-g"]);
    let output = run(
        &dir,
        Path::new(env!("CARGO_BIN_EXE_qld")),
        &["-r", "--gdb-index", "a.o", "-o", "out.o"],
    );
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("-r and --gdb-index may not be used together")
    );
}

// ---------------------------------------------------------------------------
// --debug-names
// ---------------------------------------------------------------------------

/// Clang and clang++, which write `.debug_names` with `-gpubnames` (GCC
/// does not), or `None` after printing why.
fn clang() -> Option<(PathBuf, PathBuf)> {
    if !elf_host() {
        return None;
    }
    let t = tools();
    let pick = |configured: &Option<PathBuf>, name: &str| {
        configured
            .clone()
            .filter(|c| driver_is_clang(c))
            .or_else(|| find_program(name))
    };
    match (pick(&t.cc, "clang"), pick(&t.cxx, "clang++")) {
        (Some(cc), Some(cxx)) => Some((cc, cxx)),
        _ => {
            assert!(!tools_required(), "no x86-64 clang");
            skip("no x86-64 clang");
            None
        }
    }
}

/// One entry of a decoded name index: the abbreviation's tag and its
/// attributes (index, value), with `DW_IDX_parent` replaced by the
/// parent's name.
type NameEntry = (u64, Vec<(u64, String)>);

/// A decoded `.debug_names`: unit, local and foreign type unit counts, and
/// every name with its entries (sorted).
#[derive(Debug, PartialEq, Eq)]
struct NameIndex {
    units: usize,
    local_types: usize,
    foreign_types: usize,
    names: BTreeMap<String, Vec<NameEntry>>,
}

fn uleb(data: &[u8], at: &mut usize) -> u64 {
    let mut value = 0u64;
    let mut shift = 0;
    loop {
        let byte = data[*at];
        *at += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return value;
        }
        shift += 7;
    }
}

/// `.debug_names` is a DWARF section: it is in the output's byte order.
fn read_debug_names(path: &Path) -> Option<NameIndex> {
    let big = is_big_endian(path);
    let u32_at = |d: &[u8], at: usize| {
        let b: [u8; 4] = d[at..at + 4].try_into().unwrap();
        if big {
            u32::from_be_bytes(b)
        } else {
            u32::from_le_bytes(b)
        }
    };
    let u64_at = |d: &[u8], at: usize| {
        let b: [u8; 8] = d[at..at + 8].try_into().unwrap();
        if big {
            u64::from_be_bytes(b)
        } else {
            u64::from_le_bytes(b)
        }
    };
    let u16_at = |d: &[u8], at: usize| {
        let b: [u8; 2] = d[at..at + 2].try_into().unwrap();
        if big {
            u16::from_be_bytes(b)
        } else {
            u16::from_le_bytes(b)
        }
    };
    let d = &section_bytes(path, b".debug_names")?[..];
    let strings = &section_bytes(path, b".debug_str").unwrap()[..];
    let length = u32_at(d, 0) as usize;
    assert_eq!(length + 4, d.len(), "one name index covers the section");
    let count = |i: usize| u32_at(d, 8 + i * 4) as usize;
    let (units, locals, foreigns, buckets, names, abbrev_size, augmentation) = (
        count(0),
        count(1),
        count(2),
        count(3),
        count(4),
        count(5),
        count(6),
    );
    let mut at = 36 + augmentation + units * 4 + locals * 4 + foreigns * 8 + buckets * 4;
    if buckets > 0 {
        at += names * 4;
    }
    let string_offsets = at;
    let entry_offsets = at + names * 4;
    let abbrevs_at = entry_offsets + names * 4;
    let pool = abbrevs_at + abbrev_size;
    let mut abbrevs: BTreeMap<u64, (u64, Vec<(u64, u64)>)> = BTreeMap::new();
    let mut a = abbrevs_at;
    loop {
        let code = uleb(d, &mut a);
        if code == 0 {
            break;
        }
        let tag = uleb(d, &mut a);
        let mut attrs = Vec::new();
        loop {
            let (idx, form) = (uleb(d, &mut a), uleb(d, &mut a));
            if idx == 0 && form == 0 {
                break;
            }
            attrs.push((idx, form));
        }
        abbrevs.insert(code, (tag, attrs));
    }
    let name_at = |i: usize| {
        let offset = u32_at(d, string_offsets + i * 4) as usize;
        let rest = &strings[offset..];
        String::from_utf8_lossy(&rest[..rest.iter().position(|&b| b == 0).unwrap()]).into_owned()
    };
    // Entry offset (in the pool) -> name, for parents.
    let mut owner: BTreeMap<usize, String> = BTreeMap::new();
    type RawEntries = Vec<(u64, Vec<(u64, u64)>)>;
    let mut raw: Vec<(String, RawEntries)> = Vec::new();
    for i in 0..names {
        let name = name_at(i);
        let mut e = pool + u32_at(d, entry_offsets + i * 4) as usize;
        let mut entries = Vec::new();
        loop {
            let start = e - pool;
            let code = uleb(d, &mut e);
            if code == 0 {
                break;
            }
            owner.insert(start, name.clone());
            let (tag, attrs) = &abbrevs[&code];
            let mut values = Vec::new();
            for &(idx, form) in attrs {
                let value = match form {
                    0x19 => 1,
                    0x0b | 0x11 => u64::from(d[e]),
                    0x05 | 0x12 => u64::from(u16_at(d, e)),
                    0x06 | 0x13 => u64::from(u32_at(d, e)),
                    0x07 | 0x14 => u64_at(d, e),
                    other => panic!("form {other:#x}"),
                };
                e += match form {
                    0x19 => 0,
                    0x0b | 0x11 => 1,
                    0x05 | 0x12 => 2,
                    0x06 | 0x13 => 4,
                    _ => 8,
                };
                values.push((idx, value));
            }
            entries.push((*tag, values));
        }
        raw.push((name, entries));
    }
    let mut index = NameIndex {
        units,
        local_types: locals,
        foreign_types: foreigns,
        names: BTreeMap::new(),
    };
    for (name, entries) in raw {
        let mut decoded: Vec<NameEntry> = entries
            .into_iter()
            .map(|(tag, values)| {
                let values = values
                    .into_iter()
                    .map(|(idx, value)| {
                        let shown = if idx == 4 && value != 1 {
                            owner.get(&(value as usize)).cloned().unwrap_or_default()
                        } else {
                            value.to_string()
                        };
                        (idx, shown)
                    })
                    .collect();
                (tag, values)
            })
            .collect();
        decoded.sort();
        index.names.entry(name).or_default().extend(decoded);
    }
    Some(index)
}

#[test]
fn debug_names_merges_the_input_indexes() {
    let Some((cc, cxx)) = clang() else { return };
    let dir = scratch("debug-names");
    compile(&dir, &cc, &cxx, &["-g", "-gdwarf-5", "-gpubnames", "-O1"]);
    let args = ["a.o", "b.o", "-e", "_start", "--debug-names"];
    let mut ours = args.to_vec();
    ours.extend(["-o", "qld.out"]);
    qld(&dir, &ours);
    let index = read_debug_names(&dir.join("qld.out")).expect("no .debug_names");
    assert_eq!(index.units, 2);
    for name in ["add", "global_var", "deep", "Widget", "_start", "int"] {
        assert!(
            index.names.contains_key(name),
            "{name}: {:?}",
            index.names.keys()
        );
    }
    // `deep` has `inner` as its parent.
    let deep = &index.names["deep"];
    assert!(
        deep.iter()
            .any(|(_, values)| values.contains(&(4, "inner".to_string()))),
        "{deep:?}"
    );
    if let Some(lld) = &tools().lld {
        let mut theirs = args.to_vec();
        theirs.extend(["-o", "lld.out"]);
        run_ok(&dir, lld, &theirs);
        assert_eq!(Some(index), read_debug_names(&dir.join("lld.out")));
    }
    if let Some(dwarfdump) = find_program("llvm-dwarfdump") {
        let output = run(&dir, &dwarfdump, &["--verify", "--debug-names", "qld.out"]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
}

#[test]
fn debug_names_are_deterministic_across_threads() {
    let Some((cc, cxx)) = clang() else { return };
    let dir = scratch("debug-names-threads");
    compile(&dir, &cc, &cxx, &["-g", "-gdwarf-5", "-gpubnames", "-O1"]);
    let mut first: Option<Vec<u8>> = None;
    for threads in ["1", "2"] {
        let out = format!("t{threads}.out");
        let threads = format!("--threads={threads}");
        qld(
            &dir,
            &[
                "a.o",
                "b.o",
                "-e",
                "_start",
                "--debug-names",
                "--gdb-index",
                &threads,
                "-o",
                &out,
            ],
        );
        let bytes = fs::read(dir.join(&out)).unwrap();
        match &first {
            None => first = Some(bytes),
            Some(first) => assert!(*first == bytes, "output differs with {threads}"),
        }
    }
}

#[test]
fn debug_names_merges_type_units() {
    let Some((cc, cxx)) = clang() else { return };
    let dir = scratch("debug-names-types");
    compile(
        &dir,
        &cc,
        &cxx,
        &[
            "-g",
            "-gdwarf-5",
            "-gpubnames",
            "-fdebug-types-section",
            "-O1",
        ],
    );
    qld(
        &dir,
        &["a.o", "b.o", "-e", "_start", "--debug-names", "-o", "out"],
    );
    let index = read_debug_names(&dir.join("out")).expect("no .debug_names");
    assert_eq!(index.units, 2);
    assert!(index.local_types > 0, "{index:?}");
    // Every type unit index an entry names is in the merged list.
    for entries in index.names.values() {
        for (_, values) in entries {
            for (idx, value) in values {
                if *idx == 2 {
                    let unit: usize = value.parse().unwrap();
                    assert!(unit < index.local_types + index.foreign_types, "{values:?}");
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Big-endian output (s390x).
// ---------------------------------------------------------------------------

const BIG_ENDIAN_TRIPLE: &str = "s390x-linux-gnu";

/// A clang that can compile the test sources for big-endian s390x, with
/// its C++ driver and the target flags. The sources are freestanding, so
/// no cross sysroot is needed — only a clang with the SystemZ back end.
fn big_endian_compilers() -> Option<(PathBuf, PathBuf, Vec<String>)> {
    // Probed once: two tests want it, and they run in parallel, so each
    // call used to build in the same scratch directory and one could
    // delete the other's while it was being written.
    static PROBE: OnceLock<Option<(PathBuf, PathBuf, Vec<String>)>> = OnceLock::new();
    PROBE.get_or_init(probe_big_endian_compilers).clone()
}

fn probe_big_endian_compilers() -> Option<(PathBuf, PathBuf, Vec<String>)> {
    let t = tools();
    let pick = |configured: &Option<PathBuf>, name: &str| {
        configured
            .clone()
            .filter(|c| driver_is_clang(c))
            .or_else(|| find_program(name))
    };
    let (Some(cc), Some(cxx)) = (pick(&t.cc, "clang"), pick(&t.cxx, "clang++")) else {
        assert!(!tools_required(), "no clang for {BIG_ENDIAN_TRIPLE}");
        skip("no clang (needed for big-endian objects)");
        return None;
    };
    let flags = vec![format!("--target={BIG_ENDIAN_TRIPLE}")];
    let dir = scratch("s390x-probe");
    fs::write(dir.join("t.c"), "int t(void) { return 0; }\n").unwrap();
    let mut args: Vec<&str> = flags.iter().map(String::as_str).collect();
    args.extend(["-c", "t.c", "-o", "t.o"]);
    if !run(&dir, &cc, &args).status.success() {
        assert!(!tools_required(), "clang cannot build {BIG_ENDIAN_TRIPLE}");
        skip(format!("{} cannot build {BIG_ENDIAN_TRIPLE}", cc.display()));
        return None;
    }
    Some((cc, cxx, flags))
}

/// Compiles the sources for s390x into `a.o` and `b.o`.
fn compile_big_endian(dir: &Path, cc: &Path, cxx: &Path, target: &[String], flags: &[&str]) {
    fs::write(dir.join("a.c"), C_SOURCE).unwrap();
    fs::write(dir.join("b.cc"), CXX_SOURCE).unwrap();
    let target: Vec<&str> = target.iter().map(String::as_str).collect();
    let mut args: Vec<&str> = target.clone();
    args.extend(["-c", "a.c", "-o", "a.o"]);
    args.extend_from_slice(flags);
    run_ok(dir, cc, &args);
    let mut args: Vec<&str> = target;
    args.extend([
        "-c",
        "b.cc",
        "-o",
        "b.o",
        "-fno-exceptions",
        "-fno-rtti",
        "-fno-asynchronous-unwind-tables",
    ]);
    args.extend_from_slice(flags);
    run_ok(dir, cxx, &args);
}

/// `.gdb_index` stays little-endian on a big-endian target, and holds the
/// same units, ranges and names as lld's.
#[test]
fn gdb_index_matches_lld_on_big_endian() {
    let Some((cc, cxx, target)) = big_endian_compilers() else {
        return;
    };
    let Some(lld) = &tools().lld else {
        skip("no lld");
        return;
    };
    let dir = scratch("s390x-gdb-index");
    compile_big_endian(&dir, &cc, &cxx, &target, &["-g", "-O1", "-ggnu-pubnames"]);
    let args = [
        "-m",
        "elf64_s390",
        "a.o",
        "b.o",
        "-e",
        "_start",
        "--gdb-index",
    ];
    let mut ours = args.to_vec();
    ours.extend(["-o", "qld.out"]);
    qld(&dir, &ours);
    let mut theirs = args.to_vec();
    theirs.extend(["-o", "lld.out"]);
    run_ok(&dir, lld, &theirs);
    assert!(is_big_endian(&dir.join("qld.out")));
    let index = read_gdb_index(&dir.join("qld.out")).expect("no .gdb_index");
    assert_eq!(index.version, 8);
    assert_eq!(index.units.len(), 2);
    assert!(index.names.contains_key("ns::inner::deep"), "{index:?}");
    assert_eq!(
        canonical(&dir.join("qld.out")),
        canonical(&dir.join("lld.out"))
    );
}

/// `.debug_names` follows the output's byte order, and holds the same
/// index as lld's.
#[test]
fn debug_names_matches_lld_on_big_endian() {
    let Some((cc, cxx, target)) = big_endian_compilers() else {
        return;
    };
    let Some(lld) = &tools().lld else {
        skip("no lld");
        return;
    };
    let dir = scratch("s390x-debug-names");
    compile_big_endian(
        &dir,
        &cc,
        &cxx,
        &target,
        &["-g", "-gdwarf-5", "-gpubnames", "-O1"],
    );
    let args = [
        "-m",
        "elf64_s390",
        "a.o",
        "b.o",
        "-e",
        "_start",
        "--debug-names",
    ];
    let mut ours = args.to_vec();
    ours.extend(["-o", "qld.out"]);
    qld(&dir, &ours);
    let mut theirs = args.to_vec();
    theirs.extend(["-o", "lld.out"]);
    run_ok(&dir, lld, &theirs);
    let index = read_debug_names(&dir.join("qld.out")).expect("no .debug_names");
    assert_eq!(index.units, 2);
    for name in ["add", "global_var", "deep", "Widget", "_start", "int"] {
        assert!(
            index.names.contains_key(name),
            "{name}: {:?}",
            index.names.keys()
        );
    }
    // The section really is big-endian: its first word is the length.
    let bytes = section_bytes(&dir.join("qld.out"), b".debug_names").unwrap();
    assert_eq!(
        u32::from_be_bytes(bytes[..4].try_into().unwrap()) as usize + 4,
        bytes.len()
    );
    assert_eq!(Some(index), read_debug_names(&dir.join("lld.out")));
    if let Some(dwarfdump) = find_program("llvm-dwarfdump") {
        let output = run(&dir, &dwarfdump, &["--verify", "--debug-names", "qld.out"]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
}

#[test]
fn debug_names_are_compressed_with_debug_sections() {
    let Some((cc, cxx)) = clang() else { return };
    let dir = scratch("debug-names-compressed");
    compile(&dir, &cc, &cxx, &["-g", "-gdwarf-5", "-gpubnames", "-O1"]);
    let base = ["a.o", "b.o", "-e", "_start", "--debug-names"];
    let mut plain = base.to_vec();
    plain.extend(["-o", "plain.out"]);
    qld(&dir, &plain);
    let mut compressed = base.to_vec();
    compressed.extend(["--compress-debug-sections=zlib", "-o", "z.out"]);
    qld(&dir, &compressed);
    let data = fs::read(dir.join("z.out")).unwrap();
    let elf = ElfFile::<Elf64Le>::parse(&data, Source::new(Path::new("z.out"))).unwrap();
    let (_, header) = elf.section_by_name(b".debug_names").unwrap();
    assert_ne!(header.sh_flags & 0x800, 0, "SHF_COMPRESSED");
    if let Some(dwarfdump) = find_program("llvm-dwarfdump") {
        let dump = |file: &str| {
            let text = run_ok(&dir, &dwarfdump, &["--debug-names", file]).stdout;
            String::from_utf8_lossy(&text)
                .lines()
                .skip(1)
                .map(str::to_owned)
                .collect::<Vec<_>>()
        };
        assert_eq!(dump("plain.out"), dump("z.out"));
    }
}

// ---------------------------------------------------------------------------
// --separate-debug-file
// ---------------------------------------------------------------------------

/// CRC-32 (the `.gnu_debuglink` checksum), bit by bit.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// Section name, type and flags of every section of `path`.
fn section_list(path: &Path) -> Vec<(String, u32, u64)> {
    let data = fs::read(path).unwrap();
    let elf = ElfFile::<Elf64Le>::parse(&data, Source::new(path)).unwrap();
    elf.enumerate_sections()
        .skip(1)
        .map(|(_, h)| {
            let name = elf.section_name(&h).unwrap();
            (
                String::from_utf8_lossy(name).into_owned(),
                h.sh_type,
                h.sh_flags,
            )
        })
        .collect()
}

/// The build ID of `path`.
fn build_id(path: &Path) -> Vec<u8> {
    let data = fs::read(path).unwrap();
    let elf = ElfFile::<Elf64Le>::parse(&data, Source::new(path)).unwrap();
    let note = section(&elf, b".note.gnu.build-id").expect("no build ID");
    note[16..].to_vec()
}

#[test]
fn separate_debug_file_splits_the_output() {
    let Some((cc, cxx)) = compilers() else { return };
    let dir = scratch("separate");
    compile(&dir, &cc, &cxx, &["-g", "-O1"]);
    qld(
        &dir,
        &[
            "a.o",
            "b.o",
            "-e",
            "_start",
            "--build-id",
            "--gdb-index",
            "--separate-debug-file",
            "-o",
            "prog",
        ],
    );
    let main = section_list(&dir.join("prog"));
    let names: Vec<&str> = main.iter().map(|(n, ..)| n.as_str()).collect();
    assert!(names.contains(&".gnu_debuglink"), "{names:?}");
    for (name, ..) in &main {
        assert!(
            !name.starts_with(".debug") && name != ".symtab" && name != ".gdb_index",
            "{name} is in the output"
        );
    }
    // The debug file mirrors the output's sections, empty but for debug
    // information, notes and the symbol table.
    let debug = section_list(&dir.join("prog.dbg"));
    for (name, sh_type, flags) in &debug {
        if flags & 2 != 0 && *sh_type != 7 {
            assert_eq!(*sh_type, 8, "{name} should be SHT_NOBITS");
        }
    }
    let debug_names: Vec<&str> = debug.iter().map(|(n, ..)| n.as_str()).collect();
    for name in [
        ".debug_info",
        ".symtab",
        ".gdb_index",
        ".text",
        ".note.gnu.build-id",
    ] {
        assert!(debug_names.contains(&name), "{name}: {debug_names:?}");
    }
    assert!(!debug_names.contains(&".gnu_debuglink"));
    assert_eq!(build_id(&dir.join("prog")), build_id(&dir.join("prog.dbg")));
    // `.gnu_debuglink` names the file and holds its CRC.
    let data = fs::read(dir.join("prog")).unwrap();
    let elf = ElfFile::<Elf64Le>::parse(&data, Source::new(Path::new("prog"))).unwrap();
    let link = section(&elf, b".gnu_debuglink").unwrap();
    assert_eq!(&link[..9], b"prog.dbg\0");
    let crc = u32::from_le_bytes(link[link.len() - 4..].try_into().unwrap());
    assert_eq!(crc, crc32(&fs::read(dir.join("prog.dbg")).unwrap()));

    if let Some(gdb) = find_program("gdb") {
        let output = run_ok(
            &dir,
            &gdb,
            &["-nx", "-batch", "-ex", "info line add", "prog"],
        );
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(text.contains("of \"a.c\""), "{text}");
    }
}

#[test]
fn separate_debug_file_takes_a_path_and_is_deterministic() {
    let Some((cc, cxx)) = compilers() else { return };
    let dir = scratch("separate-path");
    compile(&dir, &cc, &cxx, &["-g", "-O1"]);
    fs::create_dir_all(dir.join("sub")).unwrap();
    let mut first: Option<(Vec<u8>, Vec<u8>)> = None;
    for threads in ["1", "2"] {
        let threads = format!("--threads={threads}");
        qld(
            &dir,
            &[
                "a.o",
                "b.o",
                "-e",
                "_start",
                &threads,
                "--separate-debug-file=sub/x.debug",
                "-o",
                "prog",
            ],
        );
        let outputs = (
            fs::read(dir.join("prog")).unwrap(),
            fs::read(dir.join("sub/x.debug")).unwrap(),
        );
        // Without a build ID, the CRC still matches.
        let data = &outputs.0;
        let elf = ElfFile::<Elf64Le>::parse(data, Source::new(Path::new("prog"))).unwrap();
        let link = section(&elf, b".gnu_debuglink").unwrap();
        assert_eq!(&link[..8], b"x.debug\0");
        let crc = u32::from_le_bytes(link[link.len() - 4..].try_into().unwrap());
        assert_eq!(crc, crc32(&outputs.1));
        match &first {
            None => first = Some(outputs),
            Some(first) => assert!(*first == outputs, "outputs differ with {threads}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Malformed debug information
// ---------------------------------------------------------------------------

/// Corrupts the debug sections of an object in many ways (xorshift PRNG,
/// fixed seed) and links it with every debug output: qld may report
/// errors or warnings but must never crash.
#[test]
fn malformed_debug_info_never_crashes() {
    let Some((cc, cxx)) = compilers() else { return };
    let dir = scratch("malformed");
    let pubnames = if driver_is_clang(&cxx) {
        "-gpubnames"
    } else {
        "-ggnu-pubnames"
    };
    compile(&dir, &cc, &cxx, &["-g", "-O1", pubnames]);
    let object = fs::read(dir.join("b.o")).unwrap();
    // The file ranges of the debug sections and their relocations.
    let ranges: Vec<(usize, usize)> = {
        let elf = ElfFile::<Elf64Le>::parse(&object, Source::new(Path::new("b.o")))
            .expect("the C++ compiler did not build an ELF object");
        elf.enumerate_sections()
            .filter(|(_, h)| {
                elf.section_name(h)
                    .is_ok_and(|n| n.starts_with(b".debug") || n.starts_with(b".rela.debug"))
            })
            .map(|(_, h)| (h.sh_offset as usize, h.sh_size as usize))
            .filter(|&(_, size)| size > 0)
            .collect()
    };
    assert!(!ranges.is_empty());
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for round in 0..120 {
        let mut corrupt = object.clone();
        let (start, size) = ranges[(next() as usize) % ranges.len()];
        for _ in 0..1 + next() % 8 {
            let at = start + (next() as usize) % size;
            corrupt[at] = match next() % 4 {
                0 => 0xff,
                1 => 0,
                _ => next() as u8,
            };
        }
        fs::write(dir.join("bad.o"), &corrupt).unwrap();
        let output = run(
            &dir,
            Path::new(env!("CARGO_BIN_EXE_qld")),
            &[
                "a.o",
                "bad.o",
                "-e",
                "_start",
                "--gdb-index",
                "--debug-names",
                "--threads=2",
                "-o",
                "out",
            ],
        );
        assert!(
            matches!(output.status.code(), Some(0 | 1)),
            "round {round}: qld crashed ({:?}):\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
