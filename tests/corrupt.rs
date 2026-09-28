//! Randomized corruption of every committed fixture (workstream W51).
//!
//! `cargo fuzz` needs a second crate, and qld is one crate (see `CLAUDE.md`),
//! so this stands in for the M0 fuzzing item. It takes the binary fixtures
//! under `tests/data/` — ELF objects and shared objects, `ar` archives,
//! PE/COFF objects, import libraries and a DLL, Mach-O objects, dylibs and
//! fat files, `.tbd` stubs, and linker scripts — mutates them with a seeded
//! generator, and runs the mutated bytes through the readers and the link
//! drivers.
//!
//! The only assertion is that qld never panics, never hangs and never grows
//! without bound: every mutation must come back as `Ok` or as an
//! [`qld::Error`]. A failure prints the seed, the fixture and the mutations
//! that produced it, so it reproduces exactly.
//!
//! - `QLD_CORRUPT_SEEDS`: comma-separated seeds, replacing the fixed set CI
//!   runs (`QLD_CORRUPT_SEEDS=0..0` is not a range; list them).
//! - `QLD_CORRUPT_ITERS`: mutations per fixture per seed (default 24).
//! - `QLD_CORRUPT_LIST`: print the corpus and what each file was taken for.
//!
//! A long local run: `QLD_CORRUPT_ITERS=2000 QLD_CORRUPT_SEEDS=1,2,3,4,5,6,7,8
//! cargo test --test corrupt --release -- --test-threads=2`.

#[path = "../examples/support/objects.rs"]
mod objects;

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use qld::args::{InputAttrs, InputKind, LinkOptions, OutputBuffer, OutputKind};
use qld::diag::Collect;
use qld::target::{Architecture, BinaryFormat, Endianness, OperatingSystem, PointerWidth, Target};

// ---------------------------------------------------------------------------
// Seeded generator
// ---------------------------------------------------------------------------

