//! Integration tests for debug-information handling (`qld::debug`,
//! workstream W10).
//!
//! Codec tests cross-check qld against the system's zlib (through
//! `python3`), the `zstd` command and binutils (`objcopy`, `readelf`).
//! DWARF tests compile small programs with the host C compiler and compare
//! qld's line lookup with `addr2line` / `readelf --debug-dump=decodedline`.
//! Every test that needs a missing tool prints `SKIPPED: <reason>` and
//! passes. Small committed fixtures under `tests/data/debug/` keep the core
//! checks running on machines without those tools.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use qld::debug::compress::deflate::{Level, zlib_compress, zlib_compress_chunked};
use qld::debug::compress::{Codec, zlib_decompress_into};
use qld::debug::section::{
    CompressedSection, OutputCompression, compress_section, decompressed_name, encode_chdr,
    section_contents,
};
use qld::elf::read::{Elf64Le, ObjectFile, Source};

fn skip(reason: impl std::fmt::Display) {
    println!("SKIPPED: {reason}");
}

fn find_program(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

fn scratch_dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("qld-tests")
        .join("debug")
        .join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch directory");
    dir
}

fn data_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/debug")
}

/// Runs a command, returning stdout on success.
fn run(command: &mut Command) -> Option<Vec<u8>> {
    let output = command
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        eprintln!(
            "command failed: {command:?}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        return None;
    }
    Some(output.stdout)
}

fn noise(len: usize, mut seed: u64) -> Vec<u8> {
    seed |= 1;
    (0..len)
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 24) as u8
        })
        .collect()
}

/// Test corpora: text-like, binary, runs, incompressible, and mixtures.
fn corpora() -> Vec<(String, Vec<u8>)> {
    let mut sets = vec![
        ("empty".to_string(), Vec::new()),
        ("one".to_string(), vec![42]),
        ("zeros-100k".to_string(), vec![0; 100_000]),
        ("noise-70k".to_string(), noise(70_000, 1)),
    ];
    let mut text = Vec::new();
    let mut i = 0u32;
    while text.len() < 300_000 {
        text.extend_from_slice(
            format!(
                "DW_TAG_variable name=var_{} type=0x{:x} line={}\n",
                i % 977,
                i.wrapping_mul(2_654_435_761) % 4096,
                i / 3
            )
            .as_bytes(),
        );
        i += 1;
    }
    sets.push(("text-300k".to_string(), text.clone()));
    let mut mixed = Vec::new();
    for (k, chunk) in text.chunks(10_000).enumerate() {
        mixed.extend_from_slice(chunk);
        mixed.extend_from_slice(&noise(1_000 + k * 37, k as u64));
        mixed.extend(std::iter::repeat_n((k % 5) as u8, k * 13));
    }
    sets.push(("mixed".to_string(), mixed));
    // Periodic data with short periods (overlapping matches).
    let periodic: Vec<u8> = (0..200_000u32).map(|i| (i % 7) as u8 + b'a').collect();
    sets.push(("periodic".to_string(), periodic));
    // This test binary itself: realistic machine code and debug info.
    if let Ok(exe) = std::env::current_exe()
        && let Ok(bytes) = std::fs::read(exe)
    {
        let len = bytes.len().min(1 << 20);
        sets.push(("self-exe".to_string(), bytes[..len].to_vec()));
    }
    sets
}

// ---------------------------------------------------------------------------
// zlib
// ---------------------------------------------------------------------------

/// Compresses every corpus with Python's zlib at every level and strategy,
/// and checks that qld's inflate reproduces the input.
#[test]
fn inflate_matches_system_zlib() {
    let Some(python) = find_program("python3") else {
        return skip("python3 not found");
    };
    let dir = scratch_dir("inflate-zlib");
    let sets = corpora();
    for (name, data) in &sets {
        std::fs::write(dir.join(format!("{name}.bin")), data).unwrap();
    }
    let script = r#"
import sys, zlib, os
d = sys.argv[1]
strategies = [zlib.Z_DEFAULT_STRATEGY, zlib.Z_FILTERED, zlib.Z_HUFFMAN_ONLY, zlib.Z_RLE, zlib.Z_FIXED]
for f in sorted(os.listdir(d)):
    if not f.endswith('.bin'):
        continue
    data = open(os.path.join(d, f), 'rb').read()
    for level in (0, 1, 2, 4, 6, 9):
        for s in strategies:
            for mem in ((1, 9) if s == 0 else (8,)):
                c = zlib.compressobj(level, zlib.DEFLATED, 15, mem, s)
                out = c.compress(data) + c.flush()
                open(os.path.join(d, '%s.%d.%d.%d.z' % (f[:-4], level, s, mem)), 'wb').write(out)
"#;
    if run(Command::new(python).arg("-c").arg(script).arg(&dir)).is_none() {
        return skip("python3 zlib failed");
    }
    let mut checked = 0;
    for (name, data) in &sets {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let file = path.file_name().unwrap().to_string_lossy().into_owned();
            if !file.starts_with(&format!("{name}.")) || !file.ends_with(".z") {
                continue;
            }
            let compressed = std::fs::read(&path).unwrap();
            let mut out = vec![0u8; data.len()];
            if let Err(error) = zlib_decompress_into(&compressed, &mut out) {
                panic!("{file}: {error}");
            }
            assert!(out == *data, "{file}: output differs");
            checked += 1;
        }
    }
    assert!(checked > 0);
    println!("checked {checked} zlib streams");
}

/// qld's chunked zlib output decompresses with the system zlib, at every
/// level and several chunk sizes.
#[test]
fn deflate_output_accepted_by_system_zlib() {
    let Some(python) = find_program("python3") else {
        return skip("python3 not found");
    };
    let dir = scratch_dir("deflate-zlib");
    let mut jobs = 0;
    for (name, data) in corpora() {
        std::fs::write(dir.join(format!("{name}.bin")), &data).unwrap();
        for level in 0..=9 {
            for chunk in [4096, 1 << 20] {
                let z = zlib_compress_chunked(&data, Level::new(level), chunk);
                std::fs::write(dir.join(format!("{name}.{level}.{chunk}.z")), z).unwrap();
                jobs += 1;
            }
        }
    }
    let script = r#"
import sys, zlib, os
d = sys.argv[1]
n = 0
for f in sorted(os.listdir(d)):
    if not f.endswith('.z'):
        continue
    raw = open(os.path.join(d, f.split('.')[0] + '.bin'), 'rb').read()
    dec = zlib.decompressobj()
    out = dec.decompress(open(os.path.join(d, f), 'rb').read())
    assert dec.eof and not dec.unused_data, f
    assert out == raw, f
    n += 1
print(n)
"#;
    let out = run(Command::new(python).arg("-c").arg(script).arg(&dir))
        .expect("system zlib rejected qld's output");
    assert_eq!(String::from_utf8_lossy(&out).trim(), jobs.to_string());
}

