//! Integration tests for ARM64 PE32+ output (workstream W33).
//!
//! No MinGW toolchain for ARM64 is needed: the inputs are the prebuilt
//! objects in `tests/data/coff_link` (see `generate.sh` there) — a
//! freestanding program that calls kernel32 through an `llvm-dlltool`
//! import library, branches that need range-extension thunks, packed and
//! unpacked unwind data, and TLS reached through `SECREL` relocations —
//! plus a padding object this file writes. The images are checked with
//! `llvm-readobj` and `llvm-objdump`, and compared with lld's (`ld.lld -m
//! arm64pe`, the linker llvm-mingw uses) when it is installed.
//!
//! The fixtures marked `RUN ON WINDOWS` are what the `pe-windows-arm64` CI
//! job executes on a Windows on ARM runner.
//!
//! A test prints `SKIPPED:` and passes when a tool it needs is missing,
//! unless `QLD_REQUIRE_COFF_TOOLS` is set.

use std::path::{Path, PathBuf};
use std::process::Command;

use qld::args::{InputAttrs, InputKind, LinkOptions, OutputBuffer, OutputKind};
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

/// A fresh, empty directory for one test.
fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("coff-link")
        .join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A prebuilt fixture from `tests/data/coff_link`.
fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/data/coff_link")
        .join(name)
}

/// Runs an LLVM tool on files in `dir`, or reports it missing.
fn llvm(program: &str, args: &[&str], dir: &Path) -> Option<String> {
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
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The reference linker, when it is installed: lld's GNU-style driver.
fn lld() -> Option<String> {
    ["ld.lld", "ld.lld-23", "ld.lld-22"]
        .into_iter()
        .find(|name| tool(name).is_some())
        .map(str::to_owned)
}

/// GNU ld's `arm64pe` target.
fn arm64_target() -> Target {
    Target {
        arch: Architecture::Aarch64,
        os: OperatingSystem::Windows,
        format: BinaryFormat::Pe,
        endian: Endianness::Little,
        pointer_width: PointerWidth::Bits64,
    }
}

/// An ARM64 COFF object whose only content is `size` bytes of zeros in
/// `.text`: it pushes what follows it out of reach of short branches.
fn padding_object(size: u32) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&0xaa64u16.to_le_bytes()); // Machine
    out.extend_from_slice(&1u16.to_le_bytes()); // NumberOfSections
    out.extend_from_slice(&0u32.to_le_bytes()); // TimeDateStamp
    out.extend_from_slice(&(60 + size).to_le_bytes()); // PointerToSymbolTable
    out.extend_from_slice(&0u32.to_le_bytes()); // NumberOfSymbols
    out.extend_from_slice(&0u16.to_le_bytes()); // SizeOfOptionalHeader
    out.extend_from_slice(&0u16.to_le_bytes()); // Characteristics
    out.extend_from_slice(b".text\0\0\0");
    out.extend_from_slice(&0u32.to_le_bytes()); // VirtualSize
    out.extend_from_slice(&0u32.to_le_bytes()); // VirtualAddress
    out.extend_from_slice(&size.to_le_bytes()); // SizeOfRawData
    out.extend_from_slice(&60u32.to_le_bytes()); // PointerToRawData
    out.extend_from_slice(&[0u8; 12]); // relocations, line numbers
    // CNT_CODE | ALIGN_4BYTES | MEM_EXECUTE | MEM_READ
    out.extend_from_slice(&0x6030_0020u32.to_le_bytes());
    out.resize(out.len() + size as usize, 0);
    out.extend_from_slice(&4u32.to_le_bytes()); // empty string table
    out
}

/// The program's inputs, in link order: the padding sits between the
/// branches and their destinations.
fn program_inputs(dir: &Path) -> Vec<PathBuf> {
    let padding = dir.join("padding.o");
    std::fs::write(&padding, padding_object(0x11_0000)).unwrap();
    vec![
        fixture("arm64-main.o"),
        fixture("arm64-near.o"),
        padding,
        fixture("arm64-far.o"),
        fixture("arm64-tls.o"),
        fixture("libkernel32-arm64.a"),
    ]
}