/// xorshift64*, the generator the other corruption tests use. Seeded, so a
/// failure reproduces from the seed alone.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        // 0 is a fixed point of xorshift.
        Self(seed | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

/// Values that break a length, a count or an offset: zero, one past every
/// signed and unsigned boundary, and the sizes qld multiplies them by.
const ABSURD: [u64; 16] = [
    0,
    1,
    0x7f,
    0x80,
    0xff,
    0x7fff,
    0x8000,
    0xffff,
    0x7fff_ffff,
    0x8000_0000,
    0xffff_fffe,
    0xffff_ffff,
    0x7fff_ffff_ffff_ffff,
    0x8000_0000_0000_0000,
    0xffff_ffff_ffff_fff0,
    0xffff_ffff_ffff_ffff,
];

/// One change to a copy of a fixture, recorded so a failure can be replayed
/// by eye.
// The fields are read only through `Debug`, in the failure message.
#[allow(dead_code)]
#[derive(Debug)]
enum Mutation {
    /// Flip one bit.
    Bit { at: usize, bit: u32 },
    /// Store one byte.
    Byte { at: usize, value: u8 },
    /// Store an absurd little- or big-endian integer, which is how a count,
    /// a length or a file offset goes wrong.
    Int {
        at: usize,
        width: usize,
        big_endian: bool,
        value: u64,
    },
    /// Cut the file short.
    Truncate { len: usize },
    /// Zero a range: a table that is suddenly all zero.
    Zero { at: usize, len: usize },
    /// Copy one range over another, which makes structures point into each
    /// other and produces overlapping and cyclic ranges.
    Overlap { from: usize, to: usize, len: usize },
}

/// Chooses an offset, biased towards the headers and the end, where the
/// tables that drive every parser live.
fn offset(rng: &mut Rng, len: usize) -> usize {
    match rng.below(4) {
        0 => rng.below(len.min(128)),
        1 => len.saturating_sub(1 + rng.below((len / 4).max(1))),
        _ => rng.below(len),
    }
}

/// Applies one random mutation to `data` and returns what it did.
fn mutate(rng: &mut Rng, data: &mut Vec<u8>) -> Mutation {
    if data.is_empty() {
        data.push(rng.next() as u8);
        return Mutation::Byte {
            at: 0,
            value: data[0],
        };
    }
    let len = data.len();
    match rng.below(10) {
        0 => {
            let at = offset(rng, len);
            let bit = rng.below(8) as u32;
            data[at] ^= 1 << bit;
            Mutation::Bit { at, bit }
        }
        1 | 2 => {
            let at = offset(rng, len);
            let value = match rng.below(4) {
                0 => 0,
                1 => 0xff,
                2 => 0x7f,
                _ => rng.next() as u8,
            };
            data[at] = value;
            Mutation::Byte { at, value }
        }
        3..=5 => {
            let width = *rng.pick(&[2usize, 4, 8]);
            let at = offset(rng, len).min(len.saturating_sub(width));
            let value = *rng.pick(&ABSURD);
            let big_endian = rng.below(2) == 0;
            let bytes = if big_endian {
                value.to_be_bytes()
            } else {
                value.to_le_bytes()
            };
            let source = if big_endian {
                &bytes[8 - width..]
            } else {
                &bytes[..width]
            };
            if let Some(slot) = data.get_mut(at..at + width) {
                slot.copy_from_slice(source);
            }
            Mutation::Int {
                at,
                width,
                big_endian,
                value,
            }
        }
        6 => {
            let new_len = rng.below(len + 1);
            data.truncate(new_len);
            Mutation::Truncate { len: new_len }
        }
        7 => {
            let at = offset(rng, len);
            let count = rng.below(len - at).max(1);
            data[at..at + count].fill(0);
            Mutation::Zero { at, len: count }
        }
        _ => {
            let count = rng.below(len / 2 + 1).max(1);
            let from = rng.below(len - count + 1);
            let to = rng.below(len - count + 1);
            let slice = data[from..from + count].to_vec();
            data[to..to + count].copy_from_slice(&slice);
            Mutation::Overlap {
                from,
                to,
                len: count,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The corpus
// ---------------------------------------------------------------------------

/// What a fixture is taken for, which decides how it is fed back in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Elf,
    Coff,
    MachO,
    Archive,
    /// A linker script, a version script, a module-definition file or a
    /// `.tbd` stub: text that several front ends will try to read.
    Text,
}

/// Classifies a file by its magic, not its name: a mutated file keeps its
/// classification, so the same parser sees it before and after.
fn classify(data: &[u8], path: &Path) -> Option<Kind> {
    let head = data.get(..8)?;
    if head.starts_with(b"\x7fELF") {
        return Some(Kind::Elf);
    }
    if head.starts_with(b"!<arch>\n") || head.starts_with(b"!<thin>\n") {
        return Some(Kind::Archive);
    }
    let magic = u32::from_le_bytes([head[0], head[1], head[2], head[3]]);
    // Mach-O thin (32- and 64-bit, both byte orders) and fat.
    if matches!(
        magic,
        0xfeed_face | 0xfeed_facf | 0xcefa_edfe | 0xcffa_edfe | 0xbeba_feca | 0xcafe_babe
    ) {
        return Some(Kind::MachO);
    }
    if head.starts_with(b"MZ") || head.starts_with(b"\x00\x00\xff\xff") {
        return Some(Kind::Coff);
    }
    // A bare COFF object starts with its machine type.
    let machine = u16::from_le_bytes([head[0], head[1]]);
    if matches!(machine, 0x014c | 0x8664 | 0xaa64 | 0x01c0 | 0x01c4 | 0x0200) {
        return Some(Kind::Coff);
    }
    // Text inputs: linker and version scripts, module-definition files and
    // `.tbd` stubs. Several front ends will try to read each of them, and
    // the one that rejects it matters as much as the one that accepts it.
    let extension = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let named = matches!(
        extension,
        "ld" | "x" | "t" | "tbd" | "yaml" | "ver" | "map" | "def" | "rsp"
    );
    let looks_like_script = [
        &b"/*"[..],
        b"--- !tapi",
        b"OUTPUT_FORMAT",
        b"OUTPUT_ARCH",
        b"GROUP",
        b"INPUT",
        b"SECTIONS",
        b"ENTRY",
        b"VERSION",
        b"EXPORTS",
        b"LIBRARY",
        b"{",
    ]
    .iter()
    .any(|prefix| data.starts_with(prefix));
    if named || looks_like_script {
        return Some(Kind::Text);
    }
    None
}

/// One fixture, read once and reused for every seed.
struct Fixture {
    name: String,
    kind: Kind,
    data: Vec<u8>,
}

fn data_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data")
}

/// Every committed fixture that a parser could be handed, plus the objects
/// and archive `examples/support/objects.rs` builds, so the ELF link path is
/// covered on a machine with no fixtures of its own.
fn corpus() -> Vec<Fixture> {
    let mut files = Vec::new();
    collect(&data_dir(), &mut files);
    files.sort();
    let mut corpus: Vec<Fixture> = files
        .iter()
        .filter_map(|path| {
            let data = std::fs::read(path).ok()?;
            let kind = classify(&data, path)?;
            let name = path
                .strip_prefix(data_dir())
                .unwrap_or(path)
                .to_string_lossy()
                .into_owned();
            Some(Fixture { name, kind, data })
        })
        .collect();
    corpus.push(Fixture {
        name: "<built>/main.o".into(),
        kind: Kind::Elf,
        data: objects::main_object(),
    });
    corpus.push(Fixture {
        name: "<built>/answer.o".into(),
        kind: Kind::Elf,
        data: objects::answer_object(42),
    });
    corpus.push(Fixture {
        name: "<built>/libanswer.a".into(),
        kind: Kind::Archive,
        data: objects::archive(&[("answer.o", &objects::answer_object(7), &["answer"])]),
    });
    corpus
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, out);
        } else if path.is_file() {
            out.push(path);
        }
    }
}