/// The committed zlib fixture decodes (runs without any tools).
#[test]
fn inflate_committed_fixture() {
    let compressed = std::fs::read(data_dir().join("mixed.z")).unwrap();
    let expected = std::fs::read(data_dir().join("mixed.bin")).unwrap();
    let mut out = vec![0u8; expected.len()];
    Codec::Zlib.decompress_into(&compressed, &mut out).unwrap();
    assert_eq!(out, expected);
}

// ---------------------------------------------------------------------------
// Compressed ELF sections
// ---------------------------------------------------------------------------

/// A C source with enough functions and types to give several hundred KiB
/// of DWARF.
fn big_c_source(functions: usize) -> String {
    let mut src = String::from("#include <stddef.h>\n");
    for i in 0..functions {
        src.push_str(&format!(
            "struct s{i} {{ int a; long b[{n}]; const char *name; struct s{i} *next; }};\n\
             static int helper{i}(struct s{i} *p, int k) {{\n  int t = k * {i};\n  \
             for (int j = 0; j < {n}; j++) t += (int)p->b[j];\n  return t + p->a;\n}}\n\
             int func{i}(int k) {{\n  struct s{i} v = {{ k, {{0}}, \"f{i}\", NULL }};\n  \
             return helper{i}(&v, k);\n}}\n",
            n = i % 7 + 1
        ));
    }
    src
}

/// Compiles `source` with the given flags into `dir/name.o`.
fn compile(dir: &Path, name: &str, source: &str, flags: &[&str]) -> Option<PathBuf> {
    let cc = find_program("gcc").or_else(|| find_program("cc"))?;
    let src = dir.join(format!("{name}.c"));
    std::fs::write(&src, source).ok()?;
    let obj = dir.join(format!("{name}.o"));
    run(Command::new(cc)
        .args(flags)
        .args(["-c", "-o"])
        .arg(&obj)
        .arg(&src))?;
    Some(obj)
}

/// Section names and headers of an ELF64 little-endian object.
fn sections(data: &[u8]) -> Vec<(String, qld::elf::read::SectionHeader)> {
    let object = ObjectFile::<Elf64Le>::parse(data, Source::new(Path::new("x.o"))).unwrap();
    object
        .elf()
        .enumerate_sections()
        .map(|(_, h)| {
            let name = String::from_utf8_lossy(object.section_name(&h).unwrap()).into_owned();
            (name, h)
        })
        .collect()
}

/// Decompresses every compressed section with qld and compares it with the
/// same section after `objcopy --decompress-debug-sections`.
#[test]
fn compressed_input_sections_match_objcopy() {
    let Some(objcopy) = find_program("objcopy") else {
        return skip("objcopy not found");
    };
    let dir = scratch_dir("input-sections");
    // Some toolchains compress debug sections by default; start from none.
    let Some(plain) = compile(&dir, "plain", &big_c_source(40), &["-g", "-O1", "-gz=none"]) else {
        return skip("no C compiler");
    };
    let mut variants: Vec<(String, PathBuf)> = Vec::new();
    for kind in ["zlib", "zlib-gnu", "zstd"] {
        let out = dir.join(format!("objcopy-{kind}.o"));
        if run(Command::new(&objcopy)
            .arg(format!("--compress-debug-sections={kind}"))
            .arg(&plain)
            .arg(&out))
        .is_some()
        {
            variants.push((format!("objcopy {kind}"), out));
        }
    }
    for kind in ["zlib", "zstd"] {
        if let Some(obj) = compile(
            &dir,
            &format!("gz-{kind}"),
            &big_c_source(40),
            &["-g", "-O1", &format!("-gz={kind}")],
        ) {
            variants.push((format!("gcc -gz={kind}"), obj));
        }
    }
    let mut checked = 0;
    for (label, path) in &variants {
        let reference = dir.join("reference.o");
        run(Command::new(&objcopy)
            .arg("--decompress-debug-sections")
            .arg(path)
            .arg(&reference))
        .expect("objcopy --decompress-debug-sections");
        let data = std::fs::read(path).unwrap();
        let ref_data = std::fs::read(&reference).unwrap();
        let ref_sections = sections(&ref_data);
        let object = ObjectFile::<Elf64Le>::parse(&data, Source::new(path)).unwrap();
        for (index, header) in object.elf().enumerate_sections() {
            let Some(compressed) = CompressedSection::detect(&object, &header).unwrap() else {
                continue;
            };
            let name = object.section_name(&header).unwrap();
            let name = String::from_utf8_lossy(&decompressed_name(name)).into_owned();
            let expected = ref_sections
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, h)| *h)
                .unwrap_or_else(|| panic!("{label}: {name} missing after objcopy"));
            let expected = &ref_data
                [expected.sh_offset as usize..(expected.sh_offset + expected.sh_size) as usize];
            let got = compressed.decompress(object.source()).unwrap();
            assert!(got == expected, "{label}: section {index} ({name}) differs");
            let contents = section_contents(&object, &header).unwrap();
            assert!(*contents == *expected);
            checked += 1;
        }
    }
    assert!(checked > 0 || variants.is_empty());
    println!("checked {checked} sections in {} objects", variants.len());
}

/// Replaces the contents of section `index` of an ELF64 little-endian file:
/// the new contents go at the end of the file, and the header's offset,
/// size, flags and alignment are updated.
fn replace_section(elf: &mut Vec<u8>, index: usize, contents: &[u8], flags: u64, align: u64) {
    let read_u64 = |b: &[u8], at: usize| u64::from_le_bytes(b[at..at + 8].try_into().unwrap());
    let shoff = read_u64(elf, 0x28) as usize;
    let shentsize = u16::from_le_bytes([elf[0x3a], elf[0x3b]]) as usize;
    while !elf.len().is_multiple_of(8) {
        elf.push(0);
    }
    let offset = elf.len() as u64;
    elf.extend_from_slice(contents);
    let hdr = shoff + index * shentsize;
    elf[hdr + 8..hdr + 16].copy_from_slice(&flags.to_le_bytes());
    elf[hdr + 24..hdr + 32].copy_from_slice(&offset.to_le_bytes());
    elf[hdr + 32..hdr + 40].copy_from_slice(&(contents.len() as u64).to_le_bytes());
    elf[hdr + 48..hdr + 56].copy_from_slice(&align.to_le_bytes());
}