/// Link options for `inputs` into `output`.
fn options(inputs: &[PathBuf], output: &Path) -> LinkOptions {
    let mut options = LinkOptions::new();
    options.target = Some(arm64_target());
    options.output = Some(output.to_path_buf());
    for path in inputs {
        options.push_input(InputKind::File(path.clone()), InputAttrs::default());
    }
    options
}

/// Links with qld, returning the diagnostics or the error.
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

/// Links the fixture program into `dir/name` with qld.
fn link_program(dir: &Path, name: &str) -> Vec<PathBuf> {
    let inputs = program_inputs(dir);
    let options = options(&inputs, &dir.join(name));
    let pe = PeOptions::from_link_options(&options);
    if let Err(error) = qld_link(&options, &pe) {
        panic!("qld failed to link the ARM64 program:\n{error}");
    }
    inputs
}

/// Links the same inputs with lld into `dir/name`, if lld is installed.
fn lld_link(dir: &Path, inputs: &[PathBuf], name: &str) -> bool {
    let Some(lld) = lld() else {
        skip("ld.lld not found");
        return false;
    };
    let mut args = vec!["-m".to_owned(), "arm64pe".to_owned()];
    args.extend(inputs.iter().map(|path| path.to_str().unwrap().to_owned()));
    args.extend(["-o".to_owned(), name.to_owned()]);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    llvm(&lld, &args, dir).is_some()
}

/// The value of `field` in `llvm-readobj` output.
fn field<'t>(text: &'t str, name: &str) -> Option<&'t str> {
    text.lines().find_map(|line| {
        line.trim()
            .strip_prefix(name)
            .and_then(|rest| rest.strip_prefix(": "))
    })
}

/// `llvm-objdump` lines for a symbol or an address range (`which` are the
/// selecting options), with symbolic labels removed, so that
/// images whose symbol tables differ compare equal.
fn disassembly(dir: &Path, file: &str, which: &[&str]) -> Option<Vec<String>> {
    let mut args = vec!["-d"];
    args.extend_from_slice(which);
    args.push(file);
    let text = llvm("llvm-objdump", &args, dir)?;
    let mut lines: Vec<String> = text
        .lines()
        // Instruction lines: `<address>: <bytes> <mnemonic> ...`.
        .filter(|line| {
            line.trim().split_once(':').is_some_and(|(address, rest)| {
                !address.is_empty()
                    && address.chars().all(|c| c.is_ascii_hexdigit())
                    && !rest.trim().is_empty()
            })
        })
        .map(|line| match line.find(" <") {
            Some(at) => line[..at].trim().to_owned(),
            None => line.trim().to_owned(),
        })
        .collect();
    // A symbol ends at its `ret`: lld names no import thunk, so the
    // disassembly of the last function would run on into them.
    if which
        .iter()
        .any(|option| option.starts_with("--disassemble-symbols"))
        && let Some(at) = lines.iter().position(|line| line.ends_with("\tret"))
    {
        lines.truncate(at + 1);
    }
    Some(lines)
}

