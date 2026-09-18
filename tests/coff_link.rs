//! Integration tests for the PE/COFF link driver (workstreams W21 and W33).
//!
//! Inputs are compiled with the MinGW-w64 cross toolchains (x86-64, and
//! i686 for the `i386_*` tests), linked with qld's
//! library API (`qld::coff::link_with`), and inspected with `llvm-readobj`,
//! `llvm-objdump` or the MinGW `objdump`. Where GNU `ld` can link the same
//! inputs, the two images are compared.
//!
//! Windows binaries cannot run on the test host, so the checks are on the
//! image structure. The fixtures marked `RUN ON WINDOWS` in their doc comment
//! are the ones a `windows-latest` CI job should also execute.
//!
//! A test prints `SKIPPED:` and passes when a tool it needs is missing,
//! unless `QLD_REQUIRE_COFF_TOOLS` is set.

use std::path::{Path, PathBuf};
use std::process::Command;

use qld::args::{InputAttrs, InputKind, InputSpec, LinkOptions};
use qld::coff::PeOptions;
use qld::diag::Collect;
use qld::target::{Architecture, BinaryFormat, Endianness, OperatingSystem, PointerWidth, Target};

/// Reports a skipped check, or fails when `QLD_REQUIRE_COFF_TOOLS` is set.
fn skip(reason: &str) {
    if std::env::var_os("QLD_REQUIRE_COFF_TOOLS").is_some_and(|v| !v.is_empty() && v != "0") {
        panic!("required tool unavailable: {reason}");
    }
    println!("SKIPPED: {reason}");
}

/// Finds `name` in `PATH`, trying the `.exe` spelling on Windows.
fn tool(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let names = [name.to_owned(), format!("{name}.exe")];
    std::env::split_paths(&path)
        .flat_map(|dir| names.clone().map(|name| dir.join(name)))
        .find(|candidate| candidate.is_file())
}

/// The name to invoke a MinGW tool by. Cross toolchains prefix every tool
/// with the target triple; MSYS2's MINGW64 (the Windows CI job) prefixes
/// only the compilers, and has `nm`, `windres`, `ar` and `objdump`
/// unprefixed, so each tool is resolved on its own. `QLD_MINGW_PREFIX`
/// forces a prefix.
fn mingw(base: &str) -> String {
    let forced = std::env::var_os("QLD_MINGW_PREFIX").map(|p| p.to_string_lossy().into_owned());
    if let Some(prefix) = forced {
        return format!("{prefix}{base}");
    }
    let prefixed = format!("x86_64-w64-mingw32-{base}");
    let is_compiler = base == "gcc" || base == "g++";
    if tool(&prefixed).is_some() && (!is_compiler || targets_mingw(&prefixed)) {
        return prefixed;
    }
    // The unprefixed compiler must target Windows: on Linux and macOS `gcc`
    // builds ELF and Mach-O, and these tests would link the wrong format
    // instead of skipping.
    if tool(base).is_some() && (!is_compiler || targets_mingw(base)) {
        return base.to_owned();
    }
    prefixed
}

/// Whether `compiler` exists and builds Windows objects (`-dumpmachine`
/// reports a `mingw` or `windows-gnu` target).
fn targets_mingw(compiler: &str) -> bool {
    let Ok(output) = Command::new(compiler).arg("-dumpmachine").output() else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let target = String::from_utf8_lossy(&output.stdout);
    target.contains("mingw") || target.contains("windows-gnu")
}

/// A fresh, empty directory for one test.
fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("coff-link")
        .join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Runs a command, returning its output on success.