/// Compresses the debug sections of an object with qld, then checks that
/// binutils reads them: `readelf -z --debug-dump` matches the original,
/// and `objcopy --decompress-debug-sections` restores the original bytes.
#[test]
fn compressed_output_sections_accepted_by_binutils() {
    const SHF_COMPRESSED: u64 = 0x800;
    let (Some(objcopy), Some(readelf)) = (find_program("objcopy"), find_program("readelf")) else {
        return skip("objcopy or readelf not found");
    };
    let dir = scratch_dir("output-sections");
    let Some(plain) = compile(
        &dir,
        "plain",
        &big_c_source(400),
        &["-g", "-O1", "-gz=none"],
    ) else {
        return skip("no C compiler");
    };
    let original = std::fs::read(&plain).unwrap();
    let dump = |path: &Path| {
        let out = run(Command::new(&readelf)
            .args([
                "-W",
                "-z",
                "--debug-dump=info,abbrev,line,str,aranges,Ranges,loc",
            ])
            .arg(path))
        .expect("readelf");
        // The dump names the file; compare everything else.
        String::from_utf8_lossy(&out).replace(&path.display().to_string(), "FILE")
    };
    let expected_dump = dump(&plain);

    for label in [
        "zlib-fast",
        "zlib-default-4k-chunks",
        "zlib-stored-1k-chunks",
        "zstd",
        "zstd-1k-frames",
    ] {
        let mut patched = original.clone();
        let mut compressed = 0;
        for (index, (name, header)) in sections(&original).iter().enumerate() {
            if !name.starts_with(".debug_") || header.sh_size == 0 {
                continue;
            }
            let start = header.sh_offset as usize;
            let data = &original[start..start + header.sh_size as usize];
            let align = header.sh_addralign;
            let manual = |ch_type: u32, stream: Vec<u8>| {
                let mut contents = encode_chdr::<Elf64Le>(ch_type, data.len() as u64, align);
                contents.extend_from_slice(&stream);
                contents
            };
            let contents = match label {
                "zlib-fast" => compress_section::<Elf64Le>(
                    data,
                    OutputCompression::Zlib(Level::FASTEST),
                    align,
                ),
                "zlib-default-4k-chunks" => {
                    manual(1, zlib_compress_chunked(data, Level::DEFAULT, 4096))
                }
                "zlib-stored-1k-chunks" => {
                    manual(1, zlib_compress_chunked(data, Level::STORE, 1000))
                }
                "zstd" => compress_section::<Elf64Le>(data, OutputCompression::Zstd, align),
                _ => manual(
                    2,
                    qld::debug::compress::zstd::zstd_compress_chunked(data, 1000),
                ),
            };
            replace_section(
                &mut patched,
                index,
                &contents,
                header.sh_flags | SHF_COMPRESSED,
                8,
            );
            compressed += 1;
        }
        assert!(compressed > 3);
        let path = dir.join(format!("{label}.o"));
        std::fs::write(&path, &patched).unwrap();
        assert!(
            dump(&path) == expected_dump,
            "{label}: readelf -z --debug-dump differs"
        );
        let restored = dir.join(format!("{label}-restored.o"));
        run(Command::new(&objcopy)
            .arg("--decompress-debug-sections")
            .arg(&path)
            .arg(&restored))
        .expect("objcopy --decompress-debug-sections");
        let restored = std::fs::read(&restored).unwrap();
        for ((name, a), (_, b)) in sections(&original).iter().zip(sections(&restored).iter()) {
            if !name.starts_with(".debug_") {
                continue;
            }
            let x = &original[a.sh_offset as usize..(a.sh_offset + a.sh_size) as usize];
            let y = &restored[b.sh_offset as usize..(b.sh_offset + b.sh_size) as usize];
            assert!(x == y, "{label}: {name} differs after objcopy round trip");
        }
    }
}

// ---------------------------------------------------------------------------
// Line lookup
// ---------------------------------------------------------------------------

const LINES_HEADER: &str = r#"
static inline int header_helper(int y) {
  int z = y * 2;
  if (z > 10)
    z -= 3;
  return z;
}
"#;

const LINES_SOURCE: &str = r#"
#include "lines.h"
struct point { int x, y; };

static int twice(int v) { return v * 2; }

int compute(struct point *p, int n) {
  int total = 0;
  for (int i = 0; i < n; i++) {
    total += p[i].x * twice(p[i].y);
    if (total > 1000)
      total = header_helper(total);
  }
  return total;
}

#line 500 "generated.y"
int generated(int a) {
  return a + 42;
}
#line 24 "lines.c"

int caller(void) {
  struct point pts[4] = { {1, 2}, {3, 4}, {5, 6}, {7, 8} };
  int r = compute(pts, 4);
  r += generated(r);
  return r + header_helper(r);
}
"#;

/// Asks `addr2line` for the source position of each offset in `section`.
/// Returns `None` if addr2line reports that it cannot read the DWARF.
///
/// Without `section`, the offsets are addresses (for `llvm-addr2line`, on
/// objects with a single code section).
fn addr2line(
    tool: &Path,
    obj: &Path,
    section: Option<&str>,
    offsets: &[u64],
) -> Option<Vec<Option<(String, u32)>>> {
    let mut cmd = Command::new(tool);
    cmd.arg("-e").arg(obj);
    if let Some(section) = section {
        cmd.args(["-j", section]);
    }
    for offset in offsets {
        cmd.arg(format!("{offset:#x}"));
    }
    let out = cmd.env("LC_ALL", "C").output().expect("addr2line");
    if !out.status.success() || String::from_utf8_lossy(&out.stderr).contains("DWARF error") {
        return None;
    }
    let lines = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|line| {
            let line = line.split(" (discriminator").next().unwrap_or(line);
            let (file, number) = line.rsplit_once(':')?;
            let number: u32 = number.trim().parse().ok()?;
            (file != "??" && number != 0).then(|| (file.to_string(), number))
        })
        .collect();
    Some(lines)
}