/// A freestanding ARM64 program: imports through `adrp`/`ldr` thunks,
/// `ADDR64` data with `DIR64` base relocations, `tbnz` and `cbz` branches
/// through range-extension thunks, TLS through `SECREL_HIGH12A` and
/// `SECREL_LOW12L`, and a `.pdata` table with packed and unpacked entries.
///
/// RUN ON WINDOWS (ARM64): the image should print `hello from qld arm64`,
/// then `42 7 5 9 42`, and exit 0.
#[test]
fn arm64_freestanding_program() {
    let dir = scratch("arm64-program");
    let inputs = link_program(&dir, "qld.exe");

    let Some(headers) = llvm("llvm-readobj", &["--file-headers", "qld.exe"], &dir) else {
        return;
    };
    assert!(headers.contains("IMAGE_FILE_MACHINE_ARM64"), "{headers}");
    assert_eq!(field(&headers, "Magic"), Some("0x20B"), "{headers}");
    assert_eq!(field(&headers, "ImageBase"), Some("0x140000000"));
    assert!(headers.contains("IMAGE_DLL_CHARACTERISTICS_DYNAMIC_BASE"));
    assert!(headers.contains("IMAGE_FILE_LARGE_ADDRESS_AWARE"));
    assert_eq!(field(&headers, "MajorSubsystemVersion"), Some("6"));
    assert_eq!(field(&headers, "TLSTableSize"), Some("0x28"));
    assert_eq!(field(&headers, "ExceptionTableSize"), Some("0x10"));

    let Some(relocs) = llvm("llvm-readobj", &["--coff-basereloc", "qld.exe"], &dir) else {
        return;
    };
    assert!(relocs.contains("DIR64"), "{relocs}");
    assert!(!relocs.contains("HIGHLOW"), "{relocs}");

    // Both conditional branches go through thunks placed right after
    // `near_dispatch`, and the thunks reach the far functions.
    let Some(code) = llvm(
        "llvm-objdump",
        &["-d", "--disassemble-symbols=near_dispatch", "qld.exe"],
        &dir,
    ) else {
        return;
    };
    assert!(
        code.contains("br\tx16"),
        "no thunk after near_dispatch:\n{code}"
    );
    let Some(symbols) = llvm("llvm-nm", &["qld.exe"], &dir) else {
        return;
    };
    let address = |name: &str| -> u64 {
        symbols
            .lines()
            .find_map(|line| {
                let mut parts = line.split_whitespace();
                let value = parts.next()?;
                let _kind = parts.next()?;
                (parts.next()? == name).then(|| u64::from_str_radix(value, 16).ok())?
            })
            .unwrap_or_else(|| panic!("{name} missing:\n{symbols}"))
    };
    for target in ["far_odd", "far_zero"] {
        let low = format!("#0x{:x}", address(target) & 0xfff);
        assert!(
            code.contains(&low),
            "no thunk reaches {target} ({low}):\n{code}"
        );
    }

    // lld, the linker llvm-mingw uses, must lay the code out the same way:
    // the thunks, the branches and the TLS offsets are identical, and the
    // unwind data describes the same functions.
    if !lld_link(&dir, &inputs, "lld.exe") {
        return;
    }
    // `near_dispatch` and the two thunks after it (lld names the thunks,
    // so compare the address range rather than the symbol).
    let start = address("near_dispatch");
    let range = [
        format!("--start-address={start:#x}"),
        format!("--stop-address={:#x}", start + 36),
    ];
    let range: Vec<&str> = range.iter().map(String::as_str).collect();
    assert_eq!(
        disassembly(&dir, "qld.exe", &range),
        disassembly(&dir, "lld.exe", &range),
        "near_dispatch and its thunks differ from lld's"
    );
    assert_eq!(
        disassembly(&dir, "qld.exe", &["--disassemble-symbols=far_function"]),
        disassembly(&dir, "lld.exe", &["--disassemble-symbols=far_function"]),
        "far_function differs from lld's"
    );
    // In `bump_tls`, only the address of `_tls_index` may differ.
    let tls = |file: &str| -> Option<Vec<String>> {
        Some(
            disassembly(&dir, file, &["--disassemble-symbols=bump_tls"])?
                .into_iter()
                .filter(|line| !line.contains("adrp") && !line.contains("ldr\tw8, [x8"))
                .map(|line| line.split('\t').skip(1).collect::<Vec<_>>().join(" "))
                .collect(),
        )
    };
    assert_eq!(
        tls("qld.exe"),
        tls("lld.exe"),
        "TLS access differs from lld's"
    );
    let unwind = |file: &str| -> Option<Vec<String>> {
        Some(
            llvm("llvm-readobj", &["--unwind", file], &dir)?
                .lines()
                .filter(|line| {
                    !line.contains("Function:")
                        && !line.contains("ExceptionRecord:")
                        && !line.starts_with("File:")
                })
                .map(str::to_owned)
                .collect(),
        )
    };
    assert_eq!(unwind("qld.exe"), unwind("lld.exe"), "unwind data differs");
}

