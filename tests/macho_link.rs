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

/// `llvm-<name>`, or `<name>` when only that exists (Xcode ships the LLVM
/// tools without the prefix: `objdump`, `nm`, `lipo`, `dwarfdump`).
fn tool(name: &str) -> String {
    let prefixed = format!("llvm-{name}");
    if Command::new(&prefixed).arg("--version").output().is_ok() {
        return prefixed;
    }
    if Command::new(name).arg("--version").output().is_ok() {
        return name.to_owned();
    }
    prefixed
}

/// Creates a static archive of `objects` with `llvm-ar`, or Xcode's
/// `libtool`.
fn make_archive(archive: &Path, objects: &[&Path]) -> bool {
    let _ = std::fs::remove_file(archive);
    let mut llvm_ar = Command::new("llvm-ar");
    llvm_ar
        .args(["--format=darwin", "rcs"])
        .arg(archive)
        .args(objects);
    if llvm_ar.output().is_ok_and(|o| o.status.success()) {
        return true;
    }
    Command::new("libtool")
        .args(["-static", "-o"])
        .arg(archive)
        .args(objects)
        .output()
        .is_ok_and(|o| o.status.success())
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
    let output = Command::new(tool("objdump"))
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
    /// The `__unwind_info` encodings, in function order, without addresses.
    unwind: Vec<String>,
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
    // DWARF encodings hold an `__eh_frame` offset, which depends on which
    // CIEs a linker keeps.
    let data = std::fs::read(file).ok()?;
    let arm64 = MachOFile::parse(&data, Source::new(file))
        .ok()?
        .header()
        .cpu_type
        == qld::macho::read::consts::CPU_TYPE_ARM64;
    let dwarf_mode = if arm64 { 0x0300_0000 } else { 0x0400_0000 };
    let unwind = objdump(&["--macho", "--unwind-info"], file)?
        .lines()
        .filter_map(|l| {
            let (_, encoding) = l.split_once("encoding[")?;
            let text = encoding.split_once('=')?.1.trim();
            let value = u32::from_str_radix(text.trim_start_matches("0x"), 16).ok()?;
            Some(if value & 0x0f00_0000 == dwarf_mode {
                "dwarf".to_owned()
            } else {
                text.to_owned()
            })
        })
        .collect();
    Some(Summary {
        sections,
        exports,
        imports,
        dylibs,
        unwind,
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

/// Links `args` (an ld64 command line without `-o`) with qld into
/// `output`, and with `ld64.lld` into `output-lld` when it is available,
/// checking that both have the same sections, exports, imports and dylibs.
fn link_and_compare(args: &[String], output: &Path) -> Vec<u8> {
    let mut ours: Vec<OsString> = args.iter().map(OsString::from).collect();
    ours.push("-o".into());
    ours.push(output.into());
    let (bytes, _) = link_bytes(&ours).unwrap_or_else(|e| panic!("link failed: {e}"));
    std::fs::write(output, &bytes).unwrap();
    make_executable(output);
    if let Some(lld) = ld64_lld() {
        let mut reference = output.as_os_str().to_owned();
        reference.push("-lld");
        let reference = PathBuf::from(reference);
        let status = Command::new(lld)
            .args(args)
            .arg("-o")
            .arg(&reference)
            .status()
            .unwrap();
        assert!(status.success(), "ld64.lld failed on {args:?}");
        if let (Some(mut ours), Some(mut theirs)) = (summarize(output), summarize(&reference)) {
            // lld rewrites Objective-C metadata (relative method lists,
            // class names and method types merged into __cstring), which
            // qld does not.
            if args.iter().any(|a| a == "-lobjc") {
                ours.sections.clear();
                theirs.sections.clear();
            }
            assert_eq!(ours, theirs, "{}: qld vs ld64.lld", output.display());
        }
    }
    bytes
}

fn base_args(arch: &str) -> Vec<String> {
    [
        "-arch",
        arch,
        "-platform_version",
        "macos",
        "13.0",
        "13.0",
        "-syslibroot",
        syslibroot().to_str().unwrap(),
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect()
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|s| (*s).to_owned()).collect()
}

#[test]
fn dylib_and_client() {
    for arch in ["arm64", "x86_64"] {
        if !clang_for(arch) {
            skip(
                "dylib_and_client",
                &format!("clang cannot target {arch}-apple-macos"),
            );
            continue;
        }
        let dir = scratch("dylib");
        let greet = compile("dylib", "greet.c", arch, &[]);
        let client = compile("dylib", "use_greet.c", arch, &[]);
        let lib_dir = dir.join(arch);
        std::fs::create_dir_all(&lib_dir).unwrap();

        let mut args = base_args(arch);
        args.extend(strings(&[
            "-dylib",
            "-install_name",
            "@rpath/libgreet.dylib",
            "-current_version",
            "1.2.3",
            greet.to_str().unwrap(),
            "-lSystem",
        ]));
        let lib = lib_dir.join("libgreet.dylib");
        let bytes = link_and_compare(&args, &lib);
        let file = MachOFile::parse(&bytes, Source::new(&lib)).unwrap();
        assert_eq!(file.header().file_type, qld::macho::read::consts::MH_DYLIB);
        if let Some(trie) = objdump(&["--macho", "--exports-trie"], &lib) {
            for name in ["_greet", "_greet_count", "_greet_tls", "_greet_weak"] {
                assert!(trie.contains(name), "{arch}: {name} not exported:\n{trie}");
            }
            assert!(
                !trie.contains("_greet_hidden"),
                "{arch}: hidden symbol exported"
            );
        }
        if let Some(info) = objdump(&["--macho", "--dyld-info"], &lib) {
            assert!(
                info.lines()
                    .any(|l| l.contains("weak") && l.contains("_greet_weak")),
                "{arch}: weak definition not bound through weak lookup:\n{info}"
            );
        }

        let mut args = base_args(arch);
        args.extend(strings(&[
            client.to_str().unwrap(),
            &format!("-L{}", lib_dir.display()),
            "-lgreet",
            "-rpath",
            "@executable_path",
            "-U",
            "_missing_weak",
            "-lSystem",
        ]));
        let exe = lib_dir.join("use_greet");
        link_and_compare(&args, &exe);
        if let Some(info) = objdump(&["--macho", "--dyld-info"], &exe) {
            assert!(
                info.contains("_missing_weak (weak import)"),
                "{arch}:\n{info}"
            );
            assert!(info.contains("libgreet"), "{arch}:\n{info}");
        }
        if host_can_run(arch) {
            let stdout = run(&exe).unwrap();
            assert!(stdout.contains("hello, dylib"), "{stdout}");
        }
    }
}

#[test]
fn thread_local_variables() {
    for arch in ["arm64", "x86_64"] {
        if !clang_for(arch) {
            skip(
                "thread_local_variables",
                &format!("clang cannot target {arch}-apple-macos"),
            );
            continue;
        }
        let main = compile("tlv", "tlv.c", arch, &[]);
        let other = compile("tlv", "tlv_other.c", arch, &[]);
        let mut args = base_args(arch);
        args.extend(strings(&[
            main.to_str().unwrap(),
            other.to_str().unwrap(),
            "-lSystem",
        ]));
        let exe = scratch("tlv").join(format!("tlv-{arch}"));
        let bytes = link_and_compare(&args, &exe);
        let flags = MachOFile::parse(&bytes, Source::new(&exe))
            .unwrap()
            .header()
            .flags;
        assert_ne!(flags & qld::macho::read::consts::MH_HAS_TLV_DESCRIPTORS, 0);
        if let Some(contents) = objdump(&["--macho", "-s", "-j", "__thread_vars"], &exe) {
            // The offset fields: 0, 4, 0x50 and 0x48 into the template, in
            // descriptor order.
            assert!(contents.contains("__thread_vars"), "{contents}");
        }
        if host_can_run(arch) {
            assert_eq!(run(&exe).unwrap(), "42 3 tls 6\n");
        }
    }
}

#[test]
fn cxx_exceptions() {
    for arch in ["arm64", "x86_64"] {
        if !clang_for(arch) {
            skip(
                "cxx_exceptions",
                &format!("clang cannot target {arch}-apple-macos"),
            );
            continue;
        }
        let object = compile("exceptions", "exceptions.cpp", arch, &[]);
        let mut args = base_args(arch);
        args.extend(strings(&[object.to_str().unwrap(), "-lc++", "-lSystem"]));
        let exe = scratch("exceptions").join(format!("exceptions-{arch}"));
        link_and_compare(&args, &exe);
        if let Some(unwind) = objdump(&["--macho", "--unwind-info"], &exe) {
            assert!(
                unwind.contains("Personality functions: (count = 1)"),
                "{unwind}"
            );
            assert!(unwind.contains("LSDA descriptors:"), "{unwind}");
        }
        if host_can_run(arch) {
            assert_eq!(run(&exe).unwrap(), "caught 84 after 10 cleanups\n");
        }
    }
}

/// The address of symbol `name` in `file`, from `llvm-nm`.
fn nm_address(file: &Path, name: &str) -> Option<u64> {
    let output = Command::new(tool("nm")).arg(file).output().ok()?;
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|l| {
            let mut parts = l.split_whitespace();
            let address = parts.next()?;
            let _kind = parts.next()?;
            (parts.next()? == name)
                .then(|| u64::from_str_radix(address, 16).ok())
                .flatten()
        })
}

#[test]
fn dwarf_unwind_information() {
    for arch in ["arm64", "x86_64"] {
        if !clang_for(arch) {
            skip(
                "dwarf_unwind_information",
                &format!("clang cannot target {arch}-apple-macos"),
            );
            continue;
        }
        let main = compile("dwarf", "dwarf_unwind.cpp", arch, &[]);
        let asm = compile("dwarf", &format!("dwarf-{arch}.s"), arch, &[]);
        let mut args = base_args(arch);
        args.extend(strings(&[
            main.to_str().unwrap(),
            asm.to_str().unwrap(),
            "-lc++",
            "-lSystem",
        ]));
        let exe = scratch("dwarf").join(format!("dwarf-{arch}"));
        link_and_compare(&args, &exe);
        // The FDE in __eh_frame covers call_through.
        if let (Ok(frames), Some(function)) = (
            Command::new(tool("dwarfdump"))
                .arg("--eh-frame")
                .arg(&exe)
                .output(),
            nm_address(&exe, "_call_through"),
        ) {
            let frames = String::from_utf8_lossy(&frames.stdout);
            let wanted = format!("pc={function:08x}...");
            assert!(
                frames.contains(&wanted),
                "{arch}: no FDE for {wanted}:\n{frames}"
            );
        }
        if host_can_run(arch) {
            assert_eq!(run(&exe).unwrap(), "dwarf 7\n");
        }
    }
}

#[test]
fn dead_stripping() {
    for arch in ["arm64", "x86_64"] {
        if !clang_for(arch) {
            skip(
                "dead_stripping",
                &format!("clang cannot target {arch}-apple-macos"),
            );
            continue;
        }
        let object = compile("dead_strip", "dead_strip.c", arch, &[]);
        let mut args = base_args(arch);
        args.extend(strings(&[
            object.to_str().unwrap(),
            "-dead_strip",
            "-lSystem",
        ]));
        let exe = scratch("dead_strip").join(format!("dead_strip-{arch}"));
        link_and_compare(&args, &exe);
        for (name, kept) in [
            ("_main", true),
            ("_used_data", true),
            ("_kept_by_attribute", true),
            ("_unused_function", false),
            ("_unused_caller", false),
            ("_unused_data", false),
        ] {
            if tool_works(&tool("nm"), &["--version"]) {
                assert_eq!(
                    nm_address(&exe, name).is_some(),
                    kept,
                    "{arch}: {name} should {}be kept",
                    if kept { "" } else { "not " }
                );
            }
        }
        if host_can_run(arch) {
            assert_eq!(run(&exe).unwrap(), "stripped 42\n");
        }
    }
}

/// The objects and symbols of `dsymutil --dump-debug-map`, without
/// timestamps.
fn debug_map(binary: &Path) -> Option<Vec<String>> {
    let output = Command::new("dsymutil")
        .arg("--dump-debug-map")
        .arg(binary)
        .output()
        .ok()?;
    output.status.success().then(|| {
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|l| l.contains("filename:") || l.contains("sym:"))
            .map(|l| l.split(", binAddr").next().unwrap_or(l).trim().to_owned())
            .collect()
    })
}

#[test]
fn stabs_debug_map() {
    let arch = "arm64";
    if !clang_for(arch) {
        skip("stabs_debug_map", "clang cannot target arm64-apple-macos");
        return;
    }
    let dir = scratch("stabs");
    let main = compile("stabs", "tlv.c", arch, &["-g"]);
    let other = compile("stabs", "tlv_other.c", arch, &["-g"]);
    let archive = dir.join("libother.a");
    if !make_archive(&archive, &[&other]) {
        skip("stabs_debug_map", "neither llvm-ar nor libtool works");
        return;
    }
    let mut args = base_args(arch);
    args.extend(strings(&[
        main.to_str().unwrap(),
        archive.to_str().unwrap(),
        "-lSystem",
    ]));
    let exe = dir.join("stabs");
    link_and_compare(&args, &exe);
    let Some(map) = debug_map(&exe) else {
        skip("stabs_debug_map", "dsymutil missing");
        return;
    };
    let joined = map.join("\n");
    assert!(joined.contains("tlv-arm64.o'"), "{joined}");
    assert!(
        joined.contains("libother.a(tlv_other-arm64.o)'"),
        "{joined}"
    );
    for symbol in [
        "sym: _main, objAddr: 0x0",
        "sym: _tlv_other",
        "sym: _tlv_zero",
    ] {
        assert!(joined.contains(symbol), "{symbol} missing:\n{joined}");
    }
    let reference = PathBuf::from(format!("{}-lld", exe.display()));
    if let Some(theirs) = debug_map(&reference).filter(|_| reference.exists()) {
        let normalize = |lines: Vec<String>| {
            let mut lines: Vec<String> = lines
                .into_iter()
                .map(|l| l.replace(&reference.display().to_string(), ""))
                .collect();
            lines.sort();
            lines
        };
        assert_eq!(
            normalize(map),
            normalize(theirs),
            "debug map: qld vs ld64.lld"
        );
    }
}

#[test]
fn universal_binary() {
    let lipo = tool("lipo");
    if !clang_for("arm64") || !clang_for("x86_64") || Command::new(&lipo).output().is_err() {
        skip(
            "universal_binary",
            "clang for both architectures or lipo missing",
        );
        return;
    }
    let dir = scratch("universal");
    // A fat input object.
    let hello_arm = compile("universal", "hello.c", "arm64", &[]);
    let hello_x86 = compile("universal", "hello.c", "x86_64", &[]);
    let fat_object = dir.join("hello-fat.o");
    assert!(tool_works(
        &lipo,
        &[
            "-create",
            hello_arm.to_str().unwrap(),
            hello_x86.to_str().unwrap(),
            "-output",
            fat_object.to_str().unwrap()
        ]
    ));
    let root = syslibroot();
    let args = os(&[
        "-arch",
        "arm64",
        "-arch",
        "x86_64",
        "-platform_version",
        "macos",
        "13.0",
        "13.0",
        "-syslibroot",
        root.to_str().unwrap(),
        fat_object.to_str().unwrap(),
        "-lSystem",
    ]);
    let (bytes, _) = link_bytes(&args).unwrap();
    let exe = dir.join("hello-universal");
    std::fs::write(&exe, &bytes).unwrap();
    make_executable(&exe);

    let fat = qld::macho::read::FatFile::parse(&bytes, Source::new(&exe)).unwrap();
    assert_eq!(fat.slices().len(), 2);
    for slice in fat.slices() {
        let file = MachOFile::parse(slice.data, Source::new(&exe)).unwrap();
        assert_eq!(file.header().file_type, MH_EXECUTE);
        if slice.arch == qld::macho::read::Arch::ARM64 {
            assert_eq!(slice.offset % 0x4000, 0);
            assert_eq!(slice.align, 14);
            check_signature(slice.data);
        } else {
            assert_eq!(slice.offset % 0x1000, 0);
        }
    }
    let info = Command::new(&lipo).arg("-info").arg(&exe).output().unwrap();
    let info = String::from_utf8_lossy(&info.stdout);
    assert!(info.contains("x86_64") && info.contains("arm64"), "{info}");

    // Each slice matches a thin link of the same architecture.
    for (arch, slice_arch) in [
        ("arm64", qld::macho::read::Arch::ARM64),
        ("x86_64", qld::macho::read::Arch::X86_64),
    ] {
        let mut thin = os(&["-arch", arch]);
        thin.extend(args.iter().skip(4).cloned());
        let (thin_bytes, _) = link_bytes(&thin).unwrap();
        let slice = fat.select(slice_arch).unwrap();
        assert!(
            slice.data == thin_bytes.as_slice(),
            "{arch} slice differs from a thin link"
        );
    }
    if cfg!(target_os = "macos") && host_can_run("arm64") {
        assert_eq!(run(&exe).unwrap(), "hello from qld 3 42\n");
    }
}

#[test]
fn range_extension_thunks() {
    let arch = "arm64";
    if !clang_for(arch) {
        skip(
            "range_extension_thunks",
            "clang cannot target arm64-apple-macos",
        );
        return;
    }
    let dir = scratch("thunks");
    let main = compile("thunks", "far_main.c", arch, &[]);
    let far = compile("thunks", "far.c", arch, &[]);
    let filler_source = dir.join("filler.s");
    std::fs::write(
        &filler_source,
        ".text\n.globl _filler\n.p2align 2\n_filler:\n.space 0x8200000\nret\n.subsections_via_symbols\n",
    )
    .unwrap();
    let filler = dir.join("filler.o");
    assert!(tool_works(
        "clang",
        &[
            "--target=arm64-apple-macos13",
            "-c",
            filler_source.to_str().unwrap(),
            "-o",
            filler.to_str().unwrap()
        ]
    ));
    let mut args = base_args(arch);
    args.extend(strings(&[
        main.to_str().unwrap(),
        filler.to_str().unwrap(),
        far.to_str().unwrap(),
        "-u",
        "_filler",
        "-lSystem",
    ]));
    // Hashing 130 MiB in a debug build is slow; the signature has its own
    // tests, except on macOS where the binary runs.
    if !host_can_run(arch) {
        args.push("-no_adhoc_codesign".to_owned());
    }
    let exe = dir.join("far");
    let mut ours: Vec<OsString> = args.iter().map(OsString::from).collect();
    ours.push("-o".into());
    ours.push((&exe).into());
    let (bytes, _) = link_bytes(&ours).unwrap_or_else(|e| panic!("{e}"));
    std::fs::write(&exe, &bytes).unwrap();
    make_executable(&exe);

    // main's calls go through thunks (adrp, add, br x16) that reach their
    // targets.
    // (Decoded here: llvm-objdump is slow on 130 MiB of code.)
    if let (Some(main_address), Some(far_address)) =
        (nm_address(&exe, "_main"), nm_address(&exe, "_far_function"))
    {
        assert!(far_address - main_address > 128 << 20);
        // __TEXT starts at file offset 0.
        let text = 0x1_0000_0000u64;
        let word = |address: u64| u32_le(&bytes, (address - text) as usize);
        let mut branches = Vec::new();
        for offset in (0..0x30).step_by(4) {
            let insn = word(main_address + offset);
            if insn & 0xfc00_0000 == 0x9400_0000 {
                let delta = (((insn & 0x03ff_ffff) << 6) as i32 >> 4) as i64;
                branches.push((main_address + offset).wrapping_add(delta as u64));
            }
        }
        assert_eq!(branches.len(), 2, "branches from main: {branches:x?}");
        let mut destinations = Vec::new();
        for thunk in branches {
            assert!(thunk.abs_diff(main_address) < 128 << 20);
            let adrp = word(thunk);
            let add = word(thunk + 4);
            assert_eq!(adrp & 0x9f00_001f, 0x9000_0010, "adrp x16 at {thunk:#x}");
            assert_eq!(add & 0xffc0_03ff, 0x9100_0210, "add x16, x16 at {thunk:#x}");
            assert_eq!(word(thunk + 8), 0xd61f_0200, "br x16 at {thunk:#x}");
            let pages = ((((adrp >> 29) & 3) | (((adrp >> 5) & 0x7ffff) << 2)) << 11) as i32 >> 11;
            let target = ((thunk & !0xfff) as i64 + ((pages as i64) << 12)) as u64
                + u64::from((add >> 10) & 0xfff);
            destinations.push(target);
        }
        assert!(destinations.contains(&far_address), "{destinations:x?}");
    }
    if host_can_run(arch) {
        assert_eq!(run(&exe).unwrap(), "near\nfar\n");
    }
}

#[test]
fn legacy_dyld_info() {
    for arch in ["arm64", "x86_64"] {
        if !clang_for(arch) {
            skip(
                "legacy_dyld_info",
                &format!("clang cannot target {arch}-apple-macos"),
            );
            continue;
        }
        let object = compile("legacy", "hello.c", arch, &[]);
        let root = syslibroot();
        let mut args = os(&[
            "-arch",
            arch,
            "-platform_version",
            "macos",
            "11.0",
            "11.0",
            "-syslibroot",
            root.to_str().unwrap(),
            object.to_str().unwrap(),
            "-lSystem",
        ]);
        let (bytes, _) = link_bytes(&args).unwrap();
        let commands: Vec<u32> = load_commands(&bytes).iter().map(|c| c.0).collect();
        assert!(commands.contains(&qld::macho::read::consts::LC_DYLD_INFO_ONLY));
        assert!(!commands.contains(&LC_DYLD_CHAINED_FIXUPS));
        let exe = scratch("legacy").join(format!("hello-{arch}"));
        std::fs::write(&exe, &bytes).unwrap();
        make_executable(&exe);
        if let Some(tables) = objdump(&["--macho", "--bind", "--rebase"], &exe) {
            assert!(tables.contains("_printf"), "{arch}: {tables}");
            assert!(
                tables
                    .lines()
                    .any(|l| l.contains("__data") && l.contains("pointer")),
                "{arch}: {tables}"
            );
        }
        if host_can_run(arch) {
            assert_eq!(run(&exe).unwrap(), "hello from qld 3 42\n");
        }

        // -fixup_chains overrides the deployment target.
        args.push("-fixup_chains".into());
        let (bytes, _) = link_bytes(&args).unwrap();
        let commands: Vec<u32> = load_commands(&bytes).iter().map(|c| c.0).collect();
        assert!(commands.contains(&LC_DYLD_CHAINED_FIXUPS));
    }
}

#[test]
fn bundles_and_export_lists() {
    let arch = "arm64";
    if !clang_for(arch) {
        skip(
            "bundles_and_export_lists",
            "clang cannot target arm64-apple-macos",
        );
        return;
    }
    let dir = scratch("bundle");
    let greet = compile("bundle", "greet.c", arch, &[]);
    let list = dir.join("exports.txt");
    std::fs::write(&list, "# only greet\n_greet\n").unwrap();
    let plist = dir.join("Info.plist");
    std::fs::write(&plist, "<plist>qld</plist>\n").unwrap();
    let mut args = base_args(arch);
    args.extend(strings(&[
        "-bundle",
        greet.to_str().unwrap(),
        "-exported_symbols_list",
        list.to_str().unwrap(),
        "-sectcreate",
        "__TEXT",
        "__info_plist",
        plist.to_str().unwrap(),
        "-lSystem",
    ]));
    let bundle = dir.join("greet.bundle");
    let bytes = link_and_compare(&args, &bundle);
    let needle = b"<plist>qld</plist>\n";
    assert!(
        bytes.windows(needle.len()).any(|w| w == needle),
        "-sectcreate contents missing"
    );
    let file = MachOFile::parse(&bytes, Source::new(&bundle)).unwrap();
    assert_eq!(file.header().file_type, qld::macho::read::consts::MH_BUNDLE);
    if let Some(trie) = objdump(&["--macho", "--exports-trie"], &bundle) {
        assert!(trie.contains("_greet"), "{trie}");
        assert!(!trie.contains("_greet_count"), "{trie}");
        assert!(!trie.contains("_greet_weak"), "{trie}");
    }
    if let Some(symbols) = Command::new(tool("nm"))
        .arg("-m")
        .arg(&bundle)
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
    {
        assert!(
            symbols
                .lines()
                .any(|l| l.contains("_greet_count") && l.contains("non-external")),
            "{symbols}"
        );
    }
}

#[test]
fn objective_c() {
    for arch in ["arm64", "x86_64"] {
        if !clang_for(arch) {
            skip(
                "objective_c",
                &format!("clang cannot target {arch}-apple-macos"),
            );
            continue;
        }
        let object = compile("objc", "objc.m", arch, &[]);
        for dead_strip in [false, true] {
            let mut args = base_args(arch);
            args.extend(strings(&[object.to_str().unwrap(), "-lobjc", "-lSystem"]));
            if dead_strip {
                args.push("-dead_strip".to_owned());
            }
            let exe = scratch("objc").join(format!("objc-{arch}-{dead_strip}"));
            link_and_compare(&args, &exe);
            if let Some(headers) = objdump(&["--macho", "--section-headers"], &exe) {
                for section in ["__objc_classlist", "__objc_catlist", "__objc_imageinfo"] {
                    assert!(headers.contains(section), "{arch}: {section}:\n{headers}");
                }
            }
            if host_can_run(arch) {
                assert_eq!(run(&exe).unwrap(), "objc 42\n");
            }
        }
    }
}

/// On macOS: Apple clang links through the `qld` binary (as `ld64.qld`,
/// with `-fuse-ld`) against the real SDK, and the program runs.
#[test]
fn clang_driver_uses_qld() {
    if !cfg!(target_os = "macos") {
        eprintln!("skipping clang_driver_uses_qld: needs macOS");
        return;
    }
    let dir = scratch("driver");
    let linker = dir.join("ld64.qld");
    let _ = std::fs::remove_file(&linker);
    #[cfg(unix)]
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_qld"), &linker).unwrap();
    for (source, expected) in [
        ("hello.c", "hello from qld 3 42\n"),
        ("exceptions.cpp", "caught 84 after 10 cleanups\n"),
    ] {
        let output = dir.join(source.replace('.', "-"));
        let compiler = if source.ends_with(".cpp") {
            "clang++"
        } else {
            "clang"
        };
        let result = Command::new(compiler)
            .arg(format!("-fuse-ld={}", linker.display()))
            .arg(data_dir().join(source))
            .arg("-o")
            .arg(&output)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{compiler} -fuse-ld=ld64.qld {source}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(run(&output).unwrap(), expected);
    }
}

#[test]
fn linker_options_from_archive_members() {
    let arch = "arm64";
    if !clang_for(arch) {
        skip(
            "linker_options_from_archive_members",
            "clang cannot target arm64-apple-macos",
        );
        return;
    }
    let dir = scratch("autolink");
    let member = compile("autolink", "autolink-arm64.s", arch, &[]);
    let archive = dir.join("librelease.a");
    if !make_archive(&archive, &[&member]) {
        skip(
            "linker_options_from_archive_members",
            "neither llvm-ar nor libtool works",
        );
        return;
    }
    let main = dir.join("main.c");
    std::fs::write(
        &main,
        "void release(void *);\nint main(void) { release(0); return 0; }\n",
    )
    .unwrap();
    let main_object = dir.join("main.o");
    assert!(tool_works(
        "clang",
        &[
            "--target=arm64-apple-macos13",
            "-c",
            main.to_str().unwrap(),
            "-o",
            main_object.to_str().unwrap()
        ]
    ));
    let mut args = base_args(arch);
    args.extend(strings(&[
        main_object.to_str().unwrap(),
        archive.to_str().unwrap(),
        "-lSystem",
    ]));
    let exe = dir.join("autolink");
    link_and_compare(&args, &exe);
    if let Some(dylibs) = objdump(&["--macho", "--dylibs-used"], &exe) {
        assert!(dylibs.contains("libc++"), "{dylibs}");
    }
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
