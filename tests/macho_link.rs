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
    LC_SYMTAB, LC_UUID, MH_EXECUTE,
};
use qld::macho::read::{ChainedFixups, MachOFile, Source};
use qld::output::hash::Sha256;

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

/// `llvm-<name>`, or on macOS `<name>` when only that exists (Xcode ships
/// the LLVM tools without the prefix: `objdump`, `lipo`, `dwarfdump`).
/// Elsewhere an unprefixed tool is GNU binutils, which cannot read Mach-O
/// the same way, so it is never used.
fn tool(name: &str) -> String {
    let prefixed = format!("llvm-{name}");
    if Command::new(&prefixed).arg("--version").output().is_ok() {
        return prefixed;
    }
    if cfg!(target_os = "macos") && Command::new(name).arg("--version").output().is_ok() {
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

/// A `nlist_64` record of `LC_SYMTAB`.
#[derive(Debug)]
struct Nlist {
    name: String,
    n_type: u8,
    n_value: u64,
}

impl Nlist {
    /// Defined in a section (and not a stab).
    fn is_defined(&self) -> bool {
        self.n_type & 0xe0 == 0 && self.n_type & 0x0e == 0x0e
    }

    fn is_external(&self) -> bool {
        self.n_type & 0xe0 == 0 && self.n_type & 0x01 != 0
    }
}

/// The symbol table of a 64-bit little-endian image, read with qld's reader
/// (not `nm`, whose output differs between LLVM, Xcode and GNU binutils).
fn symbols(data: &[u8]) -> Vec<Nlist> {
    let file = MachOFile::parse(data, Source::new(Path::new("out"))).unwrap();
    let Some(command) = file.find_command(LC_SYMTAB).unwrap() else {
        return Vec::new();
    };
    let symtab = command.symtab().unwrap();
    let strtab = &data[symtab.stroff as usize..][..symtab.strsize as usize];
    (0..symtab.nsyms as usize)
        .map(|i| {
            let at = symtab.symoff as usize + i * 16;
            let strx = u32_le(data, at) as usize;
            let name = strtab[strx..].split(|&b| b == 0).next().unwrap();
            Nlist {
                name: String::from_utf8_lossy(name).into_owned(),
                n_type: data[at + 4],
                n_value: u64::from_le_bytes(data[at + 8..at + 16].try_into().unwrap()),
            }
        })
        .collect()
}

/// The address of the defined symbol `name`.
fn symbol_address(data: &[u8], name: &str) -> Option<u64> {
    symbols(data)
        .into_iter()
        .find(|s| s.is_defined() && s.name == name)
        .map(|s| s.n_value)
}

/// An import of `LC_DYLD_CHAINED_FIXUPS`.
#[derive(Debug)]
struct Import {
    name: String,
    lib_ordinal: i32,
    weak: bool,
}

/// The chained-fixup imports, read with qld's reader (Xcode's `objdump`
/// prints neither the names nor the weak-lookup binds).
fn chained_imports(data: &[u8]) -> Vec<Import> {
    let source = Source::new(Path::new("out"));
    let file = MachOFile::parse(data, source).unwrap();
    let command = file
        .find_command(LC_DYLD_CHAINED_FIXUPS)
        .unwrap()
        .expect("LC_DYLD_CHAINED_FIXUPS");
    let blob = command.linkedit_data().unwrap();
    let bytes = &data[blob.dataoff as usize..][..blob.datasize as usize];
    let fixups =
        ChainedFixups::parse(bytes, u64::from(blob.dataoff), file.endian(), source).unwrap();
    fixups
        .imports()
        .map(|import| {
            let import = import.unwrap();
            Import {
                name: String::from_utf8_lossy(import.name).into_owned(),
                lib_ordinal: import.lib_ordinal,
                weak: import.weak_import,
            }
        })
        .collect()
}

/// The import named `name`, which must exist.
fn import<'a>(imports: &'a [Import], name: &str) -> &'a Import {
    imports
        .iter()
        .find(|i| i.name == name)
        .unwrap_or_else(|| panic!("no import {name}: {imports:?}"))
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
            // Linker-specific extras: lld's lazy binding for legacy
            // (`LC_DYLD_INFO_ONLY`) outputs, which qld binds at load time.
            if matches!(
                name,
                "__unwind_info" | "__eh_frame" | "__stub_helper" | "__la_symbol_ptr"
            ) {
                return None;
            }
            // lld merges the literal sections into one `__literals`; ld64
            // and qld keep `__literal4`, `__literal8` and `__literal16`.
            Some(if name.starts_with("__literal") {
                "__literals".to_owned()
            } else {
                name.to_owned()
            })
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
        let imports = chained_imports(&bytes);
        let printf = import(&imports, "_printf");
        assert_eq!(printf.lib_ordinal, 1, "{arch}: {imports:?}");
        assert!(!printf.weak, "{arch}: {imports:?}");

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
        // References to an exported weak definition bind through weak
        // lookup (BIND_SPECIAL_DYLIB_WEAK_LOOKUP), so a strong definition
        // elsewhere can override it.
        let imports = chained_imports(&bytes);
        assert_eq!(
            import(&imports, "_greet_weak").lib_ordinal,
            -3,
            "{arch}: weak definition not bound through weak lookup: {imports:?}"
        );

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
        let bytes = link_and_compare(&args, &exe);
        let imports = chained_imports(&bytes);
        assert!(
            import(&imports, "_missing_weak").weak,
            "{arch}: {imports:?}"
        );
        // libgreet is the first dylib on the command line.
        assert_eq!(
            import(&imports, "_greet").lib_ordinal,
            1,
            "{arch}: {imports:?}"
        );
        let file = MachOFile::parse(&bytes, Source::new(&exe)).unwrap();
        let first = file
            .load_commands()
            .map(Result::unwrap)
            .find(|c| c.cmd == LC_LOAD_DYLIB)
            .unwrap();
        assert_eq!(first.dylib().unwrap().name, b"@rpath/libgreet.dylib");
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
        let bytes = link_and_compare(&args, &exe);
        let function = symbol_address(&bytes, "_call_through").unwrap();
        // The FDE in __eh_frame covers call_through.
        if let Ok(frames) = Command::new(tool("dwarfdump"))
            .arg("--eh-frame")
            .arg(&exe)
            .output()
            && frames.status.success()
        {
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
        let bytes = link_and_compare(&args, &exe);
        for (name, kept) in [
            ("_main", true),
            ("_used_data", true),
            ("_kept_by_attribute", true),
            ("_unused_function", false),
            ("_unused_caller", false),
            ("_unused_data", false),
        ] {
            assert_eq!(
                symbol_address(&bytes, name).is_some(),
                kept,
                "{arch}: {name} should {}be kept",
                if kept { "" } else { "not " }
            );
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
    {
        let main_address = symbol_address(&bytes, "_main").unwrap();
        let far_address = symbol_address(&bytes, "_far_function").unwrap();
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
    // Symbols left out of the export list stay in the symbol table, but not
    // as externals.
    let table = symbols(&bytes);
    let count = table
        .iter()
        .find(|s| s.is_defined() && s.name == "_greet_count")
        .unwrap_or_else(|| panic!("_greet_count missing: {table:?}"));
    assert!(!count.is_external(), "{count:?}");
    let greet = table
        .iter()
        .find(|s| s.is_defined() && s.name == "_greet")
        .unwrap_or_else(|| panic!("_greet missing: {table:?}"));
    assert!(greet.is_external(), "{greet:?}");
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

/// `_objc_msgSend$<selector>` calls (Apple clang's selector stubs, which
/// upstream clang does not emit, hence the assembly) get stubs, selector
/// references and method names synthesized by the linker.
#[test]
fn objective_c_selector_stubs() {
    for arch in ["arm64", "x86_64"] {
        if !clang_for(arch) {
            skip(
                "objective_c_selector_stubs",
                &format!("clang cannot target {arch}-apple-macos"),
            );
            continue;
        }
        let object = compile(
            "selector_stubs",
            &format!("selector_stubs-{arch}.s"),
            arch,
            &[],
        );
        for dead_strip in [false, true] {
            let mut args = base_args(arch);
            args.extend(strings(&[object.to_str().unwrap(), "-lobjc", "-lSystem"]));
            if dead_strip {
                args.push("-dead_strip".to_owned());
            }
            let exe = scratch("selector_stubs").join(format!("stubs-{arch}-{dead_strip}"));
            let bytes = link_and_compare(&args, &exe);
            let file = MachOFile::parse(&bytes, Source::new(&exe)).unwrap();
            let mut sections = BTreeSet::new();
            for command in file.load_commands() {
                if let Ok(segment) = command.unwrap().segment() {
                    for section in segment.sections.iter() {
                        sections.insert((
                            String::from_utf8_lossy(section.segname).into_owned(),
                            String::from_utf8_lossy(section.sectname).into_owned(),
                            section.size,
                        ));
                    }
                }
            }
            let stub_size = if arch == "arm64" { 32 } else { 13 };
            for wanted in [
                ("__TEXT", "__objc_stubs", 2 * stub_size),
                ("__DATA", "__objc_selrefs", 16),
                ("__TEXT", "__objc_methname", 27),
            ] {
                let wanted = (wanted.0.to_owned(), wanted.1.to_owned(), wanted.2);
                assert!(
                    sections.contains(&wanted),
                    "{arch}: {wanted:?} in {sections:?}"
                );
            }
            let table = symbols(&bytes);
            for stub in ["_objc_msgSend$description", "_objc_msgSend$initWithCount:"] {
                let symbol = table
                    .iter()
                    .find(|s| s.is_defined() && s.name == stub)
                    .unwrap_or_else(|| panic!("{arch}: {stub} missing: {table:?}"));
                assert!(!symbol.is_external(), "{arch}: {symbol:?}");
            }
            let imports = chained_imports(&bytes);
            assert!(
                imports.iter().any(|i| i.name == "_objc_msgSend"),
                "{arch}: {imports:?}"
            );
            if host_can_run(arch) {
                run(&exe).unwrap();
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
        let stderr = String::from_utf8_lossy(&result.stderr);
        // Until `crate::link` dispatches Mach-O targets to `macho::link`,
        // the binary reports Mach-O output as not implemented.
        if !result.status.success() && stderr.contains("not implemented yet: MachO output") {
            skip(
                "clang_driver_uses_qld",
                "the qld binary does not link Mach-O yet",
            );
            return;
        }
        assert!(
            result.status.success(),
            "{compiler} -fuse-ld=ld64.qld {source}: {stderr}"
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
fn order_file() {
    let arch = "arm64";
    if !clang_for(arch) {
        skip("order_file", "clang cannot target arm64-apple-macos");
        return;
    }
    let dir = scratch("order");
    let object = compile("order", "dead_strip.c", arch, &[]);
    let order = dir.join("order.txt");
    std::fs::write(
        &order,
        "# callers first\narm64:_unused_caller\n_main\nx86_64:_unused_function\n",
    )
    .unwrap();
    let mut args = base_args(arch);
    args.extend(strings(&[
        object.to_str().unwrap(),
        "-order_file",
        order.to_str().unwrap(),
        "-lSystem",
    ]));
    let exe = dir.join("ordered");
    let bytes = link_and_compare(&args, &exe);
    {
        let caller = symbol_address(&bytes, "_unused_caller").unwrap();
        let main = symbol_address(&bytes, "_main").unwrap();
        let function = symbol_address(&bytes, "_unused_function").unwrap();
        assert!(
            caller < main && main < function,
            "{caller:#x} {main:#x} {function:#x}"
        );
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
    // BIND_SPECIAL_DYLIB_FLAT_LOOKUP.
    let imports = chained_imports(&bytes);
    assert_eq!(import(&imports, "_missing").lib_ordinal, -2, "{imports:?}");
}

/// The sections of a Mach-O image, in load command order, as (segment,
/// section, file offset, size).
fn section_list(data: &[u8]) -> Vec<(String, String, usize, usize)> {
    let file = MachOFile::parse(data, Source::new(Path::new("out"))).unwrap();
    let mut sections = Vec::new();
    for command in file.load_commands() {
        let command = command.unwrap();
        if command.cmd == qld::macho::read::consts::LC_SEGMENT_64 {
            for section in command.segment().unwrap().sections.iter() {
                sections.push((
                    String::from_utf8_lossy(section.segname).into_owned(),
                    String::from_utf8_lossy(section.sectname).into_owned(),
                    section.offset as usize,
                    section.size as usize,
                ));
            }
        }
    }
    sections
}

/// The names of the sections of a Mach-O image, in load command order.
fn section_names(data: &[u8]) -> Vec<String> {
    section_list(data).into_iter().map(|s| s.1).collect()
}

/// The contents of section `name` (the first with that name).
fn section_contents<'a>(data: &'a [u8], name: &str) -> Option<&'a [u8]> {
    section_list(data)
        .into_iter()
        .find(|s| s.1 == name)
        .map(|(_, _, offset, size)| &data[offset..offset + size])
}

/// How many times `needle` occurs in `haystack`.
fn occurrences(haystack: &[u8], needle: &[u8]) -> usize {
    haystack
        .windows(needle.len())
        .filter(|w| *w == needle)
        .count()
}

/// Stores of immediates to statics (x86_64 `SIGNED_1`/`SIGNED_4` against a
/// local symbol that starts its atom) reach the static itself, not the
/// bytes before it.
#[test]
fn pcrel_immediate_stores() {
    for arch in ["arm64", "x86_64"] {
        if !clang_for(arch) {
            skip(
                "pcrel_immediate_stores",
                &format!("clang cannot target {arch}-apple-macos"),
            );
            continue;
        }
        let object = compile("statics", "statics.c", arch, &[]);
        let mut args = base_args(arch);
        args.extend(strings(&[object.to_str().unwrap(), "-lSystem"]));
        let exe = scratch("statics").join(format!("statics-{arch}"));
        link_and_compare(&args, &exe);
        if let Some(disassembly) = objdump(&["--macho", "-d"], &exe) {
            // llvm-objdump symbolizes RIP-relative operands: an operand one
            // byte before the static shows as `_flag-1`.
            for name in ["_flag", "_counter", "_wide"] {
                assert!(
                    !disassembly.contains(&format!("{name}-")),
                    "{arch}: {name} is missed:\n{disassembly}"
                );
            }
        }
        if host_can_run(arch) {
            assert_eq!(run(&exe).unwrap(), "1 12345678 1122334455667788\n");
        }
    }
}

/// Identical C strings and floating-point literals from two objects are
/// merged, as ld64 and lld merge them: one copy in the output, and both
/// objects' references resolve to it.
#[test]
fn literal_deduplication() {
    for arch in ["arm64", "x86_64"] {
        if !clang_for(arch) {
            skip(
                "literal_deduplication",
                &format!("clang cannot target {arch}-apple-macos"),
            );
            continue;
        }
        let a = compile("literals", "literals_a.c", arch, &[]);
        let b = compile("literals", "literals_b.c", arch, &[]);
        let mut args = base_args(arch);
        args.extend(strings(&[
            a.to_str().unwrap(),
            b.to_str().unwrap(),
            "-lSystem",
        ]));
        let exe = scratch("literals").join(format!("literals-{arch}"));
        let bytes = link_and_compare(&args, &exe);
        let cstrings = section_contents(&bytes, "__cstring").expect("__cstring");
        assert_eq!(
            occurrences(cstrings, b"shared string\0"),
            1,
            "{arch}: {cstrings:?}"
        );
        assert_eq!(occurrences(cstrings, b"only in a\0"), 1, "{arch}");
        if arch == "x86_64" {
            // The multiplier is a `__literal8` in both objects.
            let literals = section_contents(&bytes, "__literal8").expect("__literal8");
            assert_eq!(
                // 3.14159265358979
                occurrences(literals, &0x4009_21fb_5444_2d11_u64.to_le_bytes()),
                1,
                "{arch}: {literals:?}"
            );
        }
        let reference = PathBuf::from(format!("{}-lld", exe.display()));
        if let Ok(theirs) = std::fs::read(&reference) {
            assert_eq!(
                section_contents(&theirs, "__cstring").map(<[u8]>::len),
                Some(cstrings.len()),
                "{arch}: __cstring size, qld vs ld64.lld"
            );
        }
        if host_can_run(arch) {
            assert_eq!(
                run(&exe).unwrap(),
                "shared string shared string 1 only in a 9.42478\n"
            );
        }
    }
}

/// Merges `objects` with `-r` into `output` and returns the bytes.
fn link_relocatable(arch: &str, objects: &[&Path], output: &Path) -> Vec<u8> {
    let mut args = os(&[
        "-arch",
        arch,
        "-platform_version",
        "macos",
        "13.0",
        "13.0",
        "-r",
    ]);
    for object in objects {
        args.push(object.into());
    }
    args.extend(os(&["-o", output.to_str().unwrap()]));
    let (bytes, _) = link_bytes(&args).unwrap_or_else(|e| panic!("-r failed: {e}"));
    std::fs::write(output, &bytes).unwrap();
    bytes
}

/// The external symbols of an image or object: (name, defined), sorted.
fn external_symbols(data: &[u8]) -> Vec<(String, bool)> {
    let mut out: Vec<(String, bool)> = symbols(data)
        .into_iter()
        .filter(Nlist::is_external)
        .map(|s| {
            let defined = s.n_type & 0x0e != 0;
            (s.name, defined)
        })
        .collect();
    out.sort();
    out
}

/// Checks a relocatable object with qld's reader: an `MH_OBJECT` whose
/// relocations all decode and pair, and whose sections atomize.
fn check_object(data: &[u8], subsections: bool) {
    use qld::macho::read::{Atomization, ObjectFile};
    let file = MachOFile::parse(data, Source::new(Path::new("merged.o"))).unwrap();
    assert_eq!(file.header().file_type, qld::macho::read::consts::MH_OBJECT);
    assert_eq!(
        file.header().flags & qld::macho::read::consts::MH_SUBSECTIONS_VIA_SYMBOLS != 0,
        subsections
    );
    let object = ObjectFile::parse(data, Source::new(Path::new("merged.o"))).unwrap();
    Atomization::new(&object).unwrap();
    for index in 0..object.sections().len() {
        for relocation in object.paired_relocations(index).unwrap() {
            relocation.unwrap();
        }
    }
}

/// `-r`: two C++ objects (weak definitions in both, exceptions, statics, a
/// thread-local, string literals) merged into one object, which is then
/// linked into an executable. The executable matches a link of the two
/// objects themselves; `ld64.lld` (which has no `-r`) links the merged
/// object too and is compared. On macOS the executable runs, the merged
/// object's external symbols match Apple's `ld -r`, and Apple's linker
/// links the merged object into a program that runs as well.
#[test]
fn relocatable_output() {
    const EXPECTED: &str = "hello from a total -50 counter 3 local 5 tls 7 flag 1\n";
    for arch in ["arm64", "x86_64"] {
        if !clang_for(arch) {
            skip(
                "relocatable_output",
                &format!("clang cannot target {arch}-apple-macos"),
            );
            continue;
        }
        let dir = scratch("relocatable");
        let a = compile("relocatable", "relocatable_a.cpp", arch, &[]);
        let b = compile("relocatable", "relocatable_b.cpp", arch, &[]);
        let merged = dir.join(format!("merged-{arch}.o"));
        let bytes = link_relocatable(arch, &[&a, &b], &merged);
        check_object(&bytes, true);
        assert_eq!(
            link_relocatable(arch, &[&a, &b], &merged),
            bytes,
            "{arch}: -r output is not deterministic"
        );

        // One copy of each weak definition; undefined references stay.
        let externals = external_symbols(&bytes);
        let count = |name: &str| externals.iter().filter(|s| s.0 == name).count();
        assert_eq!(count("__ZNK3BoxIiE5twiceEv"), 1, "{arch}: {externals:?}");
        assert_eq!(count("__ZZ14shared_countervE5count"), 1, "{arch}");
        for undefined in ["___cxa_throw", "___gxx_personality_v0", "_printf"] {
            assert!(
                externals.iter().any(|s| s.0 == undefined && !s.1),
                "{arch}: {undefined} not undefined: {externals:?}"
            );
        }
        let names = section_names(&bytes);
        for wanted in [
            "__text",
            "__compact_unwind",
            "__gcc_except_tab",
            "__cstring",
        ] {
            assert!(
                names.iter().any(|n| n == wanted),
                "{arch}: no {wanted} in {names:?}"
            );
        }
        let cstrings = section_contents(&bytes, "__cstring").unwrap();
        assert_eq!(occurrences(cstrings, b"hello from a\0"), 1, "{arch}");

        // Linked, the merged object gives the program the objects give.
        let mut args = base_args(arch);
        args.extend(strings(&[merged.to_str().unwrap(), "-lc++", "-lSystem"]));
        let exe = dir.join(format!("merged-{arch}"));
        link_and_compare(&args, &exe);
        let mut args = base_args(arch);
        args.extend(strings(&[
            a.to_str().unwrap(),
            b.to_str().unwrap(),
            "-lc++",
            "-lSystem",
        ]));
        let direct = dir.join(format!("direct-{arch}"));
        link_and_compare(&args, &direct);
        if let (Some(ours), Some(theirs)) = (summarize(&exe), summarize(&direct)) {
            assert_eq!(ours, theirs, "{arch}: linked from -r vs from the objects");
            // `Box<int>::twice` is `.weak_def_can_be_hidden` in both
            // objects: not exported, as with ld64.
            assert!(
                !ours.exports.contains("__ZNK3BoxIiE5twiceEv"),
                "{arch}: {:?}",
                ours.exports
            );
        }
        if let (Some(ours), Some(theirs)) = (
            objdump(&["--macho", "--section-headers"], &exe),
            objdump(&["--macho", "--section-headers"], &direct),
        ) {
            // The same sections at the same addresses (the stubs and
            // `__got` slots may come in another order).
            let body = |text: &str| text.lines().skip(1).collect::<Vec<_>>().join("\n");
            assert_eq!(body(&ours), body(&theirs), "{arch}: layouts differ");
        }
        if host_can_run(arch) {
            assert_eq!(run(&exe).unwrap(), EXPECTED, "{arch}");
        }

        if cfg!(target_os = "macos") && host_can_run(arch) {
            apple_relocatable(arch, &[&a, &b], &bytes, &merged, EXPECTED);
        }
    }
}

/// On macOS: compares the external symbols of `ours` (qld's `-r` output,
/// at `merged`) with Apple's `ld -r` of the same `objects`, then links
/// `merged` with Apple's linker and runs it.
fn apple_relocatable(arch: &str, objects: &[&Path], ours: &[u8], merged: &Path, expected: &str) {
    let dir = merged.parent().unwrap();
    let apple = dir.join(format!("apple-{arch}.o"));
    let status = Command::new("ld")
        .args(["-r", "-arch", arch])
        .args(objects)
        .arg("-o")
        .arg(&apple)
        .status();
    match status {
        Ok(status) if status.success() => {
            let theirs = std::fs::read(&apple).unwrap();
            assert_eq!(
                external_symbols(ours),
                external_symbols(&theirs),
                "{arch}: external symbols, qld -r vs Apple ld -r"
            );
        }
        _ => skip("relocatable_output", "Apple ld -r failed or is missing"),
    }
    let exe = dir.join(format!("apple-linked-{arch}"));
    let status = Command::new("clang++")
        .args(["-arch", arch])
        .arg(merged)
        .arg("-o")
        .arg(&exe)
        .status()
        .unwrap();
    assert!(
        status.success(),
        "{arch}: Apple's linker rejects qld's -r output"
    );
    assert_eq!(run(&exe).unwrap(), expected, "{arch}: linked by Apple's ld");
}

/// More `-r` inputs: Objective-C metadata (selector references into merged
/// method names), `LC_LINKER_OPTION` carried to the final link, x86_64
/// `SIGNED_n` stores, private externs (made local, or kept with
/// `-keep_private_externs`), and debug information (dropped, with a
/// warning).
#[test]
fn relocatable_variants() {
    for arch in ["arm64", "x86_64"] {
        if !clang_for(arch) {
            skip(
                "relocatable_variants",
                &format!("clang cannot target {arch}-apple-macos"),
            );
            continue;
        }
        let dir = scratch("relocatable_variants");

        // Objective-C, and statics stored to with immediates.
        let objc = compile("relocatable_variants", "objc.m", arch, &[]);
        let merged = dir.join(format!("objc-{arch}.o"));
        let bytes = link_relocatable(arch, &[&objc], &merged);
        check_object(&bytes, true);
        let mut args = base_args(arch);
        args.extend(strings(&[merged.to_str().unwrap(), "-lobjc", "-lSystem"]));
        let exe = dir.join(format!("objc-{arch}"));
        link_and_compare(&args, &exe);
        if host_can_run(arch) {
            assert_eq!(run(&exe).unwrap(), "objc 42\n");
        }
        let statics = compile("relocatable_variants", "statics.c", arch, &[]);
        let merged = dir.join(format!("statics-{arch}.o"));
        link_relocatable(arch, &[&statics], &merged);
        let mut args = base_args(arch);
        args.extend(strings(&[merged.to_str().unwrap(), "-lSystem"]));
        let exe = dir.join(format!("statics-{arch}"));
        link_and_compare(&args, &exe);
        if host_can_run(arch) {
            assert_eq!(run(&exe).unwrap(), "1 12345678 1122334455667788\n");
        }

        // DWARF unwind information: `__eh_frame` rebuilt, its pointers
        // PC-relative without relocations (arm64 inputs have them).
        let main = compile("relocatable_variants", "dwarf_unwind.cpp", arch, &[]);
        let asm = compile(
            "relocatable_variants",
            &format!("dwarf-{arch}.s"),
            arch,
            &[],
        );
        let merged = dir.join(format!("dwarf-{arch}.o"));
        let bytes = link_relocatable(arch, &[&main, &asm], &merged);
        check_object(&bytes, true);
        assert!(section_names(&bytes).iter().any(|n| n == "__eh_frame"));
        let mut args = base_args(arch);
        args.extend(strings(&[merged.to_str().unwrap(), "-lc++", "-lSystem"]));
        let exe = dir.join(format!("dwarf-{arch}"));
        let linked = link_and_compare(&args, &exe);
        let function = symbol_address(&linked, "_call_through").unwrap();
        if let Ok(frames) = Command::new(tool("dwarfdump"))
            .arg("--eh-frame")
            .arg(&exe)
            .output()
            && frames.status.success()
        {
            let frames = String::from_utf8_lossy(&frames.stdout);
            let wanted = format!("pc={function:08x}...");
            assert!(
                frames.contains(&wanted),
                "{arch}: no FDE for {wanted}:\n{frames}"
            );
        }
        if host_can_run(arch) {
            assert_eq!(run(&exe).unwrap(), "dwarf 7\n");
            // Apple's linker takes the rebuilt `__eh_frame` too.
            let apple = dir.join(format!("dwarf-apple-{arch}"));
            let status = Command::new("clang++")
                .args(["-arch", arch])
                .arg(&merged)
                .arg("-o")
                .arg(&apple)
                .status()
                .unwrap();
            assert!(
                status.success(),
                "{arch}: Apple's linker rejects the object"
            );
            assert_eq!(run(&apple).unwrap(), "dwarf 7\n");
        }

        // Private externs, and debug information.
        let source = dir.join("hidden.c");
        std::fs::write(
            &source,
            "__attribute__((visibility(\"hidden\"))) int hidden_answer(void) { return 42; }\n\
             int visible_answer(void) { return hidden_answer(); }\n",
        )
        .unwrap();
        let hidden = dir.join(format!("hidden-{arch}.o"));
        assert!(tool_works(
            "clang",
            &[
                &format!("--target={arch}-apple-macos13"),
                "-g",
                "-c",
                source.to_str().unwrap(),
                "-o",
                hidden.to_str().unwrap()
            ]
        ));
        for keep in [false, true] {
            let merged = dir.join(format!("hidden-{arch}-{keep}.o"));
            let mut args = os(&[
                "-arch",
                arch,
                "-platform_version",
                "macos",
                "13.0",
                "13.0",
                "-r",
            ]);
            args.push(hidden.clone().into());
            if keep {
                args.push("-keep_private_externs".into());
            }
            args.extend(os(&["-o", merged.to_str().unwrap()]));
            let (bytes, messages) = link_bytes(&args).unwrap();
            assert!(
                messages.iter().any(|m| m.contains("DWARF")),
                "{arch}: no warning about debug sections: {messages:?}"
            );
            assert!(
                !section_names(&bytes)
                    .iter()
                    .any(|n| n.starts_with("__debug")),
                "{arch}: debug sections copied"
            );
            let symbol = symbols(&bytes)
                .into_iter()
                .find(|s| s.name == "_hidden_answer")
                .expect("_hidden_answer");
            // N_PEXT | N_EXT when kept, a plain local otherwise.
            assert_eq!(
                symbol.n_type & 0x11,
                if keep { 0x11 } else { 0 },
                "{arch}: keep_private_externs {keep}: {:#x}",
                symbol.n_type
            );
        }

        // LC_LINKER_OPTION reaches the final link through -r.
        if arch == "arm64" {
            let autolink = compile("relocatable_variants", "autolink-arm64.s", arch, &[]);
            let main = dir.join("autolink_main.c");
            std::fs::write(
                &main,
                "void release(void *);\nint main(void) { release(0); return 0; }\n",
            )
            .unwrap();
            let main_object = dir.join("autolink_main.o");
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
            let merged = dir.join("autolink.o");
            let bytes = link_relocatable(arch, &[&main_object, &autolink], &merged);
            let options: Vec<u32> = load_commands(&bytes).iter().map(|c| c.0).collect();
            assert!(
                options.contains(&qld::macho::read::consts::LC_LINKER_OPTION),
                "{options:x?}"
            );
            let mut args = base_args(arch);
            args.extend(strings(&[merged.to_str().unwrap(), "-lSystem"]));
            let exe = dir.join("autolink");
            link_and_compare(&args, &exe);
            if let Some(dylibs) = objdump(&["--macho", "--dylibs-used"], &exe) {
                assert!(dylibs.contains("libc++"), "{dylibs}");
            }
        }
    }
}

/// Runs `binary` with `args` on macOS and returns its stdout.
fn run_with(binary: &Path, args: &[&Path]) -> Option<String> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let output = Command::new(binary).args(args).output().unwrap();
    assert!(
        output.status.success(),
        "{} failed: {:?}\n{}{}",
        binary.display(),
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// `-bundle -bundle_loader <executable>`: the bundle's references to the
/// executable bind with the main-executable ordinal, and the executable
/// gets no load command. On macOS the executable loads the bundle, which
/// calls back into it.
#[test]
fn bundle_loader() {
    for arch in ["arm64", "x86_64"] {
        if !clang_for(arch) {
            skip(
                "bundle_loader",
                &format!("clang cannot target {arch}-apple-macos"),
            );
            continue;
        }
        let dir = scratch("bundle_loader");
        let host = compile("bundle_loader", "bundle_host.c", arch, &[]);
        let plugin = compile("bundle_loader", "bundle_plugin.c", arch, &[]);
        let exe = dir.join(format!("host-{arch}"));
        let mut args = base_args(arch);
        args.extend(strings(&[host.to_str().unwrap(), "-lSystem"]));
        link_and_compare(&args, &exe);

        let bundle = dir.join(format!("plugin-{arch}.bundle"));
        let mut args = base_args(arch);
        args.extend(strings(&[
            "-bundle",
            "-bundle_loader",
            exe.to_str().unwrap(),
            plugin.to_str().unwrap(),
            "-lSystem",
        ]));
        let bytes = link_and_compare(&args, &bundle);
        let file = MachOFile::parse(&bytes, Source::new(&bundle)).unwrap();
        assert_eq!(file.header().file_type, qld::macho::read::consts::MH_BUNDLE);
        let imports = chained_imports(&bytes);
        // BIND_SPECIAL_DYLIB_MAIN_EXECUTABLE.
        assert_eq!(
            import(&imports, "_host_value").lib_ordinal,
            -1,
            "{imports:?}"
        );
        let loads = file
            .load_commands()
            .map(Result::unwrap)
            .filter(|c| c.cmd == LC_LOAD_DYLIB)
            .count();
        assert_eq!(loads, 1, "{arch}: only libSystem is loaded");
        if host_can_run(arch) {
            assert_eq!(run_with(&exe, &[&bundle]).unwrap(), "bundle 42\n");
        }
    }
}

/// `-init` (an `LC_ROUTINES_64` initializer that dyld runs before the
/// dylib's other initializers) and `-alias` (a second name for a symbol).
/// `ld64.lld` ignores `-init`, so only the rest is compared with it.
#[test]
fn init_and_alias() {
    for arch in ["arm64", "x86_64"] {
        if !clang_for(arch) {
            skip(
                "init_and_alias",
                &format!("clang cannot target {arch}-apple-macos"),
            );
            continue;
        }
        let dir = scratch("init").join(arch);
        std::fs::create_dir_all(&dir).unwrap();
        let lib = compile("init", "init_lib.c", arch, &[]);
        let client = compile("init", "init_client.c", arch, &[]);
        let dylib = dir.join("libinit.dylib");
        let mut args = base_args(arch);
        args.extend(strings(&[
            "-dylib",
            "-install_name",
            "@rpath/libinit.dylib",
            "-init",
            "_lib_init",
            "-alias",
            "_lib_ready",
            "_lib_ready_alias",
            lib.to_str().unwrap(),
            "-lSystem",
        ]));
        let bytes = link_and_compare(&args, &dylib);
        let (_, at, _) = load_commands(&bytes)
            .into_iter()
            .find(|c| c.0 == qld::macho::read::consts::LC_ROUTINES_64)
            .expect("LC_ROUTINES_64");
        let init_address = u64::from_le_bytes(bytes[at + 8..at + 16].try_into().unwrap());
        assert_eq!(Some(init_address), symbol_address(&bytes, "_lib_init"));
        assert_eq!(
            symbol_address(&bytes, "_lib_ready_alias"),
            symbol_address(&bytes, "_lib_ready"),
            "{arch}: the alias is not at its target"
        );
        if let Some(trie) = objdump(&["--macho", "--exports-trie"], &dylib) {
            assert!(trie.contains("_lib_ready_alias"), "{trie}");
        }

        let exe = dir.join("init_client");
        let mut args = base_args(arch);
        args.extend(strings(&[
            client.to_str().unwrap(),
            &format!("-L{}", dir.display()),
            "-linit",
            "-rpath",
            "@executable_path",
            "-lSystem",
        ]));
        link_and_compare(&args, &exe);
        if host_can_run(arch) {
            assert_eq!(run(&exe).unwrap(), "init ran\nready 42 42\n");
        }
    }
    let error = link_bytes(&os(&["-arch", "arm64", "-init", "_main", "a.o"])).unwrap_err();
    assert!(
        error.contains("-init can only be used with -dylib"),
        "{error}"
    );
}

/// `-flat_namespace`: imports are looked up by name (ordinal
/// `BIND_SPECIAL_DYLIB_FLAT_LOOKUP`), and the header has neither
/// `MH_TWOLEVEL` nor `MH_NOUNDEFS`, as with ld64 and lld.
#[test]
fn flat_namespace() {
    for arch in ["arm64", "x86_64"] {
        if !clang_for(arch) {
            skip(
                "flat_namespace",
                &format!("clang cannot target {arch}-apple-macos"),
            );
            continue;
        }
        let object = compile("flat", "hello.c", arch, &[]);
        let exe = scratch("flat").join(format!("hello-{arch}"));
        let mut args = base_args(arch);
        args.extend(strings(&[
            "-flat_namespace",
            object.to_str().unwrap(),
            "-lSystem",
        ]));
        let bytes = link_and_compare(&args, &exe);
        let flags = MachOFile::parse(&bytes, Source::new(&exe))
            .unwrap()
            .header()
            .flags;
        assert_eq!(
            flags & qld::macho::read::consts::MH_TWOLEVEL,
            0,
            "{flags:#x}"
        );
        assert_eq!(
            flags & qld::macho::read::consts::MH_NOUNDEFS,
            0,
            "{flags:#x}"
        );
        let imports = chained_imports(&bytes);
        assert_eq!(import(&imports, "_printf").lib_ordinal, -2, "{imports:?}");
        if host_can_run(arch) {
            assert_eq!(run(&exe).unwrap(), "hello from qld 3 42\n");
        }
    }
}

/// Whether `rustc` has the standard library for `target`.
fn rust_std_for(target: &str) -> bool {
    let Ok(output) = Command::new("rustc")
        .args(["--print", "target-libdir", "--target", target])
        .output()
    else {
        return false;
    };
    let dir = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    output.status.success()
        && std::fs::read_dir(dir).is_ok_and(|entries| {
            entries
                .flatten()
                .any(|e| e.file_name().to_string_lossy().starts_with("libstd-"))
        })
}

/// How `rustc` reaches the linker.
#[derive(Clone, Copy)]
enum RustLinker<'a> {
    /// `-C linker=clang -C link-arg=-fuse-ld=<path>`: Apple clang's driver
    /// runs the linker, as `cargo` does on macOS.
    Clang(&'a Path),
    /// `-C linker-flavor=ld64.lld -C linker=<path>`: rustc runs the linker
    /// itself with an ld64 command line, against `syslibroot()`.
    Direct(&'a Path),
}

/// Builds `rust_std.rs` for `target` into `output` with `linker`, and
/// with `MACOSX_DEPLOYMENT_TARGET=deployment` when given (rustc's default
/// for arm64 is 11.0, which gets `LC_DYLD_INFO_ONLY`).
fn rustc_link(
    target: &str,
    linker: RustLinker<'_>,
    deployment: Option<&str>,
    link_args: &[&str],
    output: &Path,
) -> Result<(), String> {
    let mut command = Command::new("rustc");
    command
        .args(["--edition", "2021", "-O", "--target", target])
        .arg(data_dir().join("rust_std.rs"))
        .arg("-o")
        .arg(output)
        .current_dir(output.parent().unwrap());
    for arg in link_args {
        command.arg("-C").arg(format!("link-arg={arg}"));
    }
    match linker {
        RustLinker::Clang(path) => {
            command
                .args(["-C", "linker=clang", "-C"])
                .arg(format!("link-arg=-fuse-ld={}", path.display()));
        }
        RustLinker::Direct(path) => {
            command
                .args(["-C", "linker-flavor=ld64.lld", "-C"])
                .arg(format!("linker={}", path.display()))
                .env("SDKROOT", syslibroot());
        }
    }
    match deployment {
        Some(version) => command.env("MACOSX_DEPLOYMENT_TARGET", version),
        None => command.env_remove("MACOSX_DEPLOYMENT_TARGET"),
    };
    let result = command.output().map_err(|e| e.to_string())?;
    if result.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&result.stderr).into_owned())
    }
}

const RUST_STD_OUTPUT: &str = "\
threads: [(1, 1), (2, 4), (3, 9), (4, 16)] tls-len 12 main 0
panics: caught 3 sum 54
thread panic: true
words: [(\"the\", 3), (\"brown\", 1), (\"dog\", 1)] pi    3.142 hex 0xff float 1.2345e3
";

/// A Rust program using threads, `panic=unwind` with `catch_unwind`,
/// thread-locals, formatting and `HashMap`, linked by qld (M8's exit
/// criterion for `aarch64-apple-darwin`; `x86_64-apple-darwin` too when
/// its standard library is installed).
///
/// On macOS, rustc links through Apple clang with `-fuse-ld=ld64.qld`, and
/// the result runs. Elsewhere rustc runs `ld64.qld` directly against the
/// stub SDK, and the output is compared with `ld64.lld`'s. Both the default
/// deployment target (legacy dyld info) and 13.0 (chained fixups) are
/// linked.
#[test]
fn rust_binary() {
    if !cfg!(unix) {
        eprintln!("skipping rust_binary: needs a symlink to the qld binary");
        return;
    }
    let dir = scratch("rust");
    let linker = dir.join("ld64.qld");
    let _ = std::fs::remove_file(&linker);
    #[cfg(unix)]
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_qld"), &linker).unwrap();
    for (target, arch) in [
        ("aarch64-apple-darwin", "arm64"),
        ("x86_64-apple-darwin", "x86_64"),
    ] {
        if !rust_std_for(target) {
            // Only arm64 is required on the macOS runner.
            if arch == "arm64" {
                skip("rust_binary", &format!("rustc has no std for {target}"));
            } else {
                eprintln!("skipping rust_binary for {target}: rustc has no std for it");
            }
            continue;
        }
        for (deployment, chained) in [(None, false), (Some("13.0"), true)] {
            let variant = format!("{arch}-{}", deployment.unwrap_or("default"));
            let out_dir = dir.join(&variant);
            std::fs::create_dir_all(&out_dir).unwrap();
            let exe = out_dir.join("rust_std");
            let how = if cfg!(target_os = "macos") {
                RustLinker::Clang(&linker)
            } else {
                RustLinker::Direct(&linker)
            };
            rustc_link(target, how, deployment, &[], &exe)
                .unwrap_or_else(|e| panic!("{variant}: rustc with qld failed:\n{e}"));
            let bytes = std::fs::read(&exe).unwrap();

            let file = MachOFile::parse(&bytes, Source::new(&exe)).unwrap();
            assert_eq!(file.header().file_type, MH_EXECUTE, "{variant}");
            let commands: Vec<u32> = load_commands(&bytes).iter().map(|c| c.0).collect();
            assert!(commands.contains(&LC_MAIN), "{variant}");
            let dyld_info = if chained {
                LC_DYLD_CHAINED_FIXUPS
            } else {
                qld::macho::read::consts::LC_DYLD_INFO_ONLY
            };
            assert!(commands.contains(&dyld_info), "{variant}: {commands:x?}");
            if arch == "arm64" {
                check_signature(&bytes);
            }
            let names = section_names(&bytes);
            for wanted in [
                "__thread_vars",
                "__gcc_except_tab",
                "__eh_frame",
                "__unwind_info",
            ] {
                assert!(
                    names.iter().any(|n| n == wanted),
                    "{variant}: no {wanted} in {names:?}"
                );
            }
            // The personality is reached only from `__eh_frame` CIEs, and
            // must survive `-dead_strip`.
            assert!(
                symbol_address(&bytes, "_rust_eh_personality").is_some(),
                "{variant}: _rust_eh_personality was dead-stripped"
            );

            if let Some(lld) = ld64_lld() {
                let reference = out_dir.join("rust_std-lld");
                rustc_link(
                    target,
                    RustLinker::Direct(&lld),
                    deployment,
                    &[],
                    &reference,
                )
                .unwrap_or_else(|e| panic!("{variant}: rustc with ld64.lld failed:\n{e}"));
                if let (Some(ours), Some(theirs)) = (summarize(&exe), summarize(&reference)) {
                    assert_eq!(ours, theirs, "{variant}: qld vs ld64.lld");
                }
            }
            if host_can_run(arch) {
                assert_eq!(run(&exe).unwrap(), RUST_STD_OUTPUT, "{variant}");
            }
            if chained {
                rust_prelinked(target, arch, &linker, &out_dir);
            }
        }
    }
}

/// `-r` on a whole Rust program: rustc hands qld the program's objects
/// and the standard library's rlibs with `-r`, and the merged object then
/// links into the program (with qld, and with `ld64.lld` for comparison;
/// on macOS also with Apple's linker), which runs.
fn rust_prelinked(target: &str, arch: &str, linker: &Path, dir: &Path) {
    let merged = dir.join("rust_std.o");
    rustc_link(
        target,
        RustLinker::Direct(linker),
        Some("13.0"),
        &["-r"],
        &merged,
    )
    .unwrap_or_else(|e| panic!("{arch}: rustc with qld -r failed:\n{e}"));
    let bytes = std::fs::read(&merged).unwrap();
    check_object(&bytes, true);
    let mut args = base_args(arch);
    args.extend(strings(&[
        merged.to_str().unwrap(),
        "-lSystem",
        "-lc",
        "-lm",
        "-dead_strip",
    ]));
    let exe = dir.join("rust_std-prelinked");
    link_and_compare(&args, &exe);
    if host_can_run(arch) {
        assert_eq!(run(&exe).unwrap(), RUST_STD_OUTPUT, "{arch}: prelinked");
        let apple = dir.join("rust_std-prelinked-apple");
        let status = Command::new("clang")
            .args(["-arch", arch])
            .arg(&merged)
            .arg("-o")
            .arg(&apple)
            .status()
            .unwrap();
        assert!(
            status.success(),
            "{arch}: Apple's linker rejects the object"
        );
        assert_eq!(
            run(&apple).unwrap(),
            RUST_STD_OUTPUT,
            "{arch}: by Apple's ld"
        );
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