/// An ARM64 DLL and its import library, written by qld, used by the same
/// program in place of the far functions.
///
/// RUN ON WINDOWS (ARM64): `client.exe`, next to `far.dll`, should print
/// `hello from qld arm64`, then `42 7 5 9 42`, and exit 0.
#[test]
fn arm64_dll_and_import_library() {
    let dir = scratch("arm64-dll");
    let mut options = options(&[fixture("arm64-far.o")], &dir.join("far.dll"));
    options.kind = OutputKind::Shared;
    let mut pe = PeOptions::from_link_options(&options);
    pe.out_implib = Some(dir.join("libfar.dll.a"));
    if let Err(error) = qld_link(&options, &pe) {
        panic!("qld failed to link the ARM64 DLL:\n{error}");
    }
    let client = [
        fixture("arm64-main.o"),
        fixture("arm64-near.o"),
        fixture("arm64-tls.o"),
        dir.join("libfar.dll.a"),
        fixture("libkernel32-arm64.a"),
    ];
    let options = self::options(&client, &dir.join("client.exe"));
    let pe = PeOptions::from_link_options(&options);
    if let Err(error) = qld_link(&options, &pe) {
        panic!("qld failed to link against its ARM64 import library:\n{error}");
    }

    let Some(exports) = llvm("llvm-readobj", &["--coff-exports", "far.dll"], &dir) else {
        return;
    };
    for name in ["far_odd", "far_zero", "far_even", "far_function"] {
        assert!(exports.contains(name), "{name} is not exported:\n{exports}");
    }
    let Some(headers) = llvm("llvm-readobj", &["--file-headers", "far.dll"], &dir) else {
        return;
    };
    assert_eq!(field(&headers, "ImageBase"), Some("0x180000000"));
    let Some(imports) = llvm("llvm-readobj", &["--coff-imports", "client.exe"], &dir) else {
        return;
    };
    assert!(imports.contains("far.dll"), "{imports}");
    assert!(imports.contains("far_function"), "{imports}");
    // The import thunk is `adrp x16; ldr x16, [x16, ...]; br x16`.
    let Some(thunk) = llvm(
        "llvm-objdump",
        &["-d", "--disassemble-symbols=far_function", "client.exe"],
        &dir,
    ) else {
        return;
    };
    assert!(
        thunk.contains("adrp\tx16") && thunk.contains("ldr\tx16") && thunk.contains("br\tx16"),
        "{thunk}"
    );

    // lld must accept qld's import library and bind the same functions.
    if lld_link(&dir, &client, "lld-client.exe") {
        let Some(theirs) = llvm("llvm-readobj", &["--coff-imports", "lld-client.exe"], &dir) else {
            return;
        };
        let symbols = |text: &str| -> Vec<String> {
            let mut names: Vec<String> = text
                .lines()
                .filter_map(|line| line.trim().strip_prefix("Symbol: "))
                .map(str::to_owned)
                .collect();
            names.sort();
            names
        };
        assert_eq!(symbols(&imports), symbols(&theirs));
    }
}

/// The library API's in-memory inputs and output buffer work for PE links
/// too, and give the same bytes as files do.
#[test]
fn arm64_links_from_and_to_memory() {
    let dir = scratch("arm64-memory");
    let inputs = link_program(&dir, "file.exe");
    let mut options = LinkOptions::new();
    options.target = Some(arm64_target());
    options.output = Some(dir.join("unused.exe"));
    for path in &inputs {
        let name = path.file_name().unwrap().to_str().unwrap().to_owned();
        options.push_input(
            InputKind::bytes(name, std::fs::read(path).unwrap()),
            InputAttrs::default(),
        );
    }
    let buffer = OutputBuffer::new();
    options.output_buffer = Some(buffer.clone());
    let pe = PeOptions::from_link_options(&options);
    if let Err(error) = qld_link(&options, &pe) {
        panic!("qld failed to link from memory:\n{error}");
    }
    let image = buffer.take().expect("the link fills the buffer");
    assert!(
        !dir.join("unused.exe").exists(),
        "the output file was written"
    );
    assert_eq!(
        image,
        std::fs::read(dir.join("file.exe")).unwrap(),
        "linking from memory changed the image"
    );
}

/// ARM64EC and ARM64X hybrids are refused rather than linked as ARM64.
#[test]
fn arm64ec_is_refused() {
    let dir = scratch("arm64ec");
    let mut object = padding_object(4);
    object[0..2].copy_from_slice(&0xa641u16.to_le_bytes());
    let path = dir.join("ec.o");
    std::fs::write(&path, object).unwrap();
    let options = options(&[path], &dir.join("ec.exe"));
    let error = qld_link(&options, &PeOptions::from_link_options(&options)).unwrap_err();
    assert!(error.contains("arm64ec"), "{error}");
}
