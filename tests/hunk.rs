//! AmigaOS Hunk output tests (workstream W55).
//!
//! **vlink is the oracle.** `tests/data/hunk/` holds a small corpus: for
//! every case, the m68k assembly sources, the ELF objects vasm produced
//! from them (`vasmm68k_mot -Felf`), and the load file vlink produced from
//! those objects (`vlink -b amigahunk`). `MANIFEST.txt` records the tool
//! versions and every command that ran, and `generate.sh` regenerates the
//! lot (with `--build` it fetches and builds both tools first).
//!
//! The corpus is committed, so these tests need neither tool:
//!
//! - [`matches_vlink_reference`] links the committed objects with qld and
//!   compares its load file with vlink's **byte for byte**. On a mismatch
//!   it prints a structural diff first (hunk kinds, allocation sizes,
//!   block sizes, relocation tables and symbol tables), because that says
//!   what differs; the assertion itself is on the bytes.
//! - [`rejects_cross_hunk_pcrel`] checks that a PC-relative reference
//!   between two hunks is an error, as it is in vlink: the hunks of a load
//!   file are relocated independently.
//! - [`deterministic`] relinks each case on one and on four threads and
//!   requires the same bytes.
//!
//! [`live_tools_agree`] is the mode that keeps the corpus from drifting:
//! when vasm and vlink are available it reassembles the sources, relinks
//! with vlink into a scratch directory, and checks that the committed
//! objects and load files are exactly what the tools produce today, then
//! compares qld against the freshly built reference. It skips when either
//! tool is missing, unless `QLD_REQUIRE_HUNK_TOOLS` is set to something
//! other than `0` — which is what the CI job that builds the tools does.
//!
//! Tools are found through `$VASM` and `$VLINK`, then `vasmm68k_mot` and
//! `vlink` in `PATH`, then `$CACHE`/`$TMPDIR`'s `qld-hunk-tools` build
//! directory, which is where `generate.sh --build` puts them.
//!
//! Nothing is executed: there is no m68k emulator here by design, and a
//! structural and byte-for-byte comparison with vlink is the standard.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use qld::hunk::format::{self, Kind, LoadFile};

const DATA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/hunk");

/// One corpus case, as `link.txt` describes it.
struct Case {
    name: String,
    dir: PathBuf,
    /// Inputs in link order, as file names inside `dir`.
    objects: Vec<String>,
    /// Extra flags both linkers get.
    flags: Vec<String>,
    /// Archives to build before linking: (archive, members).
    archives: Vec<(String, Vec<String>)>,
    /// What qld's diagnostic must say, for a case qld rejects.
    error: Option<String>,
    /// Whether vlink accepts the case qld rejects: a documented
    /// difference, not a corpus error.
    vlink_accepts: bool,
}

fn cases() -> Vec<Case> {
    let mut cases = Vec::new();
    let mut entries: Vec<PathBuf> = fs::read_dir(DATA)
        .unwrap_or_else(|e| panic!("cannot read {DATA}: {e}"))
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.join("link.txt").is_file())
        .collect();
    entries.sort();
    for dir in entries {
        let name = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let text = fs::read_to_string(dir.join("link.txt")).expect("link.txt");
        let mut case = Case {
            name,
            dir,
            objects: Vec::new(),
            flags: Vec::new(),
            archives: Vec::new(),
            error: None,
            vlink_accepts: false,
        };
        for line in text.lines() {
            let mut words = line.split_whitespace();
            let Some(key) = words.next() else { continue };
            let rest: Vec<String> = words.map(str::to_owned).collect();
            match key {
                "objects" => case.objects = rest,
                "flags" => case.flags = rest,
                "archive" => {
                    let mut it = rest.into_iter();
                    if let Some(lib) = it.next() {
                        case.archives.push((lib, it.collect()));
                    }
                }
                "error" => case.error = Some(rest.join(" ")),
                "vlink" => case.vlink_accepts = rest.first().map(String::as_str) == Some("accepts"),
                _ => {}
            }
        }
        cases.push(case);
    }
    assert!(!cases.is_empty(), "no cases under {DATA}");
    cases
}

fn scratch(case: &str, what: &str) -> PathBuf {
    common::scratch::scratch_dir("qld-tests/hunk", &format!("{case}-{what}"))
}