fn run(program: &str, args: &[&str], dir: &Path) -> Option<std::process::Output> {
    let output = match Command::new(program)
        .args(args)
        .current_dir(dir)
        .env("LC_ALL", "C")
        .output()
    {
        Ok(output) => output,
        Err(error) => {
            skip(&format!("{program} unavailable: {error}"));
            return None;
        }
    };
    if !output.status.success() {
        skip(&format!(
            "`{program} {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
        return None;
    }
    Some(output)
}

/// The linker command line `gcc` would use for `args`, with the LTO plugin
/// options and the `collect2` program name removed.
fn link_argv(dir: &Path, args: &[&str]) -> Option<Vec<String>> {
    link_argv_via(&mingw("gcc"), dir, args)
}

/// The linker command line `driver` (a MinGW `gcc` or `g++`) would use.
fn link_argv_via(driver: &str, dir: &Path, args: &[&str]) -> Option<Vec<String>> {
    let mut full = vec!["-###"];
    full.extend_from_slice(args);
    let output = run(driver, &full, dir)?;
    let text = String::from_utf8_lossy(&output.stderr).into_owned();
    let line = text
        .lines()
        .rfind(|line| line.contains("collect2") || line.contains("/ld"))?;
    let mut argv = Vec::new();
    for word in split_words(line) {
        if word.starts_with("-plugin") || word.contains("collect2") || word.contains("liblto") {
            continue;
        }
        argv.push(word);
    }
    // The first word is the program name.
    if !argv.is_empty() {
        argv.remove(0);
    }
    Some(argv)
}

/// Splits a `gcc -###` line, which quotes words with `"`.
fn split_words(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut started = false;
    for c in line.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                started = true;
            }
            c if c.is_whitespace() && !quoted => {
                if started {
                    words.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            c => {
                current.push(c);
                started = true;
            }
        }
    }
    if started {
        words.push(current);
    }
    words
}

/// The PE target qld links for.
fn pe_target() -> Target {
    Target {
        arch: Architecture::X86_64,
        os: OperatingSystem::Windows,
        format: BinaryFormat::Pe,
        endian: Endianness::Little,
        pointer_width: PointerWidth::Bits64,
    }
}

/// Parses `argv` — the whole MinGW link line, PE options included — into
/// [`LinkOptions`], with the output redirected to `output` because the test
/// process does not run in the scratch directory.
fn options_from(argv: &[String], output: &Path) -> LinkOptions {
    options_for(argv, output, pe_target())
}

/// [`options_from`] for another PE target.
fn options_for(argv: &[String], output: &Path, target: Target) -> LinkOptions {
    let words: Vec<std::ffi::OsString> = std::iter::once("qld".into())
        .chain(argv.iter().map(std::ffi::OsString::from))
        .collect();
    let mut options = match qld::parse_gnu(&words) {
        Ok(qld::ParseOutcome::Link(options)) => *options,
        other => panic!("cannot parse the MinGW link line: {other:?}"),
    };
    options.target = Some(target);
    options.output = Some(output.to_path_buf());
    options
}

/// Links with qld and returns the diagnostics it emitted.
fn qld_link(options: &LinkOptions, pe: &PeOptions) -> Result<String, String> {
    let sink = Collect::new();
    let result = qld::coff::link_with(options, pe, &sink);
    let messages: Vec<String> = sink
        .take_sorted()
        .iter()
        .map(|diagnostic| format!("{}: {}", diagnostic.severity, diagnostic.message))
        .collect();
    match result {
        Ok(()) => Ok(messages.join("\n")),
        Err(error) => Err(format!("{error}\n{}", messages.join("\n"))),
    }
}

/// Runs `llvm-readobj` (or the MinGW `objdump`) on a file.
fn readobj(dir: &Path, file: &str, args: &[&str]) -> Option<String> {
    let mut full: Vec<&str> = args.to_vec();
    full.push(file);
    let output = run("llvm-readobj", &full, dir)?;
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Compiles `source` to an object in `dir`.
fn compile(dir: &Path, name: &str, source: &str, flags: &[&str]) -> Option<String> {
    compile_via(&mingw("gcc"), dir, name, source, flags)
}

/// Compiles `source` to an object in `dir` with `compiler`.
fn compile_via(
    compiler: &str,
    dir: &Path,
    name: &str,
    source: &str,
    flags: &[&str],
) -> Option<String> {
    let file = format!("{name}.c");
    std::fs::write(dir.join(&file), source).unwrap();
    let object = format!("{name}.o");
    let mut args = vec!["-c", "-fno-lto", file.as_str(), "-o", object.as_str()];
    args.extend_from_slice(flags);
    run(compiler, &args, dir)?;
    // An absolute path, so the linker can be run from any directory.
    Some(dir.join(&object).to_str()?.to_string())
}

const HELLO: &str = r#"
#include <stdio.h>
int global_counter;
const char message[] = "hello from qld\n";
int main(void) {
    global_counter += 1;
    fputs(message, stdout);
    return global_counter - 1;
}
"#;

/// A console hello world, linked through the full MinGW runtime.
///
/// RUN ON WINDOWS: the image should print `hello from qld` and exit 0.
#[test]
fn console_hello_world() {
    if tool(&mingw("gcc")).is_none() {
        skip(&format!("{} not found", mingw("gcc")));
        return;
    }
    let dir = scratch("console-hello-world");
    let Some(object) = compile(&dir, "hello", HELLO, &[]) else {
        return;
    };
    let Some(argv) = link_argv(&dir, &[object.as_str(), "-o", "gnu.exe", "-fno-lto"]) else {
        return;
    };
    // GNU ld's own image, as the reference.
    let gnu = run(
        &mingw("gcc"),
        &[object.as_str(), "-o", "gnu.exe", "-fno-lto"],
        &dir,
    );
    if gnu.is_none() {
        return;
    }

    let options = options_from(&argv, &dir.join("qld.exe"));
    let pe = PeOptions::from_link_options(&options);
    if let Err(error) = qld_link(&options, &pe) {
        panic!("qld failed to link the MinGW hello world:\n{error}");
    }

    let Some(headers) = readobj(&dir, "qld.exe", &["--file-headers"]) else {
        return;
    };
    assert!(headers.contains("IMAGE_FILE_MACHINE_AMD64"), "{headers}");
    assert!(headers.contains("IMAGE_SUBSYSTEM_WINDOWS_CUI"), "{headers}");
    assert!(headers.contains("ImageBase: 0x140000000"), "{headers}");
    assert!(
        headers.contains("IMAGE_DLL_CHARACTERISTICS_DYNAMIC_BASE"),
        "{headers}"
    );
    assert!(
        headers.contains("IMAGE_DLL_CHARACTERISTICS_NX_COMPAT"),
        "{headers}"
    );
    // The entry point, the imports and the base relocations must all exist.
    assert!(!headers.contains("AddressOfEntryPoint: 0x0"), "{headers}");
    assert!(!headers.contains("ImportTableRVA: 0x0"), "{headers}");
    assert!(
        !headers.contains("BaseRelocationTableRVA: 0x0"),
        "{headers}"
    );

    let Some(imports) = readobj(&dir, "qld.exe", &["--coff-imports"]) else {
        return;
    };
    assert!(
        imports.to_ascii_lowercase().contains("msvcrt.dll")
            || imports.to_ascii_lowercase().contains("kernel32.dll"),
        "{imports}"
    );

    // `coff::link` is what `crate::link` will call once the PE target is
    // wired into it: it must produce the same image as `link_with` does
    // with the derived options.
    let mut plain = options.clone();
    plain.output = Some(dir.join("plain.exe"));
    let sink = Collect::new();
    qld::coff::link(&plain, &sink).expect("coff::link");
    assert_eq!(
        std::fs::read(dir.join("plain.exe")).unwrap().len(),
        std::fs::read(dir.join("qld.exe")).unwrap().len(),
        "coff::link and link_with disagree"
    );
}

/// The output section set and their characteristics match GNU `ld`'s.
#[test]
fn sections_match_gnu_ld() {
    if tool(&mingw("gcc")).is_none() {
        skip(&format!("{} not found", mingw("gcc")));
        return;
    }
    let dir = scratch("sections-match-gnu-ld");
    let Some(object) = compile(&dir, "hello", HELLO, &[]) else {
        return;
    };
    let Some(argv) = link_argv(&dir, &[object.as_str(), "-o", "gnu.exe", "-fno-lto"]) else {
        return;
    };
    if run(
        &mingw("gcc"),
        &[object.as_str(), "-o", "gnu.exe", "-fno-lto"],
        &dir,
    )
    .is_none()
    {
        return;
    }
    let options = options_from(&argv, &dir.join("qld.exe"));
    let pe = PeOptions::from_link_options(&options);
    if let Err(error) = qld_link(&options, &pe) {
        panic!("qld failed to link:\n{error}");
    }
    let names = |file: &str| -> Vec<String> {
        readobj(&dir, file, &["--sections"])
            .unwrap_or_default()
            .lines()
            .filter_map(|line| line.trim().strip_prefix("Name: "))
            .map(|name| name.split_whitespace().next().unwrap_or("").to_string())
            .collect()
    };
    let mut gnu = names("gnu.exe");
    let mut ours = names("qld.exe");
    if gnu.is_empty() || ours.is_empty() {
        return;
    }
    // Allocated sections must match in order; the order of the non-allocated
    // debug sections relative to `.reloc` differs between binutils versions,
    // so compare those as sets.
    let split = |list: &mut Vec<String>| {
        let at = list
            .iter()
            .position(|name| name.starts_with(".debug") || name == ".reloc")
            .unwrap_or(list.len());
        let mut rest: Vec<String> = list.split_off(at);
        rest.sort();
        rest
    };
    let gnu_rest = split(&mut gnu);
    let ours_rest = split(&mut ours);
    assert_eq!(ours, gnu, "allocated section names differ from GNU ld");
    assert_eq!(
        ours_rest, gnu_rest,
        "trailing section names differ from GNU ld"
    );
}

#[test]
fn image_base_and_alignment_options_are_validated() {
    let mut pe = PeOptions::default();
    pe.file_alignment = 511;
    assert!(pe.validate().is_err());
    pe = PeOptions::default();
    pe.image_base = Some(0x1234);
    assert!(pe.validate().is_err());
    pe = PeOptions::default();
    pe.image_base = Some(0x1_0000_0000);
    pe.validate().unwrap();
}

#[test]
fn missing_symbols_are_reported() {
    if tool(&mingw("gcc")).is_none() {
        skip(&format!("{} not found", mingw("gcc")));
        return;
    }
    let dir = scratch("missing-symbols");
    let Some(object) = compile(
        &dir,
        "orphan",
        "extern int nowhere(void);\nint mainCRTStartup(void) { return nowhere(); }\n",
        &[],
    ) else {
        return;
    };
    let mut options = LinkOptions {
        target: Some(pe_target()),
        output: Some(dir.join("out.exe")),
        ..LinkOptions::default()
    };
    options.inputs.push(InputSpec {
        kind: InputKind::File(dir.join(&object)),
        attrs: InputAttrs::default(),
        position: 0,
    });
    let pe = PeOptions::from_link_options(&options);
    let error = qld_link(&options, &pe).unwrap_err();
    assert!(error.contains("nowhere"), "{error}");
}

const LIBRARY: &str = r#"
#include <stdio.h>
__declspec(dllexport) int exported_data = 41;
__declspec(dllexport) int add_one(int value) { return value + 1; }
__declspec(dllexport) void greet(void) { fputs("hello from the dll\n", stdout); }
int not_exported(void) { return 7; }
"#;

const CLIENT: &str = r#"
#include <stdio.h>
__declspec(dllimport) extern int exported_data;
__declspec(dllimport) int add_one(int value);
__declspec(dllimport) void greet(void);
int main(void) {
    greet();
    printf("%d\n", add_one(exported_data));
    return 0;
}
"#;

/// A DLL with `__declspec(dllexport)` exports and an import library, and an
/// executable linked against it.
///
/// RUN ON WINDOWS: the executable should print `hello from the dll` then
/// `42`, and exit 0.
#[test]
fn dll_with_import_library() {
    if tool(&mingw("gcc")).is_none() {
        skip(&format!("{} not found", mingw("gcc")));
        return;
    }
    let dir = scratch("dll-with-import-library");
    let Some(library_object) = compile(&dir, "library", LIBRARY, &[]) else {
        return;
    };
    // The DLL: gcc's `-shared` line, with qld producing the import library.
    let Some(argv) = link_argv(
        &dir,
        &[
            "-shared",
            library_object.as_str(),
            "-o",
            "sample.dll",
            "-fno-lto",
        ],
    ) else {
        return;
    };
    let mut options = options_from(&argv, &dir.join("sample.dll"));
    options.kind = qld::args::OutputKind::Shared;
    let mut pe = PeOptions::from_link_options(&options);
    pe.out_implib = Some(dir.join("libsample.dll.a"));
    pe.output_def = Some(dir.join("sample.def"));
    if let Err(error) = qld_link(&options, &pe) {
        panic!("qld failed to link the DLL:\n{error}");
    }

    let Some(exports) = readobj(&dir, "sample.dll", &["--coff-exports"]) else {
        return;
    };
    for name in ["add_one", "greet", "exported_data"] {
        assert!(exports.contains(name), "{name} missing from:\n{exports}");
    }
    assert!(
        !exports.contains("not_exported"),
        "a non-dllexport symbol leaked:\n{exports}"
    );
    let Some(headers) = readobj(&dir, "sample.dll", &["--file-headers"]) else {
        return;
    };
    assert!(headers.contains("IMAGE_FILE_DLL"), "{headers}");
    assert!(!headers.contains("ExportTableRVA: 0x0"), "{headers}");
    assert!(headers.contains("ImageBase: 0x180000000"), "{headers}");

    let def = std::fs::read_to_string(dir.join("sample.def")).unwrap();
    assert!(def.starts_with("EXPORTS\n"), "{def}");
    assert!(def.contains("add_one @"), "{def}");

    // The executable, linked against the import library qld just wrote.
    let Some(client_object) = compile(&dir, "client", CLIENT, &[]) else {
        return;
    };
    let Some(argv) = link_argv(
        &dir,
        &[client_object.as_str(), "-o", "client.exe", "-fno-lto"],
    ) else {
        return;
    };
    let mut options = options_from(&argv, &dir.join("client.exe"));
    let implib = dir.join("libsample.dll.a");
    options.inputs.push(InputSpec {
        kind: InputKind::File(implib.clone()),
        attrs: InputAttrs::default(),
        position: options.inputs.len(),
    });
    let pe = PeOptions::from_link_options(&options);
    if let Err(error) = qld_link(&options, &pe) {
        panic!("qld failed to link against its own import library:\n{error}");
    }
    let Some(imports) = readobj(&dir, "client.exe", &["--coff-imports"]) else {
        return;
    };
    assert!(imports.contains("sample.dll"), "{imports}");
    assert!(imports.contains("add_one"), "{imports}");
    assert!(imports.contains("exported_data"), "{imports}");

    // GNU ld must accept the same import library, and bind the same
    // symbols to the same DLL.
    let gnu_args = [
        client_object.as_str(),
        implib.to_str().unwrap(),
        "-o",
        "gnu-client.exe",
        "-fno-lto",
    ];
    if run(&mingw("gcc"), &gnu_args, &dir).is_some() {
        let Some(theirs) = readobj(&dir, "gnu-client.exe", &["--coff-imports"]) else {
            return;
        };
        let ours = imports;
        // The DLL's place on the command line decides its table addresses,
        // so compare the symbols it binds rather than the RVAs.
        let entry = |text: &str| -> Vec<String> {
            text.lines()
                .skip_while(|line| !line.contains("sample.dll"))
                .skip(1)
                .skip_while(|line| !line.trim().starts_with("Symbol:"))
                .take_while(|line| line.trim().starts_with("Symbol:"))
                .map(|line| line.trim().to_string())
                .collect()
        };
        assert_eq!(
            entry(&ours),
            entry(&theirs),
            "GNU ld read qld's import library differently"
        );
    }
}

/// The same DLL and client, driven entirely by qld's command line: the
/// options come from `gcc -shared`'s own link line plus `--out-implib` and
/// `--output-def`, and no [`PeOptions`] is built by hand.
///
/// RUN ON WINDOWS: the executable should print `hello from the dll` then
/// `42`, and exit 0.
#[test]
fn command_line_builds_a_dll_and_an_executable() {
    if tool(&mingw("gcc")).is_none() {
        skip(&format!("{} not found", mingw("gcc")));
        return;
    }
    let dir = scratch("command-line-dll");
    let Some(library_object) = compile(&dir, "library", LIBRARY, &[]) else {
        return;
    };
    let implib = dir.join("libargv.dll.a");
    let def = dir.join("argv.def");
    let Some(mut argv) = link_argv(
        &dir,
        &[
            "-shared",
            library_object.as_str(),
            "-o",
            "argv.dll",
            "-fno-lto",
        ],
    ) else {
        return;
    };
    argv.push("--out-implib".to_owned());
    argv.push(implib.to_str().unwrap().to_owned());
    argv.push("--output-def".to_owned());
    argv.push(def.to_str().unwrap().to_owned());
    argv.push("--major-image-version=3".to_owned());
    argv.push("--minor-image-version=1".to_owned());
    argv.push("--disable-high-entropy-va".to_owned());

    // `--shared` and the MinGW options all come from the command line.
    let options = options_from(&argv, &dir.join("argv.dll"));
    assert_eq!(options.kind, qld::args::OutputKind::Shared);
    assert_eq!(options.pe.out_implib, Some(implib.clone()));
    let sink = Collect::new();
    qld::link(&options, &sink).expect("qld::link with a MinGW -shared command line");

    let Some(headers) = readobj(&dir, "argv.dll", &["--file-headers"]) else {
        return;
    };
    assert!(headers.contains("IMAGE_FILE_DLL"), "{headers}");
    assert!(headers.contains("MajorImageVersion: 3"), "{headers}");
    assert!(headers.contains("MinorImageVersion: 1"), "{headers}");
    assert!(
        !headers.contains("IMAGE_DLL_CHARACTERISTICS_HIGH_ENTROPY_VA"),
        "--disable-high-entropy-va was ignored:\n{headers}"
    );
    let Some(exports) = readobj(&dir, "argv.dll", &["--coff-exports"]) else {
        return;
    };
    assert!(exports.contains("add_one"), "{exports}");
    assert!(
        std::fs::read_to_string(&def).unwrap().contains("add_one @"),
        "--output-def wrote no export list"
    );

    // The executable, linked against that import library, again from argv.
    let Some(client_object) = compile(&dir, "client", CLIENT, &[]) else {
        return;
    };
    let Some(mut argv) = link_argv(
        &dir,
        &[client_object.as_str(), "-o", "argv-client.exe", "-fno-lto"],
    ) else {
        return;
    };
    argv.push(implib.to_str().unwrap().to_owned());
    argv.push("--subsystem".to_owned());
    argv.push("console,6.1".to_owned());
    argv.push("--stack".to_owned());
    argv.push("0x100000,0x2000".to_owned());
    let options = options_from(&argv, &dir.join("argv-client.exe"));
    let sink = Collect::new();
    qld::link(&options, &sink).expect("qld::link with a MinGW executable command line");

    let Some(headers) = readobj(&dir, "argv-client.exe", &["--file-headers"]) else {
        return;
    };
    assert!(headers.contains("IMAGE_SUBSYSTEM_WINDOWS_CUI"), "{headers}");
    assert!(headers.contains("MajorSubsystemVersion: 6"), "{headers}");
    assert!(headers.contains("MinorSubsystemVersion: 1"), "{headers}");
    assert!(headers.contains("SizeOfStackReserve: 1048576"), "{headers}");
    assert!(headers.contains("SizeOfStackCommit: 8192"), "{headers}");
    let Some(imports) = readobj(&dir, "argv-client.exe", &["--coff-imports"]) else {
        return;
    };
    assert!(imports.contains("argv.dll"), "{imports}");
    assert!(imports.contains("add_one"), "{imports}");
}

const CXX: &str = r#"
#include <cstdio>
#include <stdexcept>
#include <string>
struct Guard {
    const char* what;
    explicit Guard(const char* w) : what(w) {}
    ~Guard() { std::printf("unwound %s\n", what); }
};
static int deep(int n) {
    Guard g("deep");
    if (n == 0) throw std::runtime_error("boom");
    return deep(n - 1) + 1;
}
int main() {
    try {
        Guard g("main");
        return deep(3);
    } catch (const std::exception& e) {
        std::printf("caught %s\n", e.what());
    }
    return 0;
}
"#;

/// A C++ program with exceptions: the SEH unwind data (`.pdata`/`.xdata`)
/// must be present, and `.pdata` sorted by address.
///
/// RUN ON WINDOWS: the image should print three `unwound deep` lines, then
/// `unwound main`, then `caught boom`, and exit 0.
#[test]
fn cxx_exceptions_and_seh_unwind_data() {
    if tool(&mingw("g++")).is_none() {
        skip(&format!("{} not found", mingw("g++")));
        return;
    }
    let dir = scratch("cxx-exceptions");
    std::fs::write(dir.join("throw.cpp"), CXX).unwrap();
    if run(
        &mingw("g++"),
        &["-c", "-fno-lto", "throw.cpp", "-o", "throw.o"],
        &dir,
    )
    .is_none()
    {
        return;
    }
    let object = dir.join("throw.o").to_str().unwrap().to_string();
    let Some(argv) = gxx_link_argv(&dir, &[object.as_str(), "-o", "gnu.exe", "-fno-lto"]) else {
        return;
    };
    let options = options_from(&argv, &dir.join("qld.exe"));
    let pe = PeOptions::from_link_options(&options);
    if let Err(error) = qld_link(&options, &pe) {
        panic!("qld failed to link a C++ program with exceptions:\n{error}");
    }
    let Some(headers) = readobj(&dir, "qld.exe", &["--file-headers"]) else {
        return;
    };
    assert!(!headers.contains("ExceptionTableRVA: 0x0"), "{headers}");
    let Some(sections) = readobj(&dir, "qld.exe", &["--sections"]) else {
        return;
    };
    assert!(sections.contains(".pdata"), "{sections}");
    assert!(sections.contains(".xdata"), "{sections}");
    assert!(pdata_is_sorted(&dir, "qld.exe"), ".pdata is not sorted");
}

/// The `RUNTIME_FUNCTION` table of `file`, checked for ascending
/// `BeginAddress`, which the Windows unwinder's binary search needs.
fn pdata_is_sorted(dir: &Path, file: &str) -> bool {
    let Some(text) = readobj(dir, file, &["--sections"]) else {
        return true;
    };
    let mut offset = 0usize;
    let mut size = 0usize;
    let mut in_pdata = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with("Name: .pdata") {
            in_pdata = true;
        } else if in_pdata {
            if let Some(value) = line.strip_prefix("RawDataSize: ") {
                size = value.trim().parse().unwrap_or(0);
            } else if let Some(value) = line.strip_prefix("PointerToRawData: 0x") {
                offset = usize::from_str_radix(value.trim(), 16).unwrap_or(0);
                break;
            }
        }
    }
    if size == 0 {
        return true;
    }
    let Ok(bytes) = std::fs::read(dir.join(file)) else {
        return true;
    };
    let table = bytes
        .get(offset..offset.saturating_add(size))
        .unwrap_or(&[]);
    let mut previous = 0u32;
    for record in table.as_chunks::<12>().0 {
        let begin = u32::from_le_bytes([record[0], record[1], record[2], record[3]]);
        if begin == 0 {
            break;
        }
        if begin < previous {
            return false;
        }
        previous = begin;
    }
    true
}

/// The linker command line `g++` would use.
fn gxx_link_argv(dir: &Path, args: &[&str]) -> Option<Vec<String>> {
    link_argv_via(&mingw("g++"), dir, args)
}

const TLS: &str = r#"
#include <stdio.h>
__thread int tls_counter = 7;
int main(void) {
    tls_counter += 1;
    printf("%d\n", tls_counter);
    return tls_counter - 8;
}
"#;

/// Thread-local storage: the TLS data directory must point at `_tls_used`.
///
/// RUN ON WINDOWS: the image should print `8` and exit 0.
#[test]
fn thread_local_storage() {
    if tool(&mingw("gcc")).is_none() {
        skip(&format!("{} not found", mingw("gcc")));
        return;
    }
    let dir = scratch("thread-local-storage");
    let Some(object) = compile(&dir, "tls", TLS, &[]) else {
        return;
    };
    let Some(argv) = link_argv(&dir, &[object.as_str(), "-o", "gnu.exe", "-fno-lto"]) else {
        return;
    };
    let options = options_from(&argv, &dir.join("qld.exe"));
    let pe = PeOptions::from_link_options(&options);
    if let Err(error) = qld_link(&options, &pe) {
        panic!("qld failed to link a TLS program:\n{error}");
    }
    let Some(headers) = readobj(&dir, "qld.exe", &["--file-headers"]) else {
        return;
    };
    assert!(!headers.contains("TLSTableRVA: 0x0"), "{headers}");
    let Some(sections) = readobj(&dir, "qld.exe", &["--sections"]) else {
        return;
    };
    assert!(sections.contains(".tls"), "{sections}");
}

const RESOURCE: &str = r#"
STRINGTABLE
BEGIN
  1 "hello resource"
END
"#;

/// A resource object from `windres`: the `.rsrc` section and the resource
/// data directory must survive the link.
///
/// RUN ON WINDOWS: `LoadString` should find string 1.
#[test]
fn windres_resources() {
    if tool(&mingw("windres")).is_none() {
        skip(&format!("{} not found", mingw("windres")));
        return;
    }
    let dir = scratch("windres-resources");
    std::fs::write(dir.join("app.rc"), RESOURCE).unwrap();
    if run(
        &mingw("windres"),
        &["app.rc", "-O", "coff", "-o", "app-rc.o"],
        &dir,
    )
    .is_none()
    {
        return;
    }
    let Some(object) = compile(&dir, "res", HELLO, &[]) else {
        return;
    };
    let resource = dir.join("app-rc.o").to_str().unwrap().to_string();
    let Some(argv) = link_argv(
        &dir,
        &[
            object.as_str(),
            resource.as_str(),
            "-o",
            "gnu.exe",
            "-fno-lto",
        ],
    ) else {
        return;
    };
    let options = options_from(&argv, &dir.join("qld.exe"));
    let pe = PeOptions::from_link_options(&options);
    if let Err(error) = qld_link(&options, &pe) {
        panic!("qld failed to link a resource object:\n{error}");
    }
    let Some(headers) = readobj(&dir, "qld.exe", &["--file-headers"]) else {
        return;
    };
    assert!(!headers.contains("ResourceTableRVA: 0x0"), "{headers}");
    let Some(sections) = readobj(&dir, "qld.exe", &["--sections"]) else {
        return;
    };
    assert!(sections.contains(".rsrc"), "{sections}");
}

const DEF_LIBRARY: &str = r#"
int by_def(int value) { return value * 2; }
int also_by_def(void) { return 3; }
int hidden(void) { return 4; }
"#;

/// Exports named by a `.def` file, with an explicit ordinal.
#[test]
fn def_file_exports() {
    if tool(&mingw("gcc")).is_none() {
        skip(&format!("{} not found", mingw("gcc")));
        return;
    }
    let dir = scratch("def-file-exports");
    std::fs::write(
        dir.join("sample.def"),
        "LIBRARY defsample.dll\nEXPORTS\n  by_def @7\n  also_by_def\n",
    )
    .unwrap();
    let Some(object) = compile(&dir, "deflib", DEF_LIBRARY, &[]) else {
        return;
    };
    let Some(argv) = link_argv(
        &dir,
        &[
            "-shared",
            object.as_str(),
            "-o",
            "defsample.dll",
            "-fno-lto",
        ],
    ) else {
        return;
    };
    let mut options = options_from(&argv, &dir.join("defsample.dll"));
    options.kind = qld::args::OutputKind::Shared;
    let mut pe = PeOptions::from_link_options(&options);
    pe.def_file = Some(dir.join("sample.def"));
    if let Err(error) = qld_link(&options, &pe) {
        panic!("qld failed to link with a .def file:\n{error}");
    }
    let Some(exports) = readobj(&dir, "defsample.dll", &["--coff-exports"]) else {
        return;
    };
    assert!(exports.contains("by_def"), "{exports}");
    assert!(exports.contains("also_by_def"), "{exports}");
    assert!(
        !exports.contains("hidden"),
        "a .def file must limit the exports:\n{exports}"
    );
    assert!(exports.contains("Ordinal: 7"), "{exports}");
}

/// Options a later milestone covers are refused, not silently ignored.
#[test]
fn unimplemented_options_are_refused() {
    let dir = scratch("unimplemented-options");
    let base = LinkOptions {
        target: Some(pe_target()),
        output: Some(dir.join("out.exe")),
        ..LinkOptions::default()
    };
    for (name, options) in [
        (
            "-r",
            LinkOptions {
                kind: qld::args::OutputKind::Relocatable,
                ..base.clone()
            },
        ),
        (
            "--gc-sections",
            LinkOptions {
                gc_sections: true,
                ..base.clone()
            },
        ),
        (
            "--icf",
            LinkOptions {
                icf: Some("all".into()),
                ..base.clone()
            },
        ),
    ] {
        let pe = PeOptions::from_link_options(&options);
        let error = qld_link(&options, &pe).unwrap_err();
        assert!(
            error.contains("not implemented") || error.contains("PE/COFF"),
            "{name}: {error}"
        );
    }
}

/// A short (MSVC-style) import library from `llvm-dlltool`, and a DLL named
/// directly on the command line: both become the same `.idata$N` objects a
/// `dlltool` library holds.
#[test]
fn short_import_library_and_direct_dll() {
    if tool(&mingw("gcc")).is_none() {
        skip(&format!("{} not found", mingw("gcc")));
        return;
    }
    if tool("llvm-dlltool").is_none() {
        skip("llvm-dlltool not found");
        return;
    }
    let dir = scratch("short-import-library");
    // A DLL to import from, built with qld.
    let Some(library_object) = compile(&dir, "library", LIBRARY, &[]) else {
        return;
    };
    let Some(argv) = link_argv(
        &dir,
        &[
            "-shared",
            library_object.as_str(),
            "-o",
            "sample.dll",
            "-fno-lto",
        ],
    ) else {
        return;
    };
    let mut options = options_from(&argv, &dir.join("sample.dll"));
    options.kind = qld::args::OutputKind::Shared;
    let pe = PeOptions::from_link_options(&options);
    if let Err(error) = qld_link(&options, &pe) {
        panic!("qld failed to link the DLL:\n{error}");
    }

    // A short import library for the same DLL.
    std::fs::write(
        dir.join("short.def"),
        "LIBRARY sample.dll\nEXPORTS\n  add_one\n  greet\n  exported_data DATA\n",
    )
    .unwrap();
    if run(
        "llvm-dlltool",
        &[
            "-m",
            "i386:x86-64",
            "-d",
            "short.def",
            "-l",
            "sample-short.lib",
        ],
        &dir,
    )
    .is_none()
    {
        return;
    }

    let Some(client_object) = compile(&dir, "client", CLIENT, &[]) else {
        return;
    };
    let Some(argv) = link_argv(&dir, &[client_object.as_str(), "-o", "out.exe", "-fno-lto"]) else {
        return;
    };
    for (name, input) in [
        ("short import library", dir.join("sample-short.lib")),
        ("the DLL itself", dir.join("sample.dll")),
    ] {
        let output = dir.join(format!("client-{}.exe", name.replace(' ', "-")));
        let mut options = options_from(&argv, &output);
        options.inputs.push(InputSpec {
            kind: InputKind::File(input),
            attrs: InputAttrs::default(),
            position: options.inputs.len(),
        });
        let pe = PeOptions::from_link_options(&options);
        if let Err(error) = qld_link(&options, &pe) {
            panic!("qld failed to link against {name}:\n{error}");
        }
        let file = output.file_name().unwrap().to_str().unwrap();
        let Some(imports) = readobj(&dir, file, &["--coff-imports"]) else {
            return;
        };
        let entry: Vec<String> = imports
            .lines()
            .skip_while(|line| !line.contains("sample.dll"))
            .skip(1)
            .skip_while(|line| !line.trim().starts_with("Symbol:"))
            .take_while(|line| line.trim().starts_with("Symbol:"))
            .map(|line| line.trim().to_string())
            .collect();
        assert!(
            entry.iter().any(|line| line.contains("add_one"))
                && entry.iter().any(|line| line.contains("exported_data"))
                && entry.iter().any(|line| line.contains("greet")),
            "{name}: {imports}"
        );
    }
}

/// The image carries a COFF symbol table, and the symbols it shares with GNU
/// `ld`'s image sit at the same addresses.
#[test]
fn symbol_table_matches_gnu_ld() {
    if tool(&mingw("gcc")).is_none() || tool(&mingw("nm")).is_none() {
        skip("the MinGW toolchain is not available");
        return;
    }
    let dir = scratch("symbol-table");
    let Some(object) = compile(&dir, "hello", HELLO, &[]) else {
        return;
    };
    let Some(argv) = link_argv(&dir, &[object.as_str(), "-o", "gnu.exe", "-fno-lto"]) else {
        return;
    };
    if run(
        &mingw("gcc"),
        &[object.as_str(), "-o", "gnu.exe", "-fno-lto"],
        &dir,
    )
    .is_none()
    {
        return;
    }
    let options = options_from(&argv, &dir.join("qld.exe"));
    let pe = PeOptions::from_link_options(&options);
    if let Err(error) = qld_link(&options, &pe) {
        panic!("qld failed to link:\n{error}");
    }
    let symbols = |file: &str| -> Vec<(String, String)> {
        let Some(output) = run(&mingw("nm"), &[file], &dir) else {
            return Vec::new();
        };
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| {
                let mut parts = line.split_whitespace();
                let address = parts.next()?.to_string();
                let kind = parts.next()?;
                let name = parts.next()?.to_string();
                // Only global definitions; `U` entries have no address.
                "TtDdBbRr".contains(kind).then_some((name, address))
            })
            .collect()
    };
    let ours = symbols("qld.exe");
    let theirs: std::collections::HashMap<String, String> =
        symbols("gnu.exe").into_iter().collect();
    if ours.is_empty() || theirs.is_empty() {
        return;
    }
    assert!(
        ours.len() > 50,
        "qld wrote no useful symbol table: {} entries",
        ours.len()
    );
    // The entry point, `main` and a global live where GNU ld puts them.
    for name in ["main", "mainCRTStartup", "global_counter", "message"] {
        let ours = ours
            .iter()
            .find(|(symbol, _)| symbol == name)
            .map(|(_, address)| address.clone());
        let Some(theirs) = theirs.get(name) else {
            continue;
        };
        assert_eq!(
            ours.as_deref(),
            Some(theirs.as_str()),
            "{name} is not where GNU ld puts it"
        );
    }
}

