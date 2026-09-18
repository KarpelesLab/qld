//! Debug index tests (workstream W29): `--gdb-index` (and, later in this
//! file, `--debug-names`).
//!
//! The inputs are small freestanding C and C++ programs compiled with the
//! host compilers, linked without libc. The index is decoded here and
//! checked for its units, address ranges and names. When lld is installed
//! (`QLD_TEST_LLD`), the same objects are linked with `ld.lld --gdb-index`
//! and the two indexes must agree, with addresses compared by the symbol
//! they fall in. When GDB is installed, it must load the index.
//!
//! Tests print `SKIPPED:` when a tool is missing, and fail instead when
//! `QLD_REQUIRE_TOOLS` is set.

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use common::tools::{driver_is_clang, find_program, skip, tools, tools_required};
use qld::elf::read::{Elf64Le, ElfFile, SectionIndex, Source};

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("debug-index-tests")
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
fn compilers() -> Option<(PathBuf, PathBuf)> {
    let t = tools();
    match (&t.cc, &t.cxx) {
        (Some(cc), Some(cxx)) if t.host.as_ref().is_some_and(|h| h.arch == "x86_64") => {
            Some((cc.clone(), cxx.clone()))
        }
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

fn read_gdb_index(path: &Path) -> Option<GdbIndex> {
    let data = fs::read(path).unwrap();
    let elf = ElfFile::<Elf64Le>::parse(&data, Source::new(path)).unwrap();
    let d = section(&elf, b".gdb_index")?;
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

/// The function symbols of an output, sorted by address.
fn functions(path: &Path) -> Vec<(u64, String)> {
    let data = fs::read(path).unwrap();
    let elf = ElfFile::<Elf64Le>::parse(&data, Source::new(path)).unwrap();
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
    let data = fs::read(dir.join("out")).unwrap();
    let elf = ElfFile::<Elf64Le>::parse(&data, Source::new(Path::new("out"))).unwrap();
    assert!(elf.section_by_name(b".debug_gnu_pubnames").is_none());
    assert!(elf.section_by_name(b".debug_gnu_pubtypes").is_none());
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