/// Runs qld on `case`, with the inputs taken from `inputs`, and returns the
/// load file, or the diagnostics of a failed link.
fn qld_link(case: &Case, inputs: &Path, out: &Path, threads: &str) -> Result<Vec<u8>, String> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_qld"));
    command
        .arg("--oformat")
        .arg("amigahunk")
        .arg(format!("--threads={threads}"))
        .arg("-o")
        .arg(out);
    for flag in &case.flags {
        command.arg(flag);
    }
    for object in &case.objects {
        command.arg(inputs.join(object));
    }
    let output = command
        .env("LC_ALL", "C")
        .output()
        .unwrap_or_else(|e| panic!("cannot run qld: {e}"));
    if !output.status.success() {
        return Err(format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(fs::read(out).unwrap_or_else(|e| panic!("cannot read {}: {e}", out.display())))
}

/// A readable rendering of a load file's structure, for failure messages.
fn structure(bytes: &[u8], what: &str) -> String {
    let file: LoadFile = match format::read(bytes, Path::new(what)) {
        Ok(file) => file,
        Err(error) => return format!("{what}: unreadable: {error}\n"),
    };
    let mut out = format!("{what}: {} hunks\n", file.hunks.len());
    for (index, hunk) in file.hunks.iter().enumerate() {
        let (alloc, memory) = file
            .sizes
            .get(index)
            .copied()
            .unwrap_or((hunk.alloc, format::MemFlags::Any));
        let kind = match hunk.kind {
            Kind::Code => "CODE",
            Kind::Data => "DATA",
            Kind::Bss => "BSS",
        };
        out.push_str(&format!(
            "  hunk {index}: {kind} alloc={alloc} block={} memory={memory:?}\n",
            hunk.data.len()
        ));
        for (target, offsets) in &hunk.relocs {
            out.push_str(&format!("    reloc32 -> hunk {target}: {offsets:?}\n"));
        }
        for (name, value) in &hunk.symbols {
            out.push_str(&format!(
                "    symbol {} = 0x{value:x}\n",
                String::from_utf8_lossy(name)
            ));
        }
    }
    out
}

fn compare(name: &str, qld: &[u8], reference: &[u8], reference_name: &str) {
    if qld == reference {
        return;
    }
    let first = qld
        .iter()
        .zip(reference)
        .position(|(a, b)| a != b)
        .unwrap_or_else(|| qld.len().min(reference.len()));
    panic!(
        "{name}: qld's load file differs from {reference_name} \
         (first difference at byte {first}, qld {} bytes, {reference_name} {} bytes)\n\n\
         {}\n{}",
        qld.len(),
        reference.len(),
        structure(qld, "qld"),
        structure(reference, reference_name),
    );
}

/// Builds the archives a case needs, in the directory the objects are in.
fn build_archives(case: &Case, dir: &Path) -> Result<(), String> {
    for (lib, members) in &case.archives {
        let ar = std::env::var_os("QLD_TEST_AR").unwrap_or_else(|| "ar".into());
        let status = Command::new(&ar)
            .arg("rcsD")
            .arg(dir.join(lib))
            .args(members.iter().map(|m| dir.join(m)))
            .status()
            .map_err(|e| format!("cannot run {}: {e}", ar.to_string_lossy()))?;
        if !status.success() {
            return Err(format!("ar failed for {lib}"));
        }
    }
    Ok(())
}

#[test]
fn matches_vlink_reference() {
    for case in cases() {
        if case.error.is_some() {
            continue;
        }
        let expected = case.dir.join("expected.hunk");
        let reference = fs::read(&expected)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", expected.display()));
        let out = scratch(&case.name, "reference").join("out.hunk");
        let bytes = match qld_link(&case, &case.dir, &out, "2") {
            Ok(bytes) => bytes,
            Err(message) => panic!("{}: qld failed:\n{message}", case.name),
        };
        compare(&case.name, &bytes, &reference, "vlink");
    }
}

#[test]
fn rejects_what_a_load_file_cannot_hold() {
    let mut checked = 0;
    for case in cases() {
        let Some(expected) = &case.error else {
            continue;
        };
        let out = scratch(&case.name, "error").join("out.hunk");
        match qld_link(&case, &case.dir, &out, "2") {
            Ok(_) => panic!(
                "{}: qld accepted a link it must reject ({expected})",
                case.name
            ),
            Err(message) => {
                assert!(
                    message.contains(expected.as_str()),
                    "{}: the diagnostic does not say {expected:?}:\n{message}",
                    case.name
                );
                checked += 1;
            }
        }
    }
    assert!(checked > 0, "no error cases in the corpus");
}

#[test]
fn deterministic() {
    for case in cases() {
        if case.error.is_some() {
            continue;
        }
        let dir = scratch(&case.name, "determinism");
        let one = qld_link(&case, &case.dir, &dir.join("one.hunk"), "1");
        let four = qld_link(&case, &case.dir, &dir.join("four.hunk"), "4");
        match (one, four) {
            (Ok(one), Ok(four)) => assert_eq!(
                one, four,
                "{}: the load file depends on the thread count",
                case.name
            ),
            (a, b) => panic!("{}: a link failed: {a:?} {b:?}", case.name),
        }
    }
}

/// `$var` if it names an executable.
fn from_env(var: &str) -> Option<PathBuf> {
    let value = std::env::var_os(var)?;
    (!value.is_empty())
        .then(|| PathBuf::from(value))
        .filter(|p| p.is_file())
}

fn in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// Where `generate.sh --build` leaves the two binaries.
fn in_cache(relative: &str) -> Option<PathBuf> {
    let cache = std::env::var_os("CACHE")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("qld-hunk-tools"));
    let path = cache.join(relative);
    path.is_file().then_some(path)
}

fn find_vasm() -> Option<PathBuf> {
    from_env("VASM")
        .or_else(|| in_path("vasmm68k_mot"))
        .or_else(|| in_cache("vasm/vasmm68k_mot"))
}

