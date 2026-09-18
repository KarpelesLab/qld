//! Integration tests for Mach-O LTO through libLTO (`qld::macho::lto`,
//! workstream W35).
//!
//! The fixtures in `tests/data/macho_lto/` are compiled to bitcode with
//! `clang --target=<arch>-apple-macos13 -flto` (or `-flto=thin`), mixed
//! with native objects and archives, and linked by qld against the stub SDK
//! of `tests/data/macho_link/sdk` (the real SDK on macOS). The outputs are
//! checked with qld's Mach-O reader: which symbols LTO kept, removed or
//! exported. When `ld64.lld` is available and its LLVM has the target, the
//! same inputs are linked with its built-in LTO and the global symbols
//! compared. On macOS the executables are run.
//!
//! Each architecture needs a clang and the libLTO of the same LLVM, so that
//! libLTO can read the bitcode:
//!
//! - on macOS, Apple's clang (`xcrun`), with qld finding Xcode's libLTO by
//!   itself;
//! - elsewhere, `QLD_LTO_CLANG` and `QLD_LTO_LIBRARY` when set, else the
//!   LLVM build in `~/.cache/qld-projects/build/llvm-23.1.1-static` (with
//!   `-lto_library`), else the `clang` in `PATH`, with qld finding the
//!   libLTO next to it; the first whose clang can generate code for the
//!   architecture.
//!
//! Missing tools make tests skip with a message. With
//! `QLD_REQUIRE_MACHO_TOOLS=1` (set on the macOS CI runner) they fail
//! instead.

#![cfg(all(feature = "plugin", unix))]

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use qld::args::{ParseOutcome, parse_darwin};
use qld::diag::Collect;
use qld::macho::read::consts::{LC_SYMTAB, MH_DYLIB, MH_EXECUTE, MH_OBJECT};
use qld::macho::read::{MachOFile, Source};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

const ARCHS: [&str; 2] = ["arm64", "x86_64"];

fn data_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/macho_lto")
}

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("macho_lto")
        .join(name);
    let _ = std::fs::remove_dir_all(&dir);
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

fn tool_works(tool: &Path, args: &[&str]) -> bool {
    Command::new(tool)
        .args(args)
        .output()
        .is_ok_and(|o| o.status.success())
}

fn llvm_build() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(|h| PathBuf::from(h).join(".cache/qld-projects/build/llvm-23.1.1-static"))
}

/// A compiler and the libLTO that reads its bitcode.
#[derive(Clone, Debug)]
struct Toolchain {
    clang: PathBuf,
    /// Passed as `-lto_library`; `None` lets qld find it.
    lto_library: Option<PathBuf>,
}

/// Whether `clang` generates code for `arch` (bitcode alone needs no
/// backend, but libLTO from the same build does).
fn clang_targets(clang: &Path, arch: &str) -> bool {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("macho_lto_probe");
    std::fs::create_dir_all(&dir).unwrap();
    let source = dir.join("probe.c");
    std::fs::write(&source, "int probe(void) { return 1; }\n").unwrap();
    let object = dir.join(format!(
        "probe-{arch}-{}.o",
        clang
            .to_string_lossy()
            .replace(|c: char| !c.is_ascii_alphanumeric(), "_")
    ));
    tool_works(
        clang,
        &[
            &format!("--target={arch}-apple-macos13"),
            "-c",
            source.to_str().unwrap(),
            "-o",
            object.to_str().unwrap(),
        ],
    )
}