const AUTO_IMPORT_CLIENT: &str = r#"
#include <stdio.h>
/* No __declspec(dllimport): the reference is direct, and MinGW auto-import
   has to bind it through the import address table at startup. */
extern int exported_data;
int *data_pointer = &exported_data;
__declspec(dllimport) void greet(void);
int main(void) {
    greet();
    printf("%d\n", *data_pointer);
    return 0;
}
"#;

/// MinGW auto-import: a pointer to a DLL's data that was compiled without
/// `__declspec(dllimport)` binds through the import address table, and the
/// image carries a runtime pseudo-relocation list for it.
///
/// RUN ON WINDOWS: the executable should print `hello from the dll` then
/// `41`, and exit 0.
#[test]
fn auto_import_and_runtime_pseudo_relocs() {
    if tool(&mingw("gcc")).is_none() {
        skip(&format!("{} not found", mingw("gcc")));
        return;
    }
    let dir = scratch("auto-import");
    let Some(library_object) = compile(&dir, "library", LIBRARY, &[]) else {
        return;
    };
    let Some(argv) = link_argv(
        &dir,
        &[
            "-shared",
            library_object.as_str(),
            "-o",
            "sample.dll",
            "-fno-lto",
        ],
    ) else {
        return;
    };
    let mut options = options_from(&argv, &dir.join("sample.dll"));
    options.kind = qld::args::OutputKind::Shared;
    let mut pe = PeOptions::from_link_options(&options);
    pe.out_implib = Some(dir.join("libsample.dll.a"));
    if let Err(error) = qld_link(&options, &pe) {
        panic!("qld failed to link the DLL:\n{error}");
    }

    let Some(client_object) = compile(&dir, "client", AUTO_IMPORT_CLIENT, &[]) else {
        return;
    };
    let Some(argv) = link_argv(&dir, &[client_object.as_str(), "-o", "out.exe", "-fno-lto"]) else {
        return;
    };
    let mut options = options_from(&argv, &dir.join("auto.exe"));
    options.inputs.push(InputSpec {
        kind: InputKind::File(dir.join("libsample.dll.a")),
        attrs: InputAttrs::default(),
        position: options.inputs.len(),
    });
    let pe = PeOptions::from_link_options(&options);
    if let Err(error) = qld_link(&options, &pe) {
        panic!("qld failed to auto-import a DLL's data:\n{error}");
    }
    // The pseudo-relocation list must be non-empty and bracketed by the
    // symbols the MinGW runtime walks.
    let Some(symbols) = run(&mingw("nm"), &["auto.exe"], &dir) else {
        return;
    };
    let text = String::from_utf8_lossy(&symbols.stdout).into_owned();
    let address = |name: &str| -> Option<u64> {
        text.lines().find_map(|line| {
            let mut parts = line.split_whitespace();
            let value = parts.next()?;
            let _kind = parts.next()?;
            (parts.next()? == name).then(|| u64::from_str_radix(value, 16).ok())?
        })
    };
    let (Some(start), Some(end)) = (
        address("__RUNTIME_PSEUDO_RELOC_LIST__"),
        address("__RUNTIME_PSEUDO_RELOC_LIST_END__"),
    ) else {
        panic!("the pseudo-relocation list bounds are missing:\n{text}");
    };
    assert!(
        end > start,
        "the pseudo-relocation list is empty ({start:#x}..{end:#x})"
    );
    // A version 2 header plus one 12-byte entry.
    assert_eq!(end - start, 24, "unexpected pseudo-relocation list size");

    // `--disable-auto-import` must refuse the same link rather than produce
    // an image that crashes.
    let mut pe = PeOptions::from_link_options(&options);
    pe.auto_import = qld::coff::options::AutoImport::Disabled;
    let error = qld_link(&options, &pe).unwrap_err();
    assert!(error.contains("exported_data"), "{error}");
}