fn find_vlink() -> Option<PathBuf> {
    from_env("VLINK")
        .or_else(|| in_path("vlink"))
        .or_else(|| in_cache("vlink/vlink"))
}

fn tools_required() -> bool {
    std::env::var_os("QLD_REQUIRE_HUNK_TOOLS").is_some_and(|v| !v.is_empty() && v != "0")
}

#[test]
fn live_tools_agree() {
    let (Some(vasm), Some(vlink)) = (find_vasm(), find_vlink()) else {
        let reason = "vasm (vasmm68k_mot) or vlink is not installed; \
                      run tests/data/hunk/generate.sh --build, or set $VASM and $VLINK";
        assert!(
            !tools_required(),
            "QLD_REQUIRE_HUNK_TOOLS is set but {reason}"
        );
        println!("SKIPPED: {reason}");
        return;
    };
    for case in cases() {
        let dir = scratch(&case.name, "live");
        let mut sources: Vec<PathBuf> = fs::read_dir(&case.dir)
            .expect("case directory")
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|e| e == "s"))
            .collect();
        sources.sort();
        for source in &sources {
            let name = source.file_stem().expect("stem");
            let object = dir.join(name).with_extension("o");
            let status = Command::new(&vasm)
                .arg("-Felf")
                .arg("-quiet")
                .arg("-o")
                .arg(&object)
                .arg(source)
                .status()
                .unwrap_or_else(|e| panic!("cannot run {}: {e}", vasm.display()));
            assert!(
                status.success(),
                "{}: vasm failed on {}",
                case.name,
                source.display()
            );
            let fresh = fs::read(&object).expect("object");
            let committed = case.dir.join(object.file_name().expect("name"));
            let stored = fs::read(&committed)
                .unwrap_or_else(|e| panic!("cannot read {}: {e}", committed.display()));
            assert_eq!(
                fresh,
                stored,
                "{}: the committed {} is not what vasm produces now; \
                 re-run tests/data/hunk/generate.sh and commit the result",
                case.name,
                committed.display()
            );
        }
        build_archives(&case, &dir).unwrap_or_else(|e| panic!("{}: {e}", case.name));
        let out = dir.join("vlink.hunk");
        let mut command = Command::new(&vlink);
        command.arg("-b").arg("amigahunk");
        for flag in &case.flags {
            command.arg(flag);
        }
        command.arg("-o").arg(&out);
        for object in &case.objects {
            command.arg(dir.join(object));
        }
        let output = command
            .env("LC_ALL", "C")
            .output()
            .unwrap_or_else(|e| panic!("cannot run {}: {e}", vlink.display()));
        if let Some(why) = &case.error {
            assert_eq!(
                output.status.success(),
                case.vlink_accepts,
                "{}: vlink's verdict on {why:?} changed; \
                 update `vlink accepts` in link.txt if that is intended",
                case.name
            );
            continue;
        }
        assert!(
            output.status.success(),
            "{}: vlink failed:\n{}{}",
            case.name,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let reference = fs::read(&out).expect("vlink output");
        let committed = fs::read(case.dir.join("expected.hunk")).expect("expected.hunk");
        assert_eq!(
            reference, committed,
            "{}: the committed expected.hunk is not what vlink produces now; \
             re-run tests/data/hunk/generate.sh and commit the result",
            case.name
        );
        let bytes = qld_link(&case, &dir, &dir.join("qld.hunk"), "2")
            .unwrap_or_else(|e| panic!("{}: qld failed:\n{e}", case.name));
        compare(&case.name, &bytes, &reference, "vlink");
    }
}

/// The library entry point: a [`qld::BinaryFormat::Hunk`] target reaches
/// `qld::hunk::link`, which settles the options a load file implies and
/// hands the link to the ELF backend. The `qld` binary gets there through
/// `--oformat amigahunk` instead, which every test above uses.
#[test]
fn hunk_target_links_through_the_library() {
    use qld::{
        Architecture, BinaryFormat, Endianness, InputAttrs, InputKind, LinkOptions,
        OperatingSystem, OutputKind, PointerWidth, Target,
    };

    let dir = Path::new(DATA).join("basic");
    let out = scratch("basic", "library").join("out.hunk");
    let mut options = LinkOptions::new();
    options.kind = OutputKind::StaticExecutable;
    options.threads = Some(2);
    options.output = Some(out.clone());
    options.target = Some(Target {
        format: BinaryFormat::Hunk,
        arch: Architecture::M68k,
        endian: Endianness::Big,
        pointer_width: PointerWidth::Bits32,
        os: OperatingSystem::None,
    });
    for object in ["main.o", "helper.o"] {
        options.push_input(InputKind::File(dir.join(object)), InputAttrs::default());
    }
    let diagnostics = qld::diag::Collect::new();
    qld::link(&options, &diagnostics).unwrap_or_else(|e| panic!("the library link failed: {e}"));
    let bytes = fs::read(&out).expect("load file");
    let reference = fs::read(dir.join("expected.hunk")).expect("reference");
    compare("basic (library)", &bytes, &reference, "vlink");
}