// ---------------------------------------------------------------------------
// Feeding a mutated fixture back in
// ---------------------------------------------------------------------------

fn target(arch: Architecture, format: BinaryFormat, os: OperatingSystem) -> Target {
    Target {
        arch,
        os,
        format,
        endian: Endianness::Little,
        pointer_width: match arch {
            Architecture::X86 | Architecture::Arm => PointerWidth::Bits32,
            _ => PointerWidth::Bits64,
        },
    }
}

/// Links `inputs` in memory, writing nothing and printing nothing.
fn link(inputs: Vec<(&str, Vec<u8>)>, format: Option<Target>, kind: OutputKind) {
    let mut options = LinkOptions::new();
    options.kind = kind;
    options.target = format;
    options.threads = Some(1);
    options.output = Some(PathBuf::from("corrupt-never-written"));
    options.output_buffer = Some(OutputBuffer::new());
    for (name, data) in inputs {
        options.push_input(InputKind::bytes(name, data), InputAttrs::default());
    }
    // Errors are the point; only a panic or a hang fails the test.
    let _ = qld::link(&options, &Collect::new());
}

/// Runs every parser and driver that would see a file of this kind.
fn feed(kind: Kind, name: &str, data: &[u8]) {
    match kind {
        Kind::Elf | Kind::Archive => {
            // On its own, and beside objects that resolve, so that the
            // mutated file reaches resolution, layout and relocation.
            link(vec![(name, data.to_vec())], None, OutputKind::Relocatable);
            link(
                vec![
                    ("main.o", objects::main_object()),
                    ("answer.o", objects::answer_object(42)),
                    (name, data.to_vec()),
                ],
                None,
                OutputKind::StaticExecutable,
            );
            link(vec![(name, data.to_vec())], None, OutputKind::Shared);
            if kind == Kind::Archive {
                let _ = qld::input::archive::Archive::parse(Path::new(name), data);
            }
        }
        Kind::Coff => {
            for arch in [
                Architecture::X86_64,
                Architecture::X86,
                Architecture::Aarch64,
            ] {
                link(
                    vec![(name, data.to_vec())],
                    Some(target(arch, BinaryFormat::Pe, OperatingSystem::Windows)),
                    OutputKind::Executable,
                );
            }
        }
        Kind::MachO => {
            for arch in [Architecture::Aarch64, Architecture::X86_64] {
                link(
                    vec![(name, data.to_vec())],
                    Some(target(arch, BinaryFormat::MachO, OperatingSystem::Darwin)),
                    OutputKind::Executable,
                );
            }
        }
        Kind::Text => {
            let path = Path::new(name);
            let _ = qld::script::parse_script(data, path, &mut qld::script::NoIncludes);
            let _ = qld::script::parse_version_script(data, path);
            let _ = qld::script::parse_expression(data, path);
            let _ = qld::script::parse_defsym(data);
            let _ =
                qld::coff::read::parse_module_definition(data, qld::coff::read::Source::new(path));
            // A script is also a valid input file, which is how GNU ld and
            // qld take `libc.so`; a `.tbd` stub is a Mach-O input.
            link(vec![(name, data.to_vec())], None, OutputKind::Executable);
            link(
                vec![(name, data.to_vec())],
                Some(target(
                    Architecture::Aarch64,
                    BinaryFormat::MachO,
                    OperatingSystem::Darwin,
                )),
                OutputKind::Executable,
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Watchdog: no hang, no unbounded growth
// ---------------------------------------------------------------------------

/// What the watchdog prints if a case never comes back.
static CURRENT: Mutex<String> = Mutex::new(String::new());
/// Milliseconds since the watchdog started, by which the current case must
/// be done. `u64::MAX` means nothing is running.
static DEADLINE: AtomicU64 = AtomicU64::new(u64::MAX);

/// Longest a single mutated fixture may take. Generous: a debug build on a
/// loaded machine is slow, and only a real hang should trip this.
const CASE_TIMEOUT: Duration = Duration::from_secs(120);
/// Largest resident set the whole test process may reach. A parser that
/// believes an absurd count allocates far more than this at once.
const RSS_LIMIT_BYTES: u64 = 4 << 30;

/// Starts the watchdog once. It aborts the process, naming the case, if a
/// case runs too long or the process outgrows [`RSS_LIMIT_BYTES`], because
/// a test harness cannot kill a thread that is stuck.
fn watchdog() {
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        let start = Instant::now();
        std::thread::Builder::new()
            .name("qld-corrupt-watchdog".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(Duration::from_millis(250));
                    let now = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
                    let deadline = DEADLINE.load(Ordering::Relaxed);
                    let stuck = now > deadline;
                    let big = rss_bytes().is_some_and(|rss| rss > RSS_LIMIT_BYTES);
                    if stuck || big {
                        let case = CURRENT.lock().map_or_else(
                            |poisoned| poisoned.into_inner().clone(),
                            |case| case.clone(),
                        );
                        let why = if stuck {
                            "did not finish"
                        } else {
                            "grew too large"
                        };
                        eprintln!("qld corruption harness: {case} {why}; aborting");
                        std::process::abort();
                    }
                }
            })
            .expect("watchdog thread");
        // Only the deadline of a running case matters.
        DEADLINE.store(u64::MAX, Ordering::Relaxed);
        START.set(start).ok();
    });
}

