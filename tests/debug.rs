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

use qld::debug::compress::{Codec, zlib_decompress_into};

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
// Throughput benchmarks (`cargo test --release --test debug -- --ignored
// --nocapture bench_`). No framework: they print MB/s.
// ---------------------------------------------------------------------------

/// A benchmark corpus: real debug-heavy bytes (this test executable,
/// repeated) up to `size` bytes, or the file named by `QLD_BENCH_FILE`.
fn bench_corpus(size: usize) -> Vec<u8> {
    if let Some(path) = std::env::var_os("QLD_BENCH_FILE") {
        return std::fs::read(path).expect("read QLD_BENCH_FILE");
    }
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