/// Compiles the line test program in many configurations and compares
/// qld's line lookup with `addr2line` at every few bytes of every code
/// section.
#[test]
fn line_lookup_matches_addr2line() {
    use qld::debug::dwarf::LineLookup;
    use qld::elf::read::{Elf32Le, ElfFormat};

    let Some(tool) = find_program("addr2line") else {
        return skip("addr2line not found");
    };
    let dir = scratch_dir("lines");
    std::fs::write(dir.join("lines.h"), LINES_HEADER).unwrap();
    let mut configs: Vec<(String, Vec<&str>)> = Vec::new();
    for version in ["-gdwarf-2", "-gdwarf-3", "-gdwarf-4", "-gdwarf-5"] {
        configs.push((format!("gcc {version} -O0"), vec![version, "-O0"]));
        configs.push((
            format!("gcc {version} -O2 sections"),
            vec![version, "-O2", "-ffunction-sections"],
        ));
    }
    configs.push(("gcc dwarf64".into(), vec!["-gdwarf-5", "-gdwarf64", "-O1"]));
    configs.push((
        "gcc dwarf64 v4".into(),
        vec!["-gdwarf-4", "-gdwarf64", "-O1"],
    ));
    configs.push(("gcc zlib".into(), vec!["-gdwarf-5", "-O1", "-gz=zlib"]));
    configs.push(("gcc zstd".into(), vec!["-gdwarf-4", "-O1", "-gz=zstd"]));
    configs.push((
        "gcc split".into(),
        vec!["-gdwarf-5", "-O1", "-gsplit-dwarf"],
    ));
    configs.push((
        "gcc split v4".into(),
        vec!["-gdwarf-4", "-O1", "-gsplit-dwarf"],
    ));
    configs.push(("gcc -m32".into(), vec!["-gdwarf-5", "-O1", "-m32"]));
    configs.push((
        "gcc -m32 v2".into(),
        vec!["-gdwarf-2", "-O1", "-m32", "-ffunction-sections"],
    ));

    let mut compared = 0usize;
    let mut built = 0usize;
    let cc = find_program("gcc");
    let clang = find_program("clang");
    let mut jobs: Vec<(String, PathBuf, Vec<String>)> = Vec::new();
    if let Some(cc) = &cc {
        for (label, flags) in &configs {
            jobs.push((
                label.clone(),
                cc.clone(),
                flags.iter().map(|s| s.to_string()).collect(),
            ));
        }
    }
    if let Some(clang) = &clang {
        for (label, flags) in [
            ("clang v5", vec!["-gdwarf-5", "-O1"]),
            ("clang v4", vec!["-gdwarf-4", "-O2", "-ffunction-sections"]),
            ("clang split", vec!["-gdwarf-5", "-O1", "-gsplit-dwarf"]),
        ] {
            jobs.push((
                label.into(),
                clang.clone(),
                flags.iter().map(|s| s.to_string()).collect(),
            ));
        }
    }
    for (index, (label, compiler, flags)) in jobs.iter().enumerate() {
        let src = dir.join("lines.c");
        std::fs::write(&src, LINES_SOURCE).unwrap();
        let obj = dir.join(format!("lines{index}.o"));
        let ok = Command::new(compiler)
            .current_dir(&dir)
            .args(["-g", "-c"])
            .args(flags)
            .arg("-o")
            .arg(&obj)
            .arg("lines.c")
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            println!("{label}: compiler rejected the flags, skipped");
            continue;
        }
        built += 1;
        let data = std::fs::read(&obj).unwrap();
        let is_32 = data[4] == 1;
        fn check<F: ElfFormat>(data: &[u8], obj: &Path, tool: &Path, label: &str) -> usize {
            let object = ObjectFile::<F>::parse(data, Source::new(obj)).unwrap();
            let lookup = LineLookup::parse(&object).unwrap();
            assert!(
                lookup.problems().is_empty(),
                "{label}: {:?}",
                lookup.problems()
            );
            assert!(!lookup.is_empty(), "{label}: no line information found");
            let code: Vec<_> = object
                .elf()
                .enumerate_sections()
                .filter(|(_, h)| h.sh_flags & 0x4 != 0 && h.sh_size != 0) // SHF_EXECINSTR
                .collect();
            let mut compared = 0;
            for &(index, header) in &code {
                let name =
                    String::from_utf8_lossy(object.section_name(&header).unwrap()).into_owned();
                let offsets: Vec<u64> = (0..header.sh_size).step_by(3).collect();
                // GNU addr2line cannot read some valid DWARF (for example
                // gcc's -gdwarf64 objects); fall back to llvm-addr2line
                // where addresses are unambiguous.
                let expected = match addr2line(tool, obj, Some(&name), &offsets) {
                    Some(expected) => expected,
                    None => match find_program("llvm-addr2line") {
                        Some(llvm) if code.len() == 1 => {
                            addr2line(&llvm, obj, None, &offsets).expect("llvm-addr2line")
                        }
                        _ => {
                            println!("{label}: addr2line cannot read this object, skipped");
                            return compared;
                        }
                    },
                };
                assert_eq!(expected.len(), offsets.len());
                let mut found_any = false;
                for (offset, expected) in offsets.iter().zip(expected) {
                    let actual = lookup.find(index, *offset).map(|s| (s.file, s.line));
                    found_any |= actual.is_some();
                    assert_eq!(actual, expected, "{label}: {name}+{offset:#x}");
                    compared += 1;
                }
                assert!(found_any, "{label}: nothing found in {name}");
            }
            compared
        }
        compared += if is_32 {
            check::<Elf32Le>(&data, &obj, &tool, label)
        } else {
            check::<Elf64Le>(&data, &obj, &tool, label)
        };
    }
    if built == 0 {
        return skip("no C compiler");
    }
    println!("compared {compared} positions in {built} objects");
}

/// Line lookup on the committed objects (runs without any tools). The
/// expected values come from `addr2line` 2.46.
#[test]
fn line_lookup_committed_fixtures() {
    use qld::debug::dwarf::{LineLookup, source_location};

    let data = std::fs::read(data_dir().join("lines-dwarf5.o")).unwrap();
    let object =
        ObjectFile::<Elf64Le>::parse(&data, Source::new(Path::new("lines-dwarf5.o"))).unwrap();
    let section = |name: &[u8]| object.elf().section_by_name(name).unwrap().0;
    let lookup = LineLookup::parse(&object).unwrap();
    assert!(lookup.problems().is_empty());
    let at = |section, offset| lookup.find(section, offset).map(|s| (s.file, s.line));
    assert_eq!(
        at(section(b".text.compute"), 0x10),
        Some(("/src/lines.c".into(), 9))
    );
    assert_eq!(
        at(section(b".text.generated"), 0),
        Some(("/src/generated.y".into(), 500))
    );
    assert_eq!(at(section(b".debug_info"), 0), None);
    let one_shot = source_location(&object, section(b".text.generated"), 0)
        .unwrap()
        .unwrap();
    assert_eq!(
        (one_shot.file.as_str(), one_shot.line),
        ("/src/generated.y", 500)
    );

    let data = std::fs::read(data_dir().join("lines-dwarf4-zlib.o")).unwrap();
    let object =
        ObjectFile::<Elf64Le>::parse(&data, Source::new(Path::new("lines-dwarf4-zlib.o"))).unwrap();
    let text = object.elf().section_by_name(b".text").unwrap().0;
    let lookup = LineLookup::parse(&object).unwrap();
    assert!(lookup.problems().is_empty());
    let at = |offset| lookup.find(text, offset).map(|s| (s.file, s.line));
    assert_eq!(at(0x30), Some(("/src/lines.c".into(), 7)));
    assert_eq!(at(0x90), Some(("/src/lines.c".into(), 11)));
}