static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

fn rss_bytes() -> Option<u64> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    Some(pages.saturating_mul(4096))
}

/// Runs `case`, watched, and turns a panic into a failure that names the
/// seed and the mutations.
fn guarded(description: &str, case: impl FnOnce() + std::panic::UnwindSafe) {
    watchdog();
    if let Some(start) = START.get() {
        let now = u64::try_from(start.elapsed().as_millis()).unwrap_or(0);
        *CURRENT.lock().expect("poisoned") = description.to_owned();
        DEADLINE.store(
            now.saturating_add(u64::try_from(CASE_TIMEOUT.as_millis()).unwrap_or(u64::MAX)),
            Ordering::Relaxed,
        );
    }
    let outcome = std::panic::catch_unwind(case);
    DEADLINE.store(u64::MAX, Ordering::Relaxed);
    if let Err(panic) = outcome {
        let message = panic
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_owned()))
            .unwrap_or_else(|| "a panic with no message".to_owned());
        panic!("qld panicked on {description}: {message}");
    }
}

// ---------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------

/// The seeds CI runs, unless `QLD_CORRUPT_SEEDS` replaces them.
const SEEDS: [u64; 4] = [
    0x9e37_79b9_7f4a_7c15,
    0x0123_4567_89ab_cdef,
    0xdead_beef_cafe_f00d,
    1,
];

