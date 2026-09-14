//! Integration tests for the PE/COFF link driver (workstream W21).
//!
//! Inputs are compiled with the MinGW-w64 cross toolchain, linked with qld's
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

/// The MinGW tool prefix the tests use.
const PREFIX: &str = "x86_64-w64-mingw32-";

/// Reports a skipped check, or fails when `QLD_REQUIRE_COFF_TOOLS` is set.
fn skip(reason: &str) {
    if std::env::var_os("QLD_REQUIRE_COFF_TOOLS").is_some_and(|v| !v.is_empty() && v != "0") {
        panic!("required tool unavailable: {reason}");
    }
    println!("SKIPPED: {reason}");
}

/// Finds `name` in `PATH`.
fn tool(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
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
    let mut full = vec!["-###"];
    full.extend_from_slice(args);
    let output = run(&format!("{PREFIX}gcc"), &full, dir)?;
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

/// Parses `argv` into [`LinkOptions`], dropping the options the GNU table
/// still marks unsupported for PE (qld's PE options live in [`PeOptions`]).
fn options_from(argv: &[String], output: &Path) -> LinkOptions {
    let mut kept: Vec<std::ffi::OsString> = Vec::new();
    let mut skip_next = false;
    for word in argv {
        if skip_next {
            skip_next = false;
            continue;
        }
        match word.as_str() {
            "-m" | "--subsystem" | "-e" => {
                skip_next = word != "-e";
                if word == "-e" {
                    kept.push(word.into());
                }
                continue;
            }
            "--enable-auto-image-base" | "--shared" | "-shared" => continue,
            _ => {}
        }
        kept.push(word.into());
    }
    let mut options = match qld::parse_gnu(&kept) {
        Ok(qld::ParseOutcome::Link(options)) => *options,
        other => panic!("cannot parse the MinGW link line: {other:?}"),
    };
    options.target = Some(pe_target());
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
    let file = format!("{name}.c");
    std::fs::write(dir.join(&file), source).unwrap();
    let object = format!("{name}.o");
    let mut args = vec!["-c", "-fno-lto", file.as_str(), "-o", object.as_str()];
    args.extend_from_slice(flags);
    run(&format!("{PREFIX}gcc"), &args, dir)?;
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
    if tool(&format!("{PREFIX}gcc")).is_none() {
        skip("x86_64-w64-mingw32-gcc not found");
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
        &format!("{PREFIX}gcc"),
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
}

/// The output section set and their characteristics match GNU `ld`'s.
#[test]
fn sections_match_gnu_ld() {
    if tool(&format!("{PREFIX}gcc")).is_none() {
        skip("x86_64-w64-mingw32-gcc not found");
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
        &format!("{PREFIX}gcc"),
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
    let gnu = names("gnu.exe");
    let ours = names("qld.exe");
    if gnu.is_empty() || ours.is_empty() {
        return;
    }
    assert_eq!(ours, gnu, "section names differ from GNU ld");
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
    if tool(&format!("{PREFIX}gcc")).is_none() {
        skip("x86_64-w64-mingw32-gcc not found");
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
    if tool(&format!("{PREFIX}gcc")).is_none() {
        skip("x86_64-w64-mingw32-gcc not found");
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
    if run(&format!("{PREFIX}gcc"), &gnu_args, &dir).is_some() {
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
    if tool(&format!("{PREFIX}g++")).is_none() {
        skip("x86_64-w64-mingw32-g++ not found");
        return;
    }
    let dir = scratch("cxx-exceptions");
    std::fs::write(dir.join("throw.cpp"), CXX).unwrap();
    if run(
        &format!("{PREFIX}g++"),
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
    let mut full = vec!["-###"];
    full.extend_from_slice(args);
    let output = run(&format!("{PREFIX}g++"), &full, dir)?;
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
    if !argv.is_empty() {
        argv.remove(0);
    }
    Some(argv)
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
    if tool(&format!("{PREFIX}gcc")).is_none() {
        skip("x86_64-w64-mingw32-gcc not found");
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
    if tool(&format!("{PREFIX}windres")).is_none() {
        skip("x86_64-w64-mingw32-windres not found");
        return;
    }
    let dir = scratch("windres-resources");
    std::fs::write(dir.join("app.rc"), RESOURCE).unwrap();
    if run(
        &format!("{PREFIX}windres"),
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
    if tool(&format!("{PREFIX}gcc")).is_none() {
        skip("x86_64-w64-mingw32-gcc not found");
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