/// A `-g` build keeps its DWARF sections, which is what MinGW debugging
/// needs: PE carries no separate debug directory here.
#[test]
fn dwarf_sections_survive() {
    if tool(&mingw("gcc")).is_none() {
        skip(&format!("{} not found", mingw("gcc")));
        return;
    }
    let dir = scratch("dwarf-sections");
    let Some(object) = compile(&dir, "debuggable", HELLO, &["-g"]) else {
        return;
    };
    let Some(argv) = link_argv(&dir, &[object.as_str(), "-o", "gnu.exe", "-g", "-fno-lto"]) else {
        return;
    };
    let options = options_from(&argv, &dir.join("qld.exe"));
    let pe = PeOptions::from_link_options(&options);
    if let Err(error) = qld_link(&options, &pe) {
        panic!("qld failed to link a -g build:\n{error}");
    }
    let Some(sections) = readobj(&dir, "qld.exe", &["--sections"]) else {
        return;
    };
    for name in [".debug_info", ".debug_line", ".debug_abbrev"] {
        assert!(sections.contains(name), "{name} missing:\n{sections}");
    }
    // Debug sections must be discardable, so the loader does not map them.
    assert!(sections.contains("IMAGE_SCN_MEM_DISCARDABLE"), "{sections}");
}

/// The image is byte-identical across repeated links and thread counts.
#[test]
fn output_is_deterministic() {
    if tool(&mingw("gcc")).is_none() {
        skip(&format!("{} not found", mingw("gcc")));
        return;
    }
    let dir = scratch("determinism");
    let Some(object) = compile(&dir, "hello", HELLO, &[]) else {
        return;
    };
    let Some(argv) = link_argv(&dir, &[object.as_str(), "-o", "gnu.exe", "-fno-lto"]) else {
        return;
    };
    let mut first: Option<Vec<u8>> = None;
    for threads in [Some(1), Some(8), None] {
        for run in 0..2 {
            let name = format!("out-{}-{run}.exe", threads.unwrap_or(0));
            let mut options = options_from(&argv, &dir.join(&name));
            options.threads = threads;
            let pe = PeOptions::from_link_options(&options);
            if let Err(error) = qld_link(&options, &pe) {
                panic!("qld failed to link:\n{error}");
            }
            let bytes = std::fs::read(dir.join(&name)).unwrap();
            match &first {
                None => first = Some(bytes),
                Some(expected) => {
                    assert_eq!(
                        bytes.len(),
                        expected.len(),
                        "{name} differs in length from the first link"
                    );
                    assert!(
                        &bytes == expected,
                        "{name} is not byte-identical to the first link"
                    );
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// i386 (PE32): workstream W33
// ---------------------------------------------------------------------------

/// The name to invoke an i686 MinGW tool by: `i686-w64-mingw32-<base>`,
/// which cross toolchains and MSYS2's MINGW32 environment both provide, or
/// the prefix `QLD_MINGW32_PREFIX` names.
fn mingw32(base: &str) -> String {
    let prefix =
        std::env::var("QLD_MINGW32_PREFIX").unwrap_or_else(|_| "i686-w64-mingw32-".to_owned());
    format!("{prefix}{base}")
}

/// The i686 MinGW compiler, or `None` (after [`skip`]) when it is missing.
fn i386_gcc() -> Option<String> {
    let gcc = mingw32("gcc");
    if tool(&gcc).is_none() {
        skip(&format!("{gcc} not found"));
        return None;
    }
    Some(gcc)
}

/// GNU ld's `i386pe` target.
fn i386_target() -> Target {
    Target {
        arch: Architecture::X86,
        os: OperatingSystem::Windows,
        format: BinaryFormat::Pe,
        endian: Endianness::Little,
        pointer_width: PointerWidth::Bits32,
    }
}

/// A prebuilt fixture from `tests/data/coff_link`.
fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/data/coff_link")
        .join(name)
}

/// The value of `field` in `llvm-readobj --file-headers` output.
fn header_field<'t>(headers: &'t str, field: &str) -> Option<&'t str> {
    headers.lines().find_map(|line| {
        line.trim()
            .strip_prefix(field)
            .and_then(|rest| rest.strip_prefix(": "))
    })
}

/// The disassembly of `symbol`, with addresses and numbers erased so that
/// two images laid out differently compare equal when their code refers to
/// the same symbols.
fn normalized_disassembly(dir: &Path, file: &str, symbol: &str) -> Option<Vec<String>> {
    let option = format!("--disassemble-symbols={symbol}");
    let output = run(
        "llvm-objdump",
        &[
            option.as_str(),
            "--no-show-raw-insn",
            "--no-leading-addr",
            file,
        ],
        dir,
    )?;
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    let mut lines = Vec::new();
    for line in text
        .lines()
        .skip_while(|line| !line.contains(&format!("<{symbol}>:")))
    {
        let mut normalized = String::new();
        let mut chars = line.trim().chars().peekable();
        while let Some(c) = chars.next() {
            if c == '0' && chars.peek() == Some(&'x') {
                chars.next();
                while chars.peek().is_some_and(char::is_ascii_hexdigit) {
                    chars.next();
                }
                normalized.push('N');
            } else if c == '+' {
                // `<symbol+0x10>`: offsets inside functions move too.
                while chars.peek().is_some_and(|&c| c != '>') {
                    chars.next();
                }
            } else {
                normalized.push(c);
            }
        }
        if !normalized.is_empty() {
            lines.push(normalized);
        }
    }
    // The padding after the function depends on what follows it.
    while lines.last().is_some_and(|line| line == "nop") {
        lines.pop();
    }
    Some(lines)
}

/// The export names of a DLL, in table order.
fn export_names(dir: &Path, file: &str) -> Option<Vec<String>> {
    Some(
        readobj(dir, file, &["--coff-exports"])?
            .lines()
            .filter_map(|line| line.trim().strip_prefix("Name: "))
            .map(str::to_owned)
            .collect(),
    )
}

/// The external symbols an import library defines, sorted, without the
/// helper symbols GNU and qld name differently (`_head_`, `_iname`,
/// `__nm_`).
fn implib_symbols(dir: &Path, file: &str) -> Option<Vec<String>> {
    let output = run("llvm-nm", &["--defined-only", "--extern-only", file], dir)?;
    let mut names: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.split_whitespace().nth(2))
        .filter(|name| {
            !name.contains("_head_") && !name.ends_with("_iname") && !name.starts_with("__nm_")
        })
        .map(str::to_owned)
        .collect();
    names.sort();
    names.dedup();
    Some(names)
}

/// A console program linked through the whole i686 MinGW runtime: the PE32
/// header fields must be GNU ld's `i386pe` defaults.
///
/// RUN ON WINDOWS: the image should print `hello from qld` and exit 0.
#[test]
fn i386_console_hello_world() {
    let Some(gcc) = i386_gcc() else {
        return;
    };
    let dir = scratch("i386-hello");
    let Some(object) = compile_via(&gcc, &dir, "hello", HELLO, &[]) else {
        return;
    };
    let args = [object.as_str(), "-o", "gnu.exe", "-fno-lto"];
    let Some(argv) = link_argv_via(&gcc, &dir, &args) else {
        return;
    };
    if run(&gcc, &args, &dir).is_none() {
        return;
    }
    let options = options_for(&argv, &dir.join("qld.exe"), i386_target());
    let pe = PeOptions::from_link_options(&options);
    if let Err(error) = qld_link(&options, &pe) {
        panic!("qld failed to link the i386 hello world:\n{error}");
    }

    let Some(ours) = readobj(&dir, "qld.exe", &["--file-headers"]) else {
        return;
    };
    let Some(theirs) = readobj(&dir, "gnu.exe", &["--file-headers"]) else {
        return;
    };
    assert!(ours.contains("IMAGE_FILE_MACHINE_I386"), "{ours}");
    assert!(ours.contains("IMAGE_FILE_32BIT_MACHINE"), "{ours}");
    assert!(!ours.contains("LARGE_ADDRESS_AWARE"), "{ours}");
    assert!(!ours.contains("HIGH_ENTROPY_VA"), "{ours}");
    for field in [
        "Magic",
        "ImageBase",
        "SectionAlignment",
        "FileAlignment",
        "MajorOperatingSystemVersion",
        "MinorOperatingSystemVersion",
        "MajorImageVersion",
        "MinorImageVersion",
        "MajorSubsystemVersion",
        "MinorSubsystemVersion",
        "Subsystem",
        "SizeOfStackReserve",
        "SizeOfHeapReserve",
        "SizeOfHeaders",
        "BaseOfCode",
        "AddressOfEntryPoint",
        "TLSTableSize",
        "OptionalHeaderSize",
    ] {
        assert_eq!(
            header_field(&ours, field),
            header_field(&theirs, field),
            "{field} differs from GNU ld"
        );
    }
    let flags = |text: &str| -> Vec<String> {
        text.lines()
            .map(str::trim)
            .filter(|line| line.starts_with("IMAGE_"))
            .map(str::to_owned)
            .collect()
    };
    assert_eq!(flags(&ours), flags(&theirs), "header flags differ");

    // The user's code refers to the same symbols in both images.
    let (Some(mine), Some(gnu)) = (
        normalized_disassembly(&dir, "qld.exe", "_main"),
        normalized_disassembly(&dir, "gnu.exe", "_main"),
    ) else {
        return;
    };
    assert!(!mine.is_empty(), "no disassembly for _main");
    assert_eq!(mine, gnu, "_main differs from GNU ld's");

    // `--large-address-aware` is off by default on i386, and can be asked for.
    let mut argv = argv;
    argv.push("--large-address-aware".into());
    let options = options_for(&argv, &dir.join("laa.exe"), i386_target());
    let pe = PeOptions::from_link_options(&options);
    qld_link(&options, &pe).expect("qld --large-address-aware");
    let Some(laa) = readobj(&dir, "laa.exe", &["--file-headers"]) else {
        return;
    };
    assert!(laa.contains("IMAGE_FILE_LARGE_ADDRESS_AWARE"), "{laa}");
}

const LIBRARY32: &str = r#"
__declspec(dllexport) int __stdcall add_std(int a, int b) { return a + b; }
__declspec(dllexport) int __fastcall twice_fast(int a) { return a * 2; }
__declspec(dllexport) int plus_one(int a) { return a + 1; }
__declspec(dllexport) int exported_value = 3;
__declspec(dllexport) int auto_value = 99;
int __stdcall not_exported(int a) { return a; }
"#;

const CLIENT32: &str = r#"
#include <stdio.h>
__declspec(dllimport) int __stdcall add_std(int, int);
__declspec(dllimport) int __fastcall twice_fast(int);
int plus_one(int);              /* called through the import thunk */
__declspec(dllimport) extern int exported_value;
extern int auto_value;          /* no dllimport: MinGW auto-import */
int main(void) {
    printf("%d %d %d %d %d\n", add_std(3, 4), twice_fast(5), plus_one(4),
           exported_value, auto_value);
    return 0;
}
"#;

/// The exports of an i386 DLL with `__stdcall`, `__fastcall`, cdecl and
/// data exports, and the symbols of its import library, match GNU ld's
/// under `--kill-at`, `--add-stdcall-alias` and `--export-all-symbols`.
#[test]
fn i386_dll_exports_match_gnu_ld() {
    let Some(gcc) = i386_gcc() else {
        return;
    };
    let dir = scratch("i386-exports");
    let Some(object) = compile_via(&gcc, &dir, "library", LIBRARY32, &["-O2"]) else {
        return;
    };
    for (index, extra) in [
        &[][..],
        &["-Wl,--kill-at"],
        &["-Wl,--add-stdcall-alias"],
        &["-Wl,--export-all-symbols"],
        &["-Wl,--kill-at,--export-all-symbols"],
    ]
    .iter()
    .enumerate()
    {
        let gnu_dll = format!("gnu{index}.dll");
        let gnu_lib = format!("-Wl,--out-implib,libgnu{index}.a");
        let mut args = vec!["-shared", object.as_str(), "-o", gnu_dll.as_str()];
        args.extend_from_slice(extra);
        args.push(gnu_lib.as_str());
        if run(&gcc, &args, &dir).is_none() {
            return;
        }
        let ours_dll = format!("qld{index}.dll");
        let ours_lib = format!("-Wl,--out-implib,libqld{index}.a");
        let mut args = vec!["-shared", object.as_str(), "-o", ours_dll.as_str()];
        args.extend_from_slice(extra);
        args.push(ours_lib.as_str());
        let Some(argv) = link_argv_via(&gcc, &dir, &args) else {
            return;
        };
        let mut options = options_for(&argv, &dir.join(&ours_dll), i386_target());
        // The link line names the import library relative to `dir`.
        options.pe.out_implib = Some(dir.join(format!("libqld{index}.a")));
        let pe = PeOptions::from_link_options(&options);
        if let Err(error) = qld_link(&options, &pe) {
            panic!("qld failed to link the i386 DLL ({extra:?}):\n{error}");
        }
        assert_eq!(
            export_names(&dir, &ours_dll),
            export_names(&dir, &gnu_dll),
            "exports differ from GNU ld with {extra:?}"
        );
        assert_eq!(
            implib_symbols(&dir, &format!("libqld{index}.a")),
            implib_symbols(&dir, &format!("libgnu{index}.a")),
            "import library symbols differ from GNU ld with {extra:?}"
        );
    }
}

/// An i386 DLL and a client that calls its stdcall, fastcall and cdecl
/// functions and reads its data through `__declspec(dllimport)` and through
/// auto-import, linked against qld's own import library.
///
/// RUN ON WINDOWS: `client.exe` should print `7 10 5 3 99` and exit 0.
#[test]
fn i386_dll_and_client() {
    let Some(gcc) = i386_gcc() else {
        return;
    };
    let dir = scratch("i386-dll");
    let Some(library) = compile_via(&gcc, &dir, "library", LIBRARY32, &[]) else {
        return;
    };
    let Some(argv) = link_argv_via(
        &gcc,
        &dir,
        &["-shared", library.as_str(), "-o", "sample.dll", "-fno-lto"],
    ) else {
        return;
    };
    let mut options = options_for(&argv, &dir.join("sample.dll"), i386_target());
    options.kind = qld::args::OutputKind::Shared;
    let mut pe = PeOptions::from_link_options(&options);
    pe.out_implib = Some(dir.join("libsample.dll.a"));
    if let Err(error) = qld_link(&options, &pe) {
        panic!("qld failed to link the i386 DLL:\n{error}");
    }
    let Some(headers) = readobj(&dir, "sample.dll", &["--file-headers"]) else {
        return;
    };
    assert!(headers.contains("ImageBase: 0x10000000"), "{headers}");
    assert!(headers.contains("IMAGE_FILE_DLL"), "{headers}");

    let Some(client) = compile_via(&gcc, &dir, "client", CLIENT32, &[]) else {
        return;
    };
    let Some(argv) = link_argv_via(&gcc, &dir, &[client.as_str(), "-o", "gnu.exe", "-fno-lto"])
    else {
        return;
    };
    let mut options = options_for(&argv, &dir.join("client.exe"), i386_target());
    let implib = dir.join("libsample.dll.a");
    options.inputs.push(InputSpec {
        kind: InputKind::File(implib.clone()),
        attrs: InputAttrs::default(),
        position: options.inputs.len(),
    });
    let pe = PeOptions::from_link_options(&options);
    if let Err(error) = qld_link(&options, &pe) {
        panic!("qld failed to link the i386 client:\n{error}");
    }
    let Some(imports) = readobj(&dir, "client.exe", &["--coff-imports"]) else {
        return;
    };
    for name in [
        "add_std@8",
        "@twice_fast@4",
        "plus_one",
        "exported_value",
        "auto_value",
    ] {
        assert!(imports.contains(name), "{name} is not imported:\n{imports}");
    }
    // One 32-bit runtime pseudo-relocation, for `auto_value`.
    let Some(symbols) = run(&mingw32("nm"), &["client.exe"], &dir) else {
        return;
    };
    let text = String::from_utf8_lossy(&symbols.stdout).into_owned();
    let address = |name: &str| -> Option<u64> {
        text.lines().find_map(|line| {
            let mut parts = line.split_whitespace();
            let value = parts.next()?;
            let _kind = parts.next()?;
            (parts.next()? == name).then(|| u64::from_str_radix(value, 16).ok())?
        })
    };
    let (Some(start), Some(end)) = (
        address("___RUNTIME_PSEUDO_RELOC_LIST__"),
        address("___RUNTIME_PSEUDO_RELOC_LIST_END__"),
    ) else {
        panic!("the pseudo-relocation list bounds are missing:\n{text}");
    };
    assert_eq!(end - start, 24, "expected a header and one entry");

    // GNU ld must read qld's import library the same way.
    let gnu_args = [
        client.as_str(),
        implib.to_str().unwrap(),
        "-o",
        "gnu-client.exe",
        "-fno-lto",
    ];
    if run(&gcc, &gnu_args, &dir).is_some() {
        let Some(theirs) = readobj(&dir, "gnu-client.exe", &["--coff-imports"]) else {
            return;
        };
        let symbols_of = |text: &str| -> Vec<String> {
            let mut names: Vec<String> = text
                .lines()
                .skip_while(|line| !line.contains("sample.dll"))
                .filter_map(|line| line.trim().strip_prefix("Symbol: "))
                .take(5)
                .map(str::to_owned)
                .collect();
            names.sort();
            names
        };
        assert_eq!(
            symbols_of(&imports),
            symbols_of(&theirs),
            "GNU ld read qld's i386 import library differently"
        );
    }
}

/// GNU ld's stdcall fixup: a reference to `_foo@4` binds to a cdecl `_foo`
/// with a warning, silently with `--enable-stdcall-fixup`, and not at all
/// with `--disable-stdcall-fixup`.
#[test]
fn i386_stdcall_fixup() {
    let Some(gcc) = i386_gcc() else {
        return;
    };
    let dir = scratch("i386-stdcall-fixup");
    let Some(caller) = compile_via(
        &gcc,
        &dir,
        "caller",
        "int __stdcall foo(int);\nint main(void) { return foo(0); }\n",
        &[],
    ) else {
        return;
    };
    let Some(callee) = compile_via(&gcc, &dir, "callee", "int foo(int x) { return x; }\n", &[])
    else {
        return;
    };
    let base = [
        caller.as_str(),
        callee.as_str(),
        "-o",
        "gnu.exe",
        "-fno-lto",
    ];
    let link = |flag: Option<&str>| -> Result<String, String> {
        let mut args = base.to_vec();
        args.extend(flag);
        let Some(argv) = link_argv_via(&gcc, &dir, &args) else {
            return Ok("SKIPPED".into());
        };
        let options = options_for(&argv, &dir.join("qld.exe"), i386_target());
        let pe = PeOptions::from_link_options(&options);
        qld_link(&options, &pe)
    };
    let warned = link(None).expect("the default fixes the reference up");
    assert!(
        warned.contains("resolving _foo@4 by linking to _foo") || warned == "SKIPPED",
        "{warned}"
    );
    let quiet = link(Some("-Wl,--enable-stdcall-fixup")).expect("--enable-stdcall-fixup");
    assert!(!quiet.contains("resolving"), "{quiet}");
    let error = link(Some("-Wl,--disable-stdcall-fixup")).unwrap_err();
    assert!(error.contains("_foo@4"), "{error}");
}

const NATIVE_TLS_MAIN: &str = r#"
#include <stdio.h>
int bump_native_tls(void);
int main(void) {
    printf("%d\n", bump_native_tls());
    return 0;
}
"#;

/// Native i386 TLS, as Clang compiles it: `__tls_index`, a `SECREL` into
/// `.tls`, and the runtime's 24-byte TLS directory.
///
/// RUN ON WINDOWS: the image should print `8` and exit 0.
#[test]
fn i386_thread_local_storage() {
    let Some(gcc) = i386_gcc() else {
        return;
    };
    let dir = scratch("i386-tls");
    let Some(main) = compile_via(&gcc, &dir, "main", NATIVE_TLS_MAIN, &[]) else {
        return;
    };
    let tls = fixture("tls-native-i386.o");
    let tls = tls.to_str().unwrap();
    let args = [main.as_str(), tls, "-o", "gnu.exe", "-fno-lto"];
    let Some(argv) = link_argv_via(&gcc, &dir, &args) else {
        return;
    };
    let options = options_for(&argv, &dir.join("qld.exe"), i386_target());
    let pe = PeOptions::from_link_options(&options);
    if let Err(error) = qld_link(&options, &pe) {
        panic!("qld failed to link native i386 TLS:\n{error}");
    }
    let Some(headers) = readobj(&dir, "qld.exe", &["--file-headers"]) else {
        return;
    };
    assert_eq!(
        header_field(&headers, "TLSTableSize"),
        Some("0x18"),
        "{headers}"
    );
    if run(&gcc, &args, &dir).is_some() {
        assert_eq!(
            normalized_disassembly(&dir, "qld.exe", "_bump_native_tls"),
            normalized_disassembly(&dir, "gnu.exe", "_bump_native_tls"),
        );
        // The SECREL displacement is the variable's offset in `.tls`, which
        // both linkers place at the same offset.
        let tls_of = |file: &str| -> Option<String> {
            let output = run(
                "llvm-objdump",
                &["--disassemble-symbols=_bump_native_tls", file],
                &dir,
            )?;
            let text = String::from_utf8_lossy(&output.stdout).into_owned();
            text.lines()
                .find(|line| line.contains("(%ecx), %eax"))
                .map(|line| line.rsplit('\t').next().unwrap_or("").to_owned())
        };
        assert_eq!(
            tls_of("qld.exe"),
            tls_of("gnu.exe"),
            "SECREL encodings differ"
        );
    }
}

/// i386 C++ exceptions, which MinGW unwinds with SJLJ or DWARF data.
///
/// RUN ON WINDOWS: the image should print `caught boom` and exit 0.
#[test]
fn i386_cxx_exceptions() {
    let gxx = mingw32("g++");
    if tool(&gxx).is_none() {
        skip(&format!("{gxx} not found"));
        return;
    }
    let dir = scratch("i386-cxx");
    std::fs::write(dir.join("throw.cpp"), CXX).unwrap();
    if run(
        &gxx,
        &["-c", "-fno-lto", "throw.cpp", "-o", "throw.o"],
        &dir,
    )
    .is_none()
    {
        return;
    }
    let object = dir.join("throw.o").to_str().unwrap().to_string();
    let Some(argv) = link_argv_via(&gxx, &dir, &[object.as_str(), "-o", "gnu.exe", "-fno-lto"])
    else {
        return;
    };
    let options = options_for(&argv, &dir.join("qld.exe"), i386_target());
    let pe = PeOptions::from_link_options(&options);
    if let Err(error) = qld_link(&options, &pe) {
        panic!("qld failed to link an i386 C++ program:\n{error}");
    }
    // The i686 toolchain may unwind with SJLJ or with DWARF `.eh_frame`;
    // either way the image has GNU ld's sections.
    if run(&gxx, &[object.as_str(), "-o", "gnu.exe", "-fno-lto"], &dir).is_none() {
        return;
    }
    let names = |file: &str| -> Vec<String> {
        readobj(&dir, file, &["--sections"])
            .unwrap_or_default()
            .lines()
            .filter_map(|line| line.trim().strip_prefix("Name: "))
            .map(|name| name.split_whitespace().next().unwrap_or("").to_string())
            .collect()
    };
    assert_eq!(
        names("qld.exe"),
        names("gnu.exe"),
        "sections differ from GNU ld"
    );
}

/// Links the SafeSEH fixture `object` into `output` with `pe` tweaks.
fn link_safeseh(
    dir: &Path,
    kernel32: &Path,
    object: &str,
    output: &str,
    tweak: impl FnOnce(&mut PeOptions),
) -> Result<String, String> {
    let mut options = LinkOptions {
        target: Some(i386_target()),
        output: Some(dir.join(output)),
        entry: Some("_start".into()),
        ..LinkOptions::default()
    };
    for (position, path) in [fixture(object), kernel32.to_path_buf()]
        .into_iter()
        .enumerate()
    {
        options.inputs.push(InputSpec {
            kind: InputKind::File(path),
            attrs: InputAttrs::default(),
            position,
        });
    }
    let mut pe = PeOptions::from_link_options(&options);
    tweak(&mut pe);
    qld_link(&options, &pe)
}

/// SafeSEH: an object that registers its handler in `.sxdata` and a load
/// configuration that refers to `___safe_se_handler_table` get a sorted
/// handler table; `/SAFESEH`-style strictness rejects an object without
/// `@feat.00`.
///
/// RUN ON WINDOWS: `registered.exe` and `registered-win6.exe` exit with 42
/// (the handler resumed after the access violation); `unregistered.exe`,
/// which registers a decoy instead, must die of the access violation.
#[test]
fn i386_safeseh() {
    let Some(gcc) = i386_gcc() else {
        return;
    };
    let dir = scratch("i386-safeseh");
    let Some(output) = run(&gcc, &["-print-file-name=libkernel32.a"], &dir) else {
        return;
    };
    let kernel32 = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    if !kernel32.is_file() {
        skip("libkernel32.a not found for i686");
        return;
    }
    link_safeseh(&dir, &kernel32, "safeseh-i386.o", "registered.exe", |_| {})
        .expect("qld failed to link the SafeSEH program");
    // The same with a subsystem version past Windows XP, where the data
    // directory holds the structure's real size.
    link_safeseh(
        &dir,
        &kernel32,
        "safeseh-i386.o",
        "registered-win6.exe",
        |pe| {
            pe.subsystem_version = qld::coff::options::Version::new(6, 0);
        },
    )
    .expect("qld failed to link the SafeSEH program for Windows 6.0");
    link_safeseh(
        &dir,
        &kernel32,
        "safeseh-i386-unregistered.o",
        "unregistered.exe",
        |_| {},
    )
    .expect("qld failed to link the decoy SafeSEH program");

    let Some(config) = readobj(
        &dir,
        "registered.exe",
        &["--coff-load-config", "--file-headers"],
    ) else {
        return;
    };
    assert!(config.contains("SEHandlerCount: 1"), "{config}");
    assert_eq!(
        header_field(&config, "LoadConfigTableSize"),
        Some("0x40"),
        "Windows XP compatibility wants 64 for subsystem 4.0:\n{config}"
    );
    let entry = header_field(&config, "AddressOfEntryPoint")
        .and_then(|value| u64::from_str_radix(value.trim_start_matches("0x"), 16).ok())
        .expect("entry point");
    // `_handler` is 0x2b bytes into `_start`'s section, the decoy 0x3c.
    let handler = 0x40_0000 + entry + 0x2b;
    assert!(
        config.contains(&format!("{handler:#X}").replace("0X", "0x")),
        "the handler {handler:#x} is not registered:\n{config}"
    );
    let Some(win6) = readobj(&dir, "registered-win6.exe", &["--file-headers"]) else {
        return;
    };
    assert_eq!(header_field(&win6, "LoadConfigTableSize"), Some("0x48"));
    let Some(decoy) = readobj(&dir, "unregistered.exe", &["--coff-load-config"]) else {
        return;
    };
    let decoy_address = 0x40_0000 + entry + 0x3c;
    assert!(
        decoy.contains(&format!("{decoy_address:#X}").replace("0X", "0x")),
        "the decoy {decoy_address:#x} is not registered:\n{decoy}"
    );

    // `/SAFESEH` strictness: GCC does not mark its objects compatible.
    let Some(object) = compile_via(&gcc, &dir, "plain", "int plain(void) { return 1; }\n", &[])
    else {
        return;
    };
    let mut options = LinkOptions {
        target: Some(i386_target()),
        output: Some(dir.join("strict.exe")),
        entry: Some("_start".into()),
        ..LinkOptions::default()
    };
    for (position, path) in [
        fixture("safeseh-i386.o"),
        PathBuf::from(&object),
        kernel32.clone(),
    ]
    .into_iter()
    .enumerate()
    {
        options.inputs.push(InputSpec {
            kind: InputKind::File(path),
            attrs: InputAttrs::default(),
            position,
        });
    }
    let mut pe = PeOptions::from_link_options(&options);
    pe.safe_seh = Some(true);
    let error = qld_link(&options, &pe).unwrap_err();
    assert!(error.contains("plain.o"), "{error}");
    // Without strictness the same link succeeds with no table.
    pe.safe_seh = None;
    qld_link(&options, &pe).expect("a non-SafeSEH object only drops the table");
    let Some(config) = readobj(&dir, "strict.exe", &["--coff-load-config"]) else {
        return;
    };
    assert!(config.contains("SEHandlerCount: 0"), "{config}");
}

/// The emulation and `--oformat` must agree, and an object for another
/// machine is refused with GNU ld's wording.
#[test]
fn i386_target_checks() {
    let parse = |args: &[&str]| -> LinkOptions {
        let words: Vec<std::ffi::OsString> = std::iter::once("qld")
            .chain(args.iter().copied())
            .map(std::ffi::OsString::from)
            .collect();
        match qld::parse_gnu(&words) {
            Ok(qld::ParseOutcome::Link(options)) => *options,
            other => panic!("cannot parse {args:?}: {other:?}"),
        }
    };
    let dir = scratch("i386-target-checks");
    let options = parse(&[
        "-m",
        "i386pe",
        "--oformat",
        "pe-x86-64",
        "unused.o",
        "-o",
        "x.exe",
    ]);
    let error = qld_link(&options, &PeOptions::from_link_options(&options)).unwrap_err();
    assert!(error.contains("pei-i386"), "{error}");

    // An x86-64 object in an i386 link.
    if tool(&mingw("gcc")).is_none() {
        skip(&format!("{} not found", mingw("gcc")));
        return;
    }
    let Some(object) = compile(&dir, "x64", "int x64(void) { return 64; }\n", &[]) else {
        return;
    };
    let output = dir.join("mixed.exe");
    let options = parse(&[
        "-m",
        "i386pe",
        object.as_str(),
        "-o",
        output.to_str().unwrap(),
    ]);
    let error = qld_link(&options, &PeOptions::from_link_options(&options)).unwrap_err();
    assert!(
        error.contains("amd64 architecture of input file is incompatible with i386 output"),
        "{error}"
    );
}