fn toolchain(arch: &str) -> Option<Toolchain> {
    let mut candidates = Vec::new();
    if cfg!(target_os = "macos") {
        if let Ok(output) = Command::new("xcrun").args(["--find", "clang"]).output()
            && output.status.success()
        {
            candidates.push(Toolchain {
                clang: PathBuf::from(String::from_utf8_lossy(&output.stdout).trim()),
                lto_library: None,
            });
        }
    } else {
        if let (Some(clang), Some(library)) = (
            std::env::var_os("QLD_LTO_CLANG"),
            std::env::var_os("QLD_LTO_LIBRARY"),
        ) {
            candidates.push(Toolchain {
                clang: clang.into(),
                lto_library: Some(library.into()),
            });
        }
        if let Some(build) = llvm_build() {
            candidates.push(Toolchain {
                clang: build.join("bin/clang"),
                lto_library: Some(build.join("lib/libLTO.so")),
            });
        }
        candidates.push(Toolchain {
            clang: PathBuf::from("clang"),
            lto_library: None,
        });
    }
    candidates.into_iter().find(|t| {
        t.lto_library.as_ref().is_none_or(|l| l.is_file()) && clang_targets(&t.clang, arch)
    })
}

/// `ld64.lld`, from `QLD_LD64_LLD` or the usual build location.
fn ld64_lld() -> Option<PathBuf> {
    let candidates = [
        std::env::var_os("QLD_LD64_LLD").map(PathBuf::from),
        llvm_build().map(|b| b.join("bin/ld64.lld")),
        Some(PathBuf::from("ld64.lld")),
    ];
    candidates
        .into_iter()
        .flatten()
        .find(|path| tool_works(path, &["--version"]))
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
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/macho_link/sdk")
}

/// Compiles fixture `source` for `arch` into `dir`, with `flags` (for
/// example `-flto`).
fn compile(tools: &Toolchain, dir: &Path, source: &str, arch: &str, flags: &[&str]) -> PathBuf {
    let stem = Path::new(source).file_stem().unwrap().to_str().unwrap();
    let suffix = flags
        .iter()
        .map(|f| f.trim_start_matches('-').replace('=', "_"))
        .collect::<Vec<_>>()
        .join("-");
    let output = dir.join(format!("{stem}-{arch}-{suffix}.o"));
    let status = Command::new(&tools.clang)
        .arg(format!("--target={arch}-apple-macos13"))
        .args(["-O2", "-c"])
        .args(flags)
        .arg(data_dir().join(source))
        .arg("-o")
        .arg(&output)
        .status()
        .unwrap();
    assert!(status.success(), "compiling {source} for {arch}");
    output
}

/// Writes a BSD `ar` archive of `members` (no tool needed).
fn archive(path: &Path, members: &[&Path]) {
    let mut out = b"!<arch>\n".to_vec();
    for member in members {
        let data = std::fs::read(member).unwrap();
        let mut name = member.file_name().unwrap().as_encoded_bytes().to_vec();
        name.push(0);
        while !(out.len() + 60 + name.len()).is_multiple_of(8) {
            name.push(0);
        }
        let header = format!(
            "{:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n",
            format!("#1/{}", name.len()),
            0,
            0,
            0,
            "100644",
            name.len() + data.len()
        );
        out.extend_from_slice(header.as_bytes());
        out.extend_from_slice(&name);
        out.extend_from_slice(&data);
        if out.len() % 2 == 1 {
            out.push(b'\n');
        }
    }
    std::fs::write(path, out).unwrap();
}

/// Wraps `object` in a universal file with one slice.
fn universal(path: &Path, object: &Path, arch: &str) {
    let data = std::fs::read(object).unwrap();
    let (cpu_type, cpu_subtype) = match arch {
        "arm64" => (0x0100_000c_u32, 0u32),
        _ => (0x0100_0007, 3),
    };
    let mut out = Vec::new();
    for word in [0xcafe_babe_u32, 1, cpu_type, cpu_subtype, 0x1000, 0, 12] {
        out.extend_from_slice(&word.to_be_bytes());
    }
    let size = u32::try_from(data.len()).unwrap();
    out[20..24].copy_from_slice(&size.to_be_bytes());
    out.resize(0x1000, 0);
    out.extend_from_slice(&data);
    std::fs::write(path, out).unwrap();
}