// ---------------------------------------------------------------------------
// Randomized corruption: nothing may panic
// ---------------------------------------------------------------------------

/// Multiplier for the number of mutations (`QLD_FUZZ_SCALE`, default 1),
/// for longer runs by hand.
fn fuzz_scale() -> usize {
    std::env::var("QLD_FUZZ_SCALE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1)
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

/// Truncates or flips bytes of `valid`, `rounds` times, handing each
/// mutant to `check`.
fn mutate(valid: &[u8], rounds: usize, seed: u64, mut check: impl FnMut(&[u8])) {
    let mut rng = Rng(seed | 1);
    for round in 0..rounds {
        let mut data = valid.to_vec();
        match round % 4 {
            0 => data.truncate(rng.below(valid.len() + 1)),
            1 => {
                let at = rng.below(data.len());
                if let Some(b) = data.get_mut(at) {
                    *b ^= 1 << rng.below(8);
                }
            }
            2 => {
                for _ in 0..1 + rng.below(8) {
                    let at = rng.below(data.len());
                    if let Some(b) = data.get_mut(at) {
                        *b = rng.next() as u8;
                    }
                }
            }
            _ => {
                let at = rng.below(data.len());
                if let Some(b) = data.get_mut(at) {
                    *b ^= rng.next() as u8 | 1;
                }
                data.truncate(at + rng.below(data.len() - at + 1));
            }
        }
        check(&data);
    }
}

#[test]
fn inflate_survives_corruption() {
    let expected = std::fs::read(data_dir().join("mixed.bin")).unwrap();
    let mut streams = vec![std::fs::read(data_dir().join("mixed.z")).unwrap()];
    for level in [0, 1, 6, 9] {
        streams.push(zlib_compress_chunked(
            &expected[..8000],
            Level::new(level),
            3000,
        ));
    }
    let mut out = vec![0u8; expected.len()];
    for (i, stream) in streams.iter().enumerate() {
        let size = if i == 0 { expected.len() } else { 8000 };
        mutate(
            stream,
            1500 * fuzz_scale(),
            0xdead_beef + i as u64,
            |data| {
                let _ = zlib_decompress_into(data, &mut out[..size]);
                let _ = qld::debug::compress::inflate_into(
                    data.get(2..).unwrap_or_default(),
                    &mut out[..size],
                );
            },
        );
    }
    // Pure noise.
    let mut rng = Rng(12345);
    for _ in 0..500 * fuzz_scale() {
        let len = rng.below(300);
        let mut data: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
        if data.len() >= 2 {
            data[0] = 0x78;
            data[1] = 0x9c;
        }
        let _ = zlib_decompress_into(&data, &mut out[..1000]);
    }
}

#[test]
fn zstd_survives_corruption() {
    let expected = std::fs::read(data_dir().join("mixed.bin")).unwrap();
    let mut out = vec![0u8; expected.len()];
    for (i, name) in ["mixed.l3.zst", "mixed.l19.zst"].iter().enumerate() {
        let stream = std::fs::read(data_dir().join(name)).unwrap();
        mutate(&stream, 3000 * fuzz_scale(), 0x5eed + i as u64, |data| {
            let _ = Codec::Zstd.decompress_into(data, &mut out);
        });
    }
    let mut rng = Rng(777);
    for _ in 0..1000 * fuzz_scale() {
        let len = rng.below(200);
        let mut data: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
        if data.len() >= 6 {
            data[..4].copy_from_slice(&0xfd2f_b528u32.to_le_bytes());
            data[4] &= 0xf7; // clear the reserved bit, keep the rest random
        }
        let _ = Codec::Zstd.decompress_into(&data, &mut out[..500]);
    }
}

/// Both compressors round-trip randomly structured inputs (runs, repeats
/// at random distances, noise) at every setting.
#[test]
fn compressors_roundtrip_random_inputs() {
    use qld::debug::compress::zstd::zstd_compress_chunked;

    let mut rng = Rng(0x1234_5678);
    for round in 0..150 * fuzz_scale() {
        let len = rng.below(if round % 10 == 0 { 300_000 } else { 5_000 });
        let mut data = Vec::with_capacity(len);
        while data.len() < len {
            match rng.below(4) {
                0 => {
                    let byte = rng.next() as u8;
                    let n = 1 + rng.below(300);
                    data.extend(std::iter::repeat_n(byte, n));
                }
                1 if !data.is_empty() => {
                    let distance = 1 + rng.below(data.len());
                    let n = 3 + rng.below(400);
                    for _ in 0..n {
                        data.push(data[data.len() - distance]);
                    }
                }
                _ => {
                    let n = 1 + rng.below(50);
                    let alphabet = 1 + rng.below(256) as u64;
                    data.extend((0..n).map(|_| (rng.next() % alphabet) as u8));
                }
            }
        }
        data.truncate(len);
        let mut out = vec![0u8; len];
        let level = Level::new(rng.below(10) as u8);
        let chunk = [1 << 20, 1000, 70_000][rng.below(3)];
        let z = zlib_compress_chunked(&data, level, chunk);
        zlib_decompress_into(&z, &mut out).unwrap();
        assert!(out == data, "zlib round {round}");
        let z = zstd_compress_chunked(&data, [1 << 21, 777, 150_000][rng.below(3)]);
        Codec::Zstd.decompress_into(&z, &mut out).unwrap();
        assert!(out == data, "zstd round {round}");
    }
}

/// Corrupts the debug and relocation sections of the committed objects
/// (the rest of the file is kept intact so that parsing reaches the DWARF
/// readers).
#[test]
fn dwarf_survives_corruption() {
    use qld::debug::dwarf::LineLookup;

    for (seed, name) in [(1u64, "lines-dwarf5.o"), (2, "lines-dwarf4-zlib.o")] {
        let original = std::fs::read(data_dir().join(name)).unwrap();
        let object = ObjectFile::<Elf64Le>::parse(&original, Source::new(Path::new(name))).unwrap();
        let ranges: Vec<(usize, usize)> = object
            .elf()
            .enumerate_sections()
            .filter(|(_, h)| {
                let n = object.section_name(h).unwrap();
                n.starts_with(b".debug") || n.starts_with(b".rela.debug")
            })
            .map(|(_, h)| (h.sh_offset as usize, h.sh_size as usize))
            .collect();
        let queries: Vec<(u32, u64)> = object
            .elf()
            .enumerate_sections()
            .filter(|(_, h)| h.sh_flags & 0x4 != 0)
            .flat_map(|(i, h)| (0..h.sh_size).step_by(7).map(move |o| (i, o)))
            .collect();
        let mut rng = Rng(seed * 0x9e37_79b9);
        for round in 0..3000 * fuzz_scale() {
            let mut data = original.clone();
            let (start, size) = ranges[rng.below(ranges.len())];
            for _ in 0..1 + rng.below(if round % 2 == 0 { 2 } else { 16 }) {
                let at = start + rng.below(size);
                data[at] = match rng.below(3) {
                    0 => data[at] ^ (1 << rng.below(8)),
                    1 => rng.next() as u8,
                    _ => [0, 0xff, 0x80, 0x7f][rng.below(4)],
                };
            }
            let Ok(object) = ObjectFile::<Elf64Le>::parse(&data, Source::new(Path::new(name)))
            else {
                continue;
            };
            if let Ok(lookup) = LineLookup::parse(&object) {
                for &(section, offset) in &queries {
                    let _ = lookup.find(section, offset);
                }
            }
        }
        // Truncated files.
        mutate(&original, 300, seed, |data| {
            if let Ok(object) = ObjectFile::<Elf64Le>::parse(data, Source::new(Path::new(name))) {
                let _ = LineLookup::parse(&object);
            }
        });
    }
}

// ---------------------------------------------------------------------------
// Tombstones
// ---------------------------------------------------------------------------

const GC_SOURCE: &str = r#"
int used_var = 1;
int unused_var = 2;
int sink;
__attribute__((noinline)) int used(int x) {
  int y = x * 3;
  for (int i = 0; i < x; i++) y += i * sink;
  sink = y;
  return y + used_var;
}
__attribute__((noinline)) int unused(int x) {
  int y = x * 5;
  for (int i = 0; i < x; i++) y ^= i * sink;
  sink = y;
  return y + unused_var;
}
void _start(void) { sink = used(sink); for (;;) ; }
"#;

/// Links an object with `ld --gc-sections` and checks that every relocation
/// from a debug section into a collected section holds the value
/// `Style::Gnu` predicts, then that the lld rules differ exactly where
/// documented.
#[test]
fn tombstones_match_gnu_ld() {
    use qld::debug::tombstone::{DeadTarget, Style, Tombstones, truncate};
    use qld::elf::read::{ElfFile, SectionIndex};

    let Some(ld) = find_program("ld.bfd").or_else(|| find_program("ld")) else {
        return skip("GNU ld not found");
    };
    let dir = scratch_dir("tombstones");
    let gnu = Tombstones::new(Style::Gnu);
    let lld = Tombstones::new(Style::Lld);
    let mut seen_sections = std::collections::BTreeSet::new();
    for version in [2, 4, 5] {
        let flags = [
            "-g",
            &format!("-gdwarf-{version}"),
            "-gz=none",
            "-O1",
            "-fno-inline",
            "-fno-ipa-cp",
            "-fno-ipa-sra",
            "-ffunction-sections",
            "-fdata-sections",
            "-fno-asynchronous-unwind-tables",
        ];
        let Some(obj) = compile(&dir, &format!("gc{version}"), GC_SOURCE, &flags) else {
            return skip("no C compiler");
        };
        let exe = dir.join(format!("gc{version}"));
        let output = Command::new(&ld)
            .args(["--gc-sections", "--print-gc-sections", "-e", "_start", "-o"])
            .arg(&exe)
            .arg(&obj)
            .output()
            .unwrap();
        if !output.status.success() {
            return skip(format!(
                "ld failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        let removed: Vec<String> = String::from_utf8_lossy(&output.stderr)
            .lines()
            .filter_map(|l| l.split('\'').nth(1).map(str::to_string))
            .collect();
        assert!(removed.iter().any(|s| s == ".text.unused"), "{removed:?}");

        let obj_data = std::fs::read(&obj).unwrap();
        let exe_data = std::fs::read(&exe).unwrap();
        let object = ObjectFile::<Elf64Le>::parse(&obj_data, Source::new(&obj)).unwrap();
        let exe_elf = ElfFile::<Elf64Le>::parse(&exe_data, Source::new(&exe)).unwrap();
        for rel in object.relocation_sections() {
            let rel = rel.unwrap();
            let target = object.section_header(rel.target).unwrap();
            let name = String::from_utf8_lossy(object.section_name(&target).unwrap()).into_owned();
            if !name.starts_with(".debug") || name == ".debug_str" || name == ".debug_line_str" {
                continue;
            }
            let (_, out_header) = exe_elf.section_by_name(name.as_bytes()).unwrap();
            let qld::elf::read::Relocations::Rela(relas) = rel.relocations else {
                panic!("x86-64 uses RELA");
            };
            for r in relas.iter() {
                let symbol = object.symbols().get(r.symbol as usize).unwrap();
                let SectionIndex::Section(index) = symbol.section else {
                    continue;
                };
                let section = object.section_header(index).unwrap();
                let section_name = String::from_utf8_lossy(object.section_name(&section).unwrap());
                if !removed.iter().any(|s| *s == section_name) {
                    continue;
                }
                let width = match r.r_type {
                    1 => 8,       // R_X86_64_64
                    10 | 11 => 4, // R_X86_64_32, R_X86_64_32S
                    other => panic!("unexpected relocation type {other} in {name}"),
                };
                let at = (out_header.sh_offset + r.offset) as usize;
                let mut bytes = [0u8; 8];
                bytes[..width].copy_from_slice(&exe_data[at..at + width]);
                let actual = u64::from_le_bytes(bytes);
                let expected = truncate(
                    gnu.value(name.as_bytes(), DeadTarget::Discarded).unwrap(),
                    width,
                );
                assert_eq!(
                    actual, expected,
                    "DWARF {version}: {name}+{:#x} against {section_name}",
                    r.offset
                );
                seen_sections.insert(name.clone());
            }
        }
    }
    println!("GNU ld tombstones verified in {seen_sections:?}");
    assert!(seen_sections.contains(".debug_ranges"));
    assert!(seen_sections.contains(".debug_loc"));
    assert!(seen_sections.contains(".debug_info"));
    // Where lld differs from GNU ld.
    assert_eq!(gnu.value(b".debug_loc", DeadTarget::Discarded), Some(0));
    assert_eq!(lld.value(b".debug_loc", DeadTarget::Discarded), Some(1));
}

// ---------------------------------------------------------------------------
// zstd
// ---------------------------------------------------------------------------

/// Compresses every corpus with the `zstd` command at a range of levels and
/// options, and checks that qld's decoder reproduces the input.
#[test]
fn zstd_matches_system_zstd() {
    let Some(zstd) = find_program("zstd") else {
        return skip("zstd not found");
    };
    let dir = scratch_dir("zstd-cli");
    let variants: &[(&str, &[&str])] = &[
        ("fast5", &["--fast=5"]),
        ("l1", &["-1"]),
        ("l3", &["-3"]),
        ("l3nocheck", &["-3", "--no-check"]),
        ("l9", &["-9"]),
        ("l19", &["-19"]),
        ("ultra22", &["--ultra", "-22"]),
        ("long", &["-3", "--long=24"]),
        ("nosize", &["-3", "--no-content-size"]),
    ];
    let mut checked = 0;
    for (name, data) in corpora() {
        let raw = dir.join(format!("{name}.bin"));
        std::fs::write(&raw, &data).unwrap();
        for (tag, args) in variants {
            if matches!(*tag, "l19" | "ultra22") && data.len() > 400_000 {
                continue; // slow to compress; covered by the smaller corpora
            }
            let packed = dir.join(format!("{name}.{tag}.zst"));
            let status = Command::new(&zstd)
                .args(*args)
                .args(["-q", "-f", "-o"])
                .arg(&packed)
                .arg(&raw)
                .status()
                .unwrap();
            assert!(status.success(), "zstd {args:?} failed");
            let compressed = std::fs::read(&packed).unwrap();
            let mut out = vec![0u8; data.len()];
            if let Err(error) = Codec::Zstd.decompress_into(&compressed, &mut out) {
                panic!("{name}.{tag}: {error}");
            }
            assert!(out == data, "{name}.{tag}: output differs");
            checked += 1;
        }
    }
    println!("checked {checked} zstd streams");
}

/// qld's zstd output decompresses with the `zstd` command (libzstd), for
/// every corpus and several frame sizes.
#[test]
fn zstd_output_accepted_by_system_zstd() {
    use qld::debug::compress::zstd::zstd_compress_chunked;

    let Some(zstd) = find_program("zstd") else {
        return skip("zstd not found");
    };
    let dir = scratch_dir("zstd-output");
    let mut checked = 0;
    for (name, data) in corpora() {
        for chunk in [4096, 200_000, 1 << 21] {
            let z = zstd_compress_chunked(&data, chunk);
            let path = dir.join(format!("{name}.{chunk}.zst"));
            std::fs::write(&path, &z).unwrap();
            let out = run(Command::new(&zstd).args(["-d", "-c", "-q"]).arg(&path))
                .unwrap_or_else(|| panic!("zstd rejected {name} (chunk {chunk})"));
            assert!(out == data, "{name} (chunk {chunk}): zstd output differs");
            checked += 1;
        }
    }
    println!("zstd accepted {checked} streams");
}

/// The committed zstd fixtures decode (runs without any tools).
#[test]
fn zstd_committed_fixtures() {
    let expected = std::fs::read(data_dir().join("mixed.bin")).unwrap();
    for name in ["mixed.l3.zst", "mixed.l19.zst"] {
        let compressed = std::fs::read(data_dir().join(name)).unwrap();
        let mut out = vec![0u8; expected.len()];
        Codec::Zstd.decompress_into(&compressed, &mut out).unwrap();
        assert_eq!(out, expected, "{name}");
    }
}

/// Several frames in a row, including a skippable frame, decode as one.
#[test]
fn zstd_concatenated_frames() {
    let Some(zstd) = find_program("zstd") else {
        return skip("zstd not found");
    };
    let dir = scratch_dir("zstd-frames");
    let parts = [noise(5000, 1), b"hello ".repeat(3000), vec![0u8; 70_000]];
    let mut stream = Vec::new();
    let mut expected = Vec::new();
    for (i, part) in parts.iter().enumerate() {
        let raw = dir.join(format!("part{i}"));
        std::fs::write(&raw, part).unwrap();
        let packed = run(Command::new(&zstd).args(["-q", "-c", "-5"]).arg(&raw)).unwrap();
        stream.extend_from_slice(&packed);
        expected.extend_from_slice(part);
        // A skippable frame between frames.
        stream.extend_from_slice(&0x184d_2a53u32.to_le_bytes());
        stream.extend_from_slice(&3u32.to_le_bytes());
        stream.extend_from_slice(b"abc");
    }
    let mut out = vec![0u8; expected.len()];
    Codec::Zstd.decompress_into(&stream, &mut out).unwrap();
    assert!(out == expected);
    let mut short = vec![0u8; expected.len() - 1];
    assert!(Codec::Zstd.decompress_into(&stream, &mut short).is_err());
    let mut long = vec![0u8; expected.len() + 1];
    assert!(Codec::Zstd.decompress_into(&stream, &mut long).is_err());
}

// ---------------------------------------------------------------------------
// Throughput benchmarks (`cargo test --release --test debug -- --ignored
// --nocapture bench_`). No framework: they print MB/s.
// ---------------------------------------------------------------------------

/// A benchmark corpus: real debug-heavy bytes (this test executable,
/// repeated) up to `size` bytes, or the file named by `QLD_BENCH_FILE`.
fn bench_corpus(size: usize) -> Vec<u8> {
    if let Some(path) = std::env::var_os("QLD_BENCH_FILE") {
        return std::fs::read(path).expect("read QLD_BENCH_FILE");
    }
    let size = std::env::var("QLD_BENCH_MB")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .map_or(size, |mb| mb << 20);
    let exe = std::fs::read(std::env::current_exe().unwrap()).unwrap();
    let mut out = Vec::with_capacity(size);
    while out.len() < size {
        let take = (size - out.len()).min(exe.len());
        out.extend_from_slice(&exe[..take]);
    }
    out
}

fn mb_per_s(bytes: usize, elapsed: std::time::Duration) -> f64 {
    bytes as f64 / (1 << 20) as f64 / elapsed.as_secs_f64()
}

/// Best of `rounds` runs.
fn best_of(rounds: usize, mut f: impl FnMut()) -> std::time::Duration {
    (0..rounds)
        .map(|_| {
            let start = std::time::Instant::now();
            f();
            start.elapsed()
        })
        .min()
        .unwrap()
}

#[test]
#[ignore = "benchmark"]
fn bench_deflate() {
    let data = bench_corpus(64 << 20);
    let python = find_program("python3");
    let dir = scratch_dir("bench-deflate");
    let raw = dir.join("corpus.bin");
    std::fs::write(&raw, &data).unwrap();
    for level in [1u8, 6, 9] {
        let lvl = Level::new(level);
        let serial = rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build()
            .unwrap();
        let mut z = Vec::new();
        let one = best_of(2, || z = serial.install(|| zlib_compress(&data, lvl)));
        let mut zp = Vec::new();
        let all = best_of(3, || zp = zlib_compress(&data, lvl));
        assert!(z == zp, "output depends on thread count");
        let mut back = vec![0u8; data.len()];
        zlib_decompress_into(&z, &mut back).unwrap();
        assert!(back == data);
        let system = python.as_ref().and_then(|python| {
            let script = format!(
                "import zlib,sys,time\nd=open(sys.argv[1],'rb').read()\nt=time.perf_counter()\nz=zlib.compress(d,{level})\nt=time.perf_counter()-t\nprint(len(d)/1048576/t, len(z))"
            );
            let out = run(Command::new(python).arg("-c").arg(&script).arg(&raw))?;
            let text = String::from_utf8_lossy(&out).into_owned();
            let mut words = text.split_whitespace();
            let speed: f64 = words.next()?.parse().ok()?;
            let size: usize = words.next()?.parse().ok()?;
            Some((speed, size))
        });
        let (sys_speed, sys_size) = system.unwrap_or((0.0, 0));
        println!(
            "deflate level {level}: qld 1 thread {:.0} MB/s, {} threads {:.0} MB/s, ratio {:.3}; system zlib (1 thread) {:.0} MB/s, ratio {:.3}",
            mb_per_s(data.len(), one),
            rayon::current_num_threads(),
            mb_per_s(data.len(), all),
            z.len() as f64 / data.len() as f64,
            sys_speed,
            sys_size as f64 / data.len() as f64,
        );
    }
}

#[test]
#[ignore = "benchmark"]
fn bench_zstd() {
    let Some(zstd) = find_program("zstd") else {
        return skip("zstd not found");
    };
    let data = bench_corpus(64 << 20);
    let dir = scratch_dir("bench-zstd");
    let raw = dir.join("corpus.bin");
    std::fs::write(&raw, &data).unwrap();
    let levels: Vec<u32> = std::env::var("QLD_BENCH_LEVELS")
        .ok()
        .map(|v| v.split(',').filter_map(|l| l.parse().ok()).collect())
        .unwrap_or_else(|| vec![1, 3, 9, 19]);
    for level in levels {
        let packed = dir.join(format!("corpus.{level}.zst"));
        let ok = Command::new(&zstd)
            .args(["-q", "-f", "-T0", &format!("-{level}"), "-o"])
            .arg(&packed)
            .arg(&raw)
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            return skip("zstd failed");
        }
        let z = std::fs::read(&packed).unwrap();
        let mut buf = vec![0u8; data.len()];
        let elapsed = best_of(5, || Codec::Zstd.decompress_into(&z, &mut buf).unwrap());
        assert!(buf == data);
        // The reference decoder's in-memory speed: `zstd -b` on the file.
        let reference = run(Command::new(&zstd)
            .args(["-b", &format!("-{level}"), "-i1"])
            .arg(&raw))
        .map(|out| String::from_utf8_lossy(&out).into_owned())
        .unwrap_or_default();
        // Progress lines are separated by '\r'; the decompression speed is
        // the second "MB/s" figure of the last complete one.
        let reference = reference
            .split(['\r', '\n'])
            .rfind(|line| line.matches("MB/s").count() == 2)
            .and_then(|line| line.rsplit(',').next())
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        println!(
            "zstd level {level}: {:.1} MiB -> {:.1} MiB, qld {:.0} MB/s, libzstd (zstd -b) {reference}",
            z.len() as f64 / 1048576.0,
            data.len() as f64 / 1048576.0,
            mb_per_s(data.len(), elapsed),
        );
    }
}

#[test]
#[ignore = "benchmark"]
fn bench_zstd_compress() {
    use qld::debug::compress::zstd::zstd_compress;

    let data = bench_corpus(64 << 20);
    let serial = rayon::ThreadPoolBuilder::new()
        .num_threads(1)
        .build()
        .unwrap();
    let mut z = Vec::new();
    let one = best_of(2, || z = serial.install(|| zstd_compress(&data)));
    let mut zp = Vec::new();
    let all = best_of(3, || zp = zstd_compress(&data));
    assert!(z == zp, "output depends on thread count");
    let mut back = vec![0u8; data.len()];
    Codec::Zstd.decompress_into(&z, &mut back).unwrap();
    assert!(back == data);
    println!(
        "zstd compress: qld 1 thread {:.0} MB/s, {} threads {:.0} MB/s, ratio {:.3}",
        mb_per_s(data.len(), one),
        rayon::current_num_threads(),
        mb_per_s(data.len(), all),
        z.len() as f64 / data.len() as f64,
    );
    let Some(zstd) = find_program("zstd") else {
        return skip("zstd not found");
    };
    let dir = scratch_dir("bench-zstd-compress");
    let raw = dir.join("corpus.bin");
    std::fs::write(&raw, &data).unwrap();
    for level in [1, 3] {
        // `zstd -b` reports in-memory compression speed and ratio.
        let text = run(Command::new(&zstd)
            .args(["-b", &format!("-{level}"), "-i2", "-T1"])
            .arg(&raw))
        .map(|out| String::from_utf8_lossy(&out).into_owned())
        .unwrap_or_default();
        let line = text
            .split(['\r', '\n'])
            .rfind(|line| line.matches("MB/s").count() == 2)
            .unwrap_or_default()
            .trim()
            .to_string();
        println!("  libzstd -{level} (1 thread): {line}");
    }
}

#[test]
#[ignore = "benchmark"]
fn bench_inflate() {
    let Some(python) = find_program("python3") else {
        return skip("python3 not found");
    };
    let data = bench_corpus(64 << 20);
    let dir = scratch_dir("bench-inflate");
    let raw = dir.join("corpus.bin");
    std::fs::write(&raw, &data).unwrap();
    for level in [1, 6, 9] {
        let script = format!(
            "import zlib,sys,time\nd=open(sys.argv[1],'rb').read()\nz=zlib.compress(d,{level})\nopen(sys.argv[2],'wb').write(z)\nb=min((lambda t:(zlib.decompress(z),time.perf_counter()-t)[1])(time.perf_counter()) for _ in range(3))\nprint(len(d)/1048576/b)"
        );
        let zpath = dir.join(format!("corpus.{level}.z"));
        let Some(out) = run(Command::new(&python)
            .arg("-c")
            .arg(&script)
            .arg(&raw)
            .arg(&zpath))
        else {
            return skip("python3 zlib failed");
        };
        let system: f64 = String::from_utf8_lossy(&out).trim().parse().unwrap_or(0.0);
        let z = std::fs::read(&zpath).unwrap();
        let mut buf = vec![0u8; data.len()];
        let elapsed = best_of(5, || zlib_decompress_into(&z, &mut buf).unwrap());
        assert!(buf == data);
        println!(
            "inflate level {level}: {:.1} MiB -> {:.1} MiB, qld {:.0} MB/s, system zlib {:.0} MB/s",
            z.len() as f64 / 1048576.0,
            data.len() as f64 / 1048576.0,
            mb_per_s(data.len(), elapsed),
            system
        );
    }
}
