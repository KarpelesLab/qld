//! Integration tests for the Mach-O linker (`qld::macho`, workstream W25).
//!
//! Fixtures in `tests/data/macho_link/` are compiled with
//! `clang --target=<arch>-apple-macos13` (no SDK needed: the sources declare
//! what they use, and `tests/data/macho_link/sdk` provides `.tbd` stubs for
//! libSystem and libc++). The outputs are checked structurally with qld's
//! own Mach-O reader and, when installed, `llvm-objdump`; the code signature
//! hashes are recomputed; and when `ld64.lld` is available the same inputs
//! are linked with it and the two outputs compared.
//!
//! On macOS the fixtures link against the real SDK (`xcrun --show-sdk-path`)
//! and are run.
//!
//! Missing tools make tests skip with a message. With
//! `QLD_REQUIRE_MACHO_TOOLS=1` (set on the macOS CI runner) a missing tool is
//! a failure instead.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use qld::args::{ParseOutcome, parse_darwin};
use qld::diag::{Collect, DiagnosticSink};
use qld::macho::read::consts::{
    LC_CODE_SIGNATURE, LC_DYLD_CHAINED_FIXUPS, LC_DYLD_EXPORTS_TRIE, LC_LOAD_DYLIB, LC_MAIN,
    LC_UUID, MH_EXECUTE,
};
use qld::macho::read::{MachOFile, Source};
use qld::macho::sha256::Sha256;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn data_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/macho_link")
}

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("macho_link")
        .join(name);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn require_tools() -> bool {
    std::env::var_os("QLD_REQUIRE_MACHO_TOOLS").is_some_and(|v| v == "1")
}

/// Skips (or fails, under `QLD_REQUIRE_MACHO_TOOLS=1`) with `why`.
fn skip(test: &str, why: &str) {
    assert!(!require_tools(), "{test}: {why}");
    eprintln!("skipping {test}: {why}");
}