fn os(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

/// The arguments every link of `arch` starts with.
fn base_args(arch: &str) -> Vec<OsString> {
    let root = syslibroot();
    let mut args = os(&[
        "-arch",
        arch,
        "-platform_version",
        "macos",
        "13.0",
        "13.0",
        "-syslibroot",
    ]);
    args.push(root.into());
    args
}

/// Links with an ld64 command line (without `argv[0]`), adding
/// `-lto_library` and the `-mllvm` option clang passes on Apple platforms
/// (the same in every link: LLVM parses its options once per process);
/// returns the output bytes and the diagnostics.
fn qld_link(tools: &Toolchain, args: &[OsString]) -> Result<(Vec<u8>, Vec<String>), String> {
    let mut argv = vec![OsString::from("ld64.qld")];
    if let Some(library) = &tools.lto_library {
        argv.push("-lto_library".into());
        argv.push(library.into());
    }
    argv.extend(os(&["-mllvm", "-enable-linkonceodr-outlining"]));
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

/// Links, writes the output to `output` and returns it.
fn link(tools: &Toolchain, args: &[OsString], output: &Path) -> Vec<u8> {
    let (bytes, messages) = qld_link(tools, args).unwrap_or_else(|e| panic!("link failed: {e}"));
    assert!(
        messages.iter().all(|m| !m.contains("-mllvm")),
        "{messages:?}"
    );
    std::fs::write(output, &bytes).unwrap();
    make_executable(output);
    bytes
}

#[cfg(unix)]
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// The global symbols of an image: `(defined, undefined)` names.
fn globals(data: &[u8]) -> (BTreeSet<String>, BTreeSet<String>) {
    let file = MachOFile::parse(data, Source::new(Path::new("out"))).unwrap();
    let mut defined = BTreeSet::new();
    let mut undefined = BTreeSet::new();
    let Some(command) = file.find_command(LC_SYMTAB).unwrap() else {
        return (defined, undefined);
    };
    let symtab = command.symtab().unwrap();
    let strtab = &data[symtab.stroff as usize..][..symtab.strsize as usize];
    for index in 0..symtab.nsyms as usize {
        let at = symtab.symoff as usize + index * 16;
        let strx = u32::from_le_bytes(data[at..at + 4].try_into().unwrap()) as usize;
        let name = strtab[strx..].split(|&b| b == 0).next().unwrap();
        let name = String::from_utf8_lossy(name).into_owned();
        let n_type = data[at + 4];
        // Stabs, and symbols that are not external.
        if n_type & 0xe0 != 0 || n_type & 0x01 == 0 {
            continue;
        }
        if n_type & 0x0e == 0 {
            undefined.insert(name);
        } else {
            defined.insert(name);
        }
    }
    (defined, undefined)
}

fn file_type(data: &[u8]) -> u32 {
    MachOFile::parse(data, Source::new(Path::new("out")))
        .unwrap()
        .header()
        .file_type
}

/// Links `args` with `ld64.lld` into `output` and compares the global
/// symbols with qld's `bytes`. Skips when there is no `ld64.lld` or its
/// LLVM lacks the target.
fn compare_with_lld(test: &str, args: &[OsString], output: &Path, bytes: &[u8]) {
    let Some(lld) = ld64_lld() else {
        eprintln!("{test}: no ld64.lld, not compared");
        return;
    };
    let result = Command::new(&lld)
        .args(args)
        .arg("-o")
        .arg(output)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&result.stderr);
    if stderr.contains("No available targets") {
        eprintln!("{test}: {} lacks the target, not compared", lld.display());
        return;
    }
    assert!(result.status.success(), "{test}: ld64.lld failed: {stderr}");
    let reference = std::fs::read(output).unwrap();
    let (mut ours, ours_undefined) = globals(bytes);
    let (mut theirs, mut theirs_undefined) = globals(&reference);
    // Linker-defined and lazy-binding symbols differ between linkers.
    for set in [&mut ours, &mut theirs] {
        set.remove("___dso_handle");
    }
    theirs_undefined.remove("dyld_stub_binder");
    assert_eq!(ours, theirs, "{test}: defined globals, qld vs ld64.lld");
    assert_eq!(
        ours_undefined, theirs_undefined,
        "{test}: undefined globals, qld vs ld64.lld"
    );
}

fn host_can_run(arch: &str) -> bool {
    cfg!(target_os = "macos")
        && (arch == std::env::consts::ARCH
            || (arch == "arm64" && std::env::consts::ARCH == "aarch64")
            || (arch == "x86_64" && tool_works(Path::new("arch"), &["-x86_64", "/usr/bin/true"])))
}

/// Runs `binary` on macOS and checks its output.
fn run_and_check(binary: &Path, arch: &str, expected: &str) {
    if !host_can_run(arch) {
        return;
    }
    let output = Command::new(binary).output().unwrap();
    assert!(
        output.status.success(),
        "{} failed: {:?}\n{}",
        binary.display(),
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), expected);
}