fn seeds() -> Vec<u64> {
    match std::env::var("QLD_CORRUPT_SEEDS") {
        Ok(value) if !value.trim().is_empty() => value
            .split(',')
            .filter_map(|seed| {
                let seed = seed.trim();
                seed.strip_prefix("0x").map_or_else(
                    || seed.parse().ok(),
                    |hex| u64::from_str_radix(hex, 16).ok(),
                )
            })
            .collect(),
        _ => SEEDS.to_vec(),
    }
}

fn iterations() -> usize {
    std::env::var("QLD_CORRUPT_ITERS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(24)
}

/// The corpus is real: every format qld reads has at least one fixture in
/// it, so a change that stops classifying one does not silently shrink the
/// harness.
#[test]
fn corpus_covers_every_format() {
    let corpus = corpus();
    if std::env::var_os("QLD_CORRUPT_LIST").is_some() {
        for fixture in &corpus {
            println!(
                "{:?}\t{} bytes\t{}",
                fixture.kind,
                fixture.data.len(),
                fixture.name
            );
        }
    }
    for kind in [
        Kind::Elf,
        Kind::Coff,
        Kind::MachO,
        Kind::Archive,
        Kind::Text,
    ] {
        let count = corpus.iter().filter(|f| f.kind == kind).count();
        assert!(count > 0, "no {kind:?} fixture in tests/data");
    }
    assert!(corpus.len() > 40, "corpus shrank to {}", corpus.len());
}

/// Every fixture, mutated once per iteration with a fresh mutation from a
/// seeded generator, must come back as `Ok` or an error.
#[test]
fn mutated_fixtures_never_panic() {
    let corpus = corpus();
    let iterations = iterations();
    for seed in seeds() {
        for fixture in &corpus {
            for round in 0..iterations {
                let mut rng = Rng::new(seed ^ (round as u64).wrapping_mul(0x9e37_79b9));
                let mut data = fixture.data.clone();
                let count = 1 + rng.below(4);
                let mut log = Vec::with_capacity(count);
                for _ in 0..count {
                    log.push(mutate(&mut rng, &mut data));
                }
                let description =
                    format!("{} (seed {seed:#x}, round {round}, {log:?})", fixture.name);
                let kind = fixture.kind;
                let name = fixture.name.clone();
                guarded(&description, move || feed(kind, &name, &data));
            }
        }
    }
}

/// Every prefix length of a few fixtures: truncation on its own finds the
/// bounds checks that a random byte never reaches.
#[test]
fn every_truncation_of_a_fixture_never_panics() {
    let corpus = corpus();
    // One of each kind, the smallest, so that every prefix is affordable.
    for kind in [
        Kind::Elf,
        Kind::Coff,
        Kind::MachO,
        Kind::Archive,
        Kind::Text,
    ] {
        let Some(fixture) = corpus
            .iter()
            .filter(|f| f.kind == kind)
            .min_by_key(|f| f.data.len())
        else {
            continue;
        };
        // Every prefix for a small file, a sample for a larger one.
        let step = (fixture.data.len() / 512).max(1);
        for len in (0..=fixture.data.len()).step_by(step) {
            let data = fixture.data[..len].to_vec();
            let description = format!("{} truncated to {len}", fixture.name);
            let name = fixture.name.clone();
            guarded(&description, move || feed(kind, &name, &data));
        }
    }
}