fn tool_works(tool: &str, args: &[&str]) -> bool {
    Command::new(tool)
        .args(args)
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Whether clang can compile for `arch`-apple-macos.
fn clang_for(arch: &str) -> bool {
    let dir = scratch("probe");
    let source = dir.join("probe.c");
    std::fs::write(&source, "int probe(void) { return 1; }\n").unwrap();
    let object = dir.join(format!("probe-{arch}.o"));
    tool_works(
        "clang",
        &[
            &format!("--target={arch}-apple-macos13"),
            "-c",
            source.to_str().unwrap(),
            "-o",
            object.to_str().unwrap(),
        ],
    )
}

/// Compiles fixture `source` for `arch` into the test's scratch directory.
fn compile(test: &str, source: &str, arch: &str, extra: &[&str]) -> PathBuf {
    let dir = scratch(test);
    let input = data_dir().join(source);
    let stem = Path::new(source).file_stem().unwrap().to_str().unwrap();
    let output = dir.join(format!("{stem}-{arch}.o"));
    let compiler = if source.ends_with(".cpp") || source.ends_with(".mm") {
        "clang++"
    } else {
        "clang"
    };
    let status = Command::new(compiler)
        .arg(format!("--target={arch}-apple-macos13"))
        .args(["-O1", "-c"])
        .args(extra)
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .status()
        .unwrap();
    assert!(status.success(), "compiling {source} for {arch}");
    output
}

/// The `-syslibroot` to link against: the real SDK on macOS, the stubs
/// elsewhere.
fn syslibroot() -> PathBuf {
    if cfg!(target_os = "macos")
        && let Ok(output) = Command::new("xcrun").arg("--show-sdk-path").output()
        && output.status.success()
    {
        return PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    }
    data_dir().join("sdk")
}

fn os(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

/// Links with an ld64 command line (without `argv[0]`) and returns the
/// output bytes and the diagnostics.
fn link_bytes(args: &[OsString]) -> Result<(Vec<u8>, Vec<String>), String> {
    let mut argv = vec![OsString::from("ld64.qld")];
    argv.extend_from_slice(args);
    let options = match parse_darwin(&argv) {
        Ok(ParseOutcome::Link(options)) => options,
        Ok(other) => return Err(format!("not a link: {other:?}")),
        Err(error) => return Err(error.to_string()),
    };
    let diagnostics = Collect::new();
    let result = qld::macho::link_to_bytes(&options, &diagnostics);
    let messages: Vec<String> = diagnostics
        .take_sorted()
        .into_iter()
        .map(|d| d.message)
        .collect();
    match result {
        Ok(bytes) => Ok((bytes, messages)),
        Err(error) => Err(format!("{error}: {messages:?}")),
    }
}

/// Links `objects` into an executable for `arch` and writes it to `output`.
fn link_executable(arch: &str, objects: &[&Path], extra: &[&str], output: &Path) -> Vec<u8> {
    let root = syslibroot();
    let mut args = os(&[
        "-arch",
        arch,
        "-platform_version",
        "macos",
        "13.0",
        "13.0",
        "-syslibroot",
        root.to_str().unwrap(),
        "-o",
        output.to_str().unwrap(),
    ]);
    for object in objects {
        args.push(object.into());
    }
    args.extend(os(extra));
    args.extend(os(&["-lSystem"]));
    let (bytes, _) = link_bytes(&args).unwrap_or_else(|e| panic!("link failed: {e}"));
    std::fs::write(output, &bytes).unwrap();
    make_executable(output);
    bytes
}

#[cfg(unix)]
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[cfg(not(unix))]
fn make_executable(_: &Path) {}

/// The load commands of `data`, as `(cmd, payload offset)`.
fn load_commands(data: &[u8]) -> Vec<(u32, usize, usize)> {
    let file = MachOFile::parse(data, Source::new(Path::new("out"))).unwrap();
    file.load_commands()
        .map(|c| {
            let c = c.unwrap();
            (c.cmd, c.offset as usize, c.data.len())
        })
        .collect()
}

fn u32_le(data: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(data[at..at + 4].try_into().unwrap())
}

fn u32_be(data: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(data[at..at + 4].try_into().unwrap())
}

/// Recomputes the page hashes of the ad-hoc signature.
fn check_signature(data: &[u8]) {
    let (_, offset, _) = load_commands(data)
        .into_iter()
        .find(|c| c.0 == LC_CODE_SIGNATURE)
        .expect("LC_CODE_SIGNATURE");
    let sig_offset = u32_le(data, offset + 8) as usize;
    let sig_size = u32_le(data, offset + 12) as usize;
    assert_eq!(sig_offset + sig_size, data.len(), "signature ends the file");
    let sig = &data[sig_offset..];
    assert_eq!(u32_be(sig, 0), 0xfade_0cc0, "SuperBlob magic");
    assert_eq!(u32_be(sig, 8), 1, "one blob");
    let cd = u32_be(sig, 16) as usize;
    assert_eq!(u32_be(sig, cd), 0xfade_0c02, "CodeDirectory magic");
    let flags = u32_be(sig, cd + 12);
    assert_eq!(flags, 0x0002_0002, "adhoc | linker-signed");
    let hash_offset = cd + u32_be(sig, cd + 16) as usize;
    let slots = u32_be(sig, cd + 28) as usize;
    let code_limit = u32_be(sig, cd + 32) as usize;
    assert_eq!(code_limit, sig_offset);
    assert_eq!(sig[cd + 36], 32, "hash size");
    assert_eq!(sig[cd + 37], 2, "SHA-256");
    assert_eq!(sig[cd + 39], 12, "4 KiB pages");
    assert_eq!(slots, code_limit.div_ceil(4096));
    for slot in 0..slots {
        let page = &data[slot * 4096..((slot + 1) * 4096).min(code_limit)];
        let expected = Sha256::digest(page);
        let at = hash_offset + slot * 32;
        assert_eq!(&sig[at..at + 32], &expected, "hash of page {slot}");
    }
}

fn objdump(args: &[&str], file: &Path) -> Option<String> {
    let output = Command::new("llvm-objdump")
        .args(args)
        .arg(file)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// `ld64.lld`, from `QLD_LD64_LLD` or the usual build location.
fn ld64_lld() -> Option<PathBuf> {
    let candidates = [
        std::env::var_os("QLD_LD64_LLD").map(PathBuf::from),
        std::env::var_os("HOME").map(|h| {
            PathBuf::from(h).join(".cache/qld-projects/build/llvm-23.1.1-static/bin/ld64.lld")
        }),
        Some(PathBuf::from("ld64.lld")),
    ];
    candidates
        .into_iter()
        .flatten()
        .find(|path| tool_works(path.to_str().unwrap_or(""), &["--version"]))
}

/// Names of the sections, exports and imports, for comparing outputs.
#[derive(Debug, PartialEq, Eq)]
struct Summary {
    sections: BTreeSet<String>,
    exports: BTreeSet<String>,
    imports: BTreeSet<String>,
    dylibs: Vec<String>,
}

fn summarize(file: &Path) -> Option<Summary> {
    let headers = objdump(&["--macho", "--section-headers"], file)?;
    let sections = headers
        .lines()
        .filter_map(|l| {
            let mut parts = l.split_whitespace();
            let _index = parts.next()?.parse::<u32>().ok()?;
            let name = parts.next()?;
            // Linker-specific extras.
            (name != "__unwind_info" && name != "__eh_frame").then(|| name.to_owned())
        })
        .collect();
    let trie = objdump(&["--macho", "--exports-trie"], file)?;
    let exports = trie
        .lines()
        .filter_map(|l| l.split_whitespace().nth(1).map(str::to_owned))
        .filter(|n| n.starts_with('_'))
        .collect();
    let fixups = objdump(&["--macho", "--dyld-info"], file)?;
    let imports = fixups
        .lines()
        .filter(|l| l.contains("bind"))
        .filter_map(|l| l.split_whitespace().last().map(str::to_owned))
        .collect();
    let dylibs = objdump(&["--macho", "--dylibs-used"], file)?
        .lines()
        .skip(1)
        .map(|l| l.split_whitespace().next().unwrap_or("").to_owned())
        .collect();
    Some(Summary {
        sections,
        exports,
        imports,
        dylibs,
    })
}

/// Runs `binary` on macOS and returns its stdout.
fn run(binary: &Path) -> Option<String> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let output = Command::new(binary).output().unwrap();
    assert!(
        output.status.success(),
        "{} failed: {:?}\n{}",
        binary.display(),
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn host_can_run(arch: &str) -> bool {
    cfg!(target_os = "macos")
        && (arch == std::env::consts::ARCH
            || (arch == "arm64" && std::env::consts::ARCH == "aarch64")
            || (arch == "x86_64" && tool_works("arch", &["-x86_64", "/usr/bin/true"])))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn hello_executables() {
    for arch in ["arm64", "x86_64"] {
        if !clang_for(arch) {
            skip(
                "hello_executables",
                &format!("clang cannot target {arch}-apple-macos"),
            );
            continue;
        }
        let object = compile("hello", "hello.c", arch, &[]);
        let output = scratch("hello").join(format!("hello-{arch}"));
        let bytes = link_executable(arch, &[&object], &[], &output);

        let file = MachOFile::parse(&bytes, Source::new(&output)).unwrap();
        assert_eq!(file.header().file_type, MH_EXECUTE);
        let commands: Vec<u32> = load_commands(&bytes).iter().map(|c| c.0).collect();
        for wanted in [
            LC_DYLD_CHAINED_FIXUPS,
            LC_DYLD_EXPORTS_TRIE,
            LC_MAIN,
            LC_UUID,
            LC_LOAD_DYLIB,
        ] {
            assert!(
                commands.contains(&wanted),
                "{arch}: missing command {wanted:#x}"
            );
        }
        if arch == "arm64" {
            check_signature(&bytes);
        } else {
            assert!(!commands.contains(&LC_CODE_SIGNATURE));
        }

        if let Some(disassembly) = objdump(&["--macho", "-d"], &output) {
            assert!(
                disassembly.contains("symbol stub for: _printf"),
                "{arch}: {disassembly}"
            );
        }
        if let Some(fixups) = objdump(&["--macho", "--chained-fixups"], &output) {
            assert!(fixups.contains("(_printf)"), "{arch}: {fixups}");
        }

        // Same inputs through ld64.lld: the same sections, exports, imports
        // and dylibs.
        if let Some(lld) = ld64_lld() {
            let reference = scratch("hello").join(format!("hello-{arch}-lld"));
            let status = Command::new(lld)
                .args(["-arch", arch, "-platform_version", "macos", "13.0", "13.0"])
                .arg("-syslibroot")
                .arg(syslibroot())
                .arg(&object)
                .arg("-lSystem")
                .arg("-o")
                .arg(&reference)
                .status()
                .unwrap();
            assert!(status.success());
            if let (Some(ours), Some(theirs)) = (summarize(&output), summarize(&reference)) {
                assert_eq!(ours, theirs, "{arch}: qld vs ld64.lld");
            }
        }

        if host_can_run(arch) {
            let stdout = run(&output).unwrap();
            assert_eq!(stdout, "hello from qld 3 42\n");
        }
    }
}

#[test]
fn output_is_deterministic() {
    if !clang_for("arm64") {
        skip(
            "output_is_deterministic",
            "clang cannot target arm64-apple-macos",
        );
        return;
    }
    let object = compile("determinism", "hello.c", "arm64", &[]);
    let root = syslibroot();
    let args = os(&[
        "-arch",
        "arm64",
        "-platform_version",
        "macos",
        "13.0",
        "13.0",
        "-syslibroot",
        root.to_str().unwrap(),
        object.to_str().unwrap(),
        "-lSystem",
        "-o",
        "determinism-out",
    ]);
    let mut outputs = Vec::new();
    for threads in [1, 2, 8] {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .unwrap();
        outputs.push(pool.install(|| link_bytes(&args).unwrap().0));
    }
    assert!(outputs.windows(2).all(|w| w[0] == w[1]));
}

#[test]
fn undefined_symbols_are_reported() {
    if !clang_for("arm64") {
        skip(
            "undefined_symbols_are_reported",
            "clang cannot target arm64-apple-macos",
        );
        return;
    }
    let dir = scratch("undefined");
    let source = dir.join("undefined.c");
    std::fs::write(
        &source,
        "void missing(void); int main(void) { missing(); return 0; }\n",
    )
    .unwrap();
    let object = dir.join("undefined.o");
    assert!(tool_works(
        "clang",
        &[
            "--target=arm64-apple-macos13",
            "-c",
            source.to_str().unwrap(),
            "-o",
            object.to_str().unwrap()
        ]
    ));
    let root = data_dir().join("sdk");
    let base = os(&[
        "-arch",
        "arm64",
        "-syslibroot",
        root.to_str().unwrap(),
        object.to_str().unwrap(),
        "-lSystem",
    ]);
    let error = link_bytes(&base).unwrap_err();
    assert!(error.contains("undefined symbol: _missing"), "{error}");

    let mut lookup = base.clone();
    lookup.extend(os(&["-undefined", "dynamic_lookup"]));
    let (bytes, _) = link_bytes(&lookup).unwrap();
    let path = dir.join("lookup");
    std::fs::write(&path, &bytes).unwrap();
    if let Some(fixups) = objdump(&["--macho", "--chained-fixups"], &path) {
        assert!(fixups.contains("flat-namespace"), "{fixups}");
    }
}

/// Runs the link in `W25_ARGS` (whitespace-separated), for development.
#[test]
#[ignore = "manual"]
fn manual() {
    let args: Vec<OsString> = std::env::var("W25_ARGS")
        .unwrap_or_default()
        .split_whitespace()
        .map(OsString::from)
        .collect();
    let mut argv = vec![OsString::from("ld64.qld")];
    argv.extend(args);
    let Ok(ParseOutcome::Link(options)) = parse_darwin(&argv) else {
        panic!("not a link");
    };
    if let Err(error) = qld::macho::link(&options, &qld::diag::Stderr::new("qld")) {
        panic!("{error}");
    }
    let _: &dyn DiagnosticSink = &Collect::new();
}