/// The objects and archive of the program: `main.c` and `helper.c` as
/// bitcode, `native.c` native, and `libmixed.a` with bitcode and native
/// members.
struct Program {
    objects: Vec<PathBuf>,
    library_dir: PathBuf,
}

fn program(tools: &Toolchain, dir: &Path, arch: &str, lto: &str) -> Program {
    let main = compile(tools, dir, "main.c", arch, &[lto]);
    let helper = compile(tools, dir, "helper.c", arch, &[lto]);
    let native = compile(tools, dir, "native.c", arch, &[]);
    let lib_bc = compile(tools, dir, "lib_bc.c", arch, &[lto]);
    let lib_native = compile(tools, dir, "lib_native.c", arch, &[]);
    let lib_never = compile(tools, dir, "lib_never.c", arch, &[lto]);
    let library_dir = dir.join("lib");
    std::fs::create_dir_all(&library_dir).unwrap();
    archive(
        &library_dir.join("libmixed.a"),
        &[&lib_bc, &lib_native, &lib_never],
    );
    Program {
        objects: vec![main, helper, native],
        library_dir,
    }
}

/// Checks what LTO kept in the program.
fn check_program(test: &str, bytes: &[u8]) {
    assert_eq!(file_type(bytes), MH_EXECUTE);
    let (defined, undefined) = globals(bytes);
    for name in [
        "_main",
        "_bc_called_from_native",
        "_lib_native",
        "_shared_weak",
    ] {
        assert!(defined.contains(name), "{test}: {name} kept: {defined:?}");
    }
    // Unreferenced bitcode globals of an executable are internalized and
    // removed; the unreferenced archive member is never loaded.
    for name in ["_helper_unused", "_lib_bc_unused", "_lib_never"] {
        assert!(
            !defined.contains(name),
            "{test}: {name} removed: {defined:?}"
        );
    }
    assert!(undefined.contains("_printf"), "{test}: {undefined:?}");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn full_lto_executables() {
    for arch in ARCHS {
        let Some(tools) = toolchain(arch) else {
            skip(
                "full_lto_executables",
                &format!("no clang and libLTO for {arch}-apple-macos"),
            );
            continue;
        };
        let dir = scratch(&format!("full-{arch}"));
        let program = program(&tools, &dir, arch, "-flto");
        let mut args = base_args(arch);
        args.extend(program.objects.iter().map(OsString::from));
        args.push("-L".into());
        args.push(program.library_dir.clone().into());
        args.extend(os(&["-lmixed", "-lSystem"]));
        let object_path = dir.join("lto.o");
        let mut qld_args = args.clone();
        qld_args.push("-object_path_lto".into());
        qld_args.push(object_path.clone().into());
        let output = dir.join("program");
        let bytes = link(&tools, &qld_args, &output);
        check_program("full_lto_executables", &bytes);
        // The LTO object is kept for the debug map.
        let lto_object = std::fs::read(&object_path).expect("-object_path_lto object");
        assert_eq!(file_type(&lto_object), MH_OBJECT);
        run_and_check(&output, arch, "lto 34\n");
        compare_with_lld(
            "full_lto_executables",
            &args,
            &dir.join("program-lld"),
            &bytes,
        );
    }
}

#[test]
fn thin_lto_executables() {
    for arch in ARCHS {
        let Some(tools) = toolchain(arch) else {
            skip(
                "thin_lto_executables",
                &format!("no clang and libLTO for {arch}-apple-macos"),
            );
            continue;
        };
        let dir = scratch(&format!("thin-{arch}"));
        let program = program(&tools, &dir, arch, "-flto=thin");
        let mut args = base_args(arch);
        args.extend(program.objects.iter().map(OsString::from));
        args.push("-L".into());
        args.push(program.library_dir.clone().into());
        args.extend(os(&["-lmixed", "-lSystem"]));
        let objects = dir.join("objects");
        let cache = dir.join("cache");
        let mut qld_args = args.clone();
        qld_args.push("-object_path_lto".into());
        qld_args.push(objects.clone().into());
        qld_args.push("-cache_path_lto".into());
        qld_args.push(cache.clone().into());
        let output = dir.join("program");
        let bytes = link(&tools, &qld_args, &output);
        check_program("thin_lto_executables", &bytes);
        // One object per module in the objects directory, and a cache.
        let written = std::fs::read_dir(&objects).unwrap().count();
        assert!(written >= 2, "{written} ThinLTO objects");
        assert!(std::fs::read_dir(&cache).unwrap().count() > 0, "cache");
        run_and_check(&output, arch, "lto 34\n");

        // Again, from the cache: the same output.
        let again = link(&tools, &qld_args, &dir.join("program-again"));
        assert!(again == bytes, "ThinLTO links from the cache differ");

        compare_with_lld(
            "thin_lto_executables",
            &args,
            &dir.join("program-lld"),
            &bytes,
        );
    }
}

#[test]
fn dylib_exports() {
    for arch in ARCHS {
        let Some(tools) = toolchain(arch) else {
            skip(
                "dylib_exports",
                &format!("no clang and libLTO for {arch}-apple-macos"),
            );
            continue;
        };
        let dir = scratch(&format!("dylib-{arch}"));
        let api = compile(&tools, &dir, "api.c", arch, &["-flto"]);
        let mut args = base_args(arch);
        args.extend(os(&["-dylib", "-install_name", "@rpath/libapi.dylib"]));
        args.push(api.clone().into());
        args.push("-lSystem".into());

        // A dylib exports every default-visibility global: LTO keeps them.
        let output = dir.join("libapi.dylib");
        let bytes = link(&tools, &args, &output);
        assert_eq!(file_type(&bytes), MH_DYLIB);
        let (defined, _) = globals(&bytes);
        for name in ["_api_exported", "_api_other", "_api_uses_hidden"] {
            assert!(defined.contains(name), "{name} exported: {defined:?}");
        }
        assert!(!defined.contains("_api_hidden"), "{defined:?}");
        compare_with_lld(
            "dylib_exports",
            &args,
            &dir.join("libapi-lld.dylib"),
            &bytes,
        );

        // With an export list, the others are internalized.
        let list = dir.join("exports.txt");
        std::fs::write(&list, "_api_exported\n").unwrap();
        let mut listed = args.clone();
        listed.push("-exported_symbols_list".into());
        listed.push(list.into());
        let output = dir.join("libapi-listed.dylib");
        let bytes = link(&tools, &listed, &output);
        let (defined, _) = globals(&bytes);
        assert_eq!(
            defined,
            BTreeSet::from(["_api_exported".to_owned()]),
            "exported with a list"
        );
        compare_with_lld(
            "dylib_exports (list)",
            &listed,
            &dir.join("libapi-listed-lld.dylib"),
            &bytes,
        );

        // An executable keeps its globals with -export_dynamic.
        let main = compile(&tools, &dir, "main.c", arch, &["-flto"]);
        let helper = compile(&tools, &dir, "helper.c", arch, &["-flto"]);
        let native = compile(&tools, &dir, "native.c", arch, &[]);
        let lib_bc = compile(&tools, &dir, "lib_bc.c", arch, &["-flto"]);
        let lib_native = compile(&tools, &dir, "lib_native.c", arch, &[]);
        let mut exec = base_args(arch);
        for object in [&main, &helper, &native, &lib_bc, &lib_native] {
            exec.push(object.into());
        }
        exec.extend(os(&["-lSystem", "-export_dynamic"]));
        let bytes = link(&tools, &exec, &dir.join("program"));
        let (defined, _) = globals(&bytes);
        for name in ["_helper", "_helper_unused", "_lib_bc_unused"] {
            assert!(defined.contains(name), "-export_dynamic keeps {name}");
        }
    }
}

#[test]
fn relocatable_output_and_universal_input() {
    for arch in ARCHS {
        let Some(tools) = toolchain(arch) else {
            skip(
                "relocatable_output_and_universal_input",
                &format!("no clang and libLTO for {arch}-apple-macos"),
            );
            continue;
        };
        let dir = scratch(&format!("relocatable-{arch}"));
        let helper = compile(&tools, &dir, "helper.c", arch, &["-flto"]);
        let native = compile(&tools, &dir, "native.c", arch, &[]);

        // `-r` keeps every global of the bitcode for the final link.
        let mut args = os(&["-arch", arch, "-r"]);
        args.push(helper.clone().into());
        args.push(native.clone().into());
        let bytes = link(&tools, &args, &dir.join("combined.o"));
        assert_eq!(file_type(&bytes), MH_OBJECT);
        let (defined, _) = globals(&bytes);
        for name in [
            "_helper",
            "_helper_unused",
            "_bc_called_from_native",
            "_from_native_object",
        ] {
            assert!(defined.contains(name), "-r keeps {name}: {defined:?}");
        }

        // Bitcode inside a universal file.
        let fat = dir.join("helper-universal.o");
        universal(&fat, &helper, arch);
        let main = compile(&tools, &dir, "main.c", arch, &["-flto"]);
        let lib_bc = compile(&tools, &dir, "lib_bc.c", arch, &["-flto"]);
        let lib_native = compile(&tools, &dir, "lib_native.c", arch, &[]);
        let mut exec = base_args(arch);
        for object in [&main, &fat, &native, &lib_bc, &lib_native] {
            exec.push(object.into());
        }
        exec.push("-lSystem".into());
        let output = dir.join("program");
        let bytes = link(&tools, &exec, &output);
        let (defined, _) = globals(&bytes);
        assert!(defined.contains("_bc_called_from_native"), "{defined:?}");
        assert!(!defined.contains("_helper_unused"), "{defined:?}");
        run_and_check(&output, arch, "lto 34\n");
    }
}

#[test]
fn missing_libraries_are_reported() {
    let dir = scratch("missing");
    // Only the magic matters: libLTO is looked for before the bitcode is
    // read.
    let bitcode = dir.join("fake.o");
    std::fs::write(&bitcode, b"BC\xc0\xde\x35\x14\x00\x00\x05\x00\x00\x00").unwrap();
    let tools = Toolchain {
        clang: PathBuf::from("clang"),
        lto_library: Some(dir.join("no-such-libLTO.dylib")),
    };
    let mut args = base_args("arm64");
    args.push(bitcode.into());
    let error = qld_link(&tools, &args).unwrap_err();
    assert!(error.contains("-lto_library"), "{error}");
}
