//! The target of an ELF link without `-m` (workstream W40): the first input
//! that names one sets it, GCC LTO objects and LLVM bitcode included, and an
//! object for another machine is then rejected, as GNU ld rejects it.
//!
//! Tools: an AArch64 `gcc` with its LTO plugin (the host's on an AArch64
//! host, else `aarch64-linux-gnu-gcc` or `aarch64-unknown-linux-gnu-gcc`),
//! and `clang` with `LLVMgold.so`. A test prints `SKIPPED:` and passes when
//! they are missing, unless `QLD_REQUIRE_TOOLS=1`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// Skips the test (returning `None`) when a tool is missing, or fails it
/// when tools are required.
fn need<T>(tool: Option<T>, what: &str) -> Option<T> {
    if tool.is_none() {
        let required =
            std::env::var_os("QLD_REQUIRE_TOOLS").is_some_and(|v| !v.is_empty() && v != "0");
        assert!(!required, "QLD_REQUIRE_TOOLS is set but {what} is missing");
        println!("SKIPPED: no {what}");
    }
    tool
}

fn run(dir: &Path, program: &Path, args: &[&str]) -> Output {
    Command::new(program)
        .args(args)
        .current_dir(dir)
        .env("LC_ALL", "C")
        .output()
        .unwrap_or_else(|e| panic!("cannot run {}: {e}", program.display()))
}

fn run_ok(dir: &Path, program: &Path, args: &[&str]) -> String {
    let output = run(dir, program, args);
    assert!(
        output.status.success(),
        "`{} {}` failed:\n{}",
        program.display(),
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn qld(dir: &Path, args: &[&str]) -> Output {
    run(dir, Path::new(env!("CARGO_BIN_EXE_qld")), args)
}

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("elf-target")
        .join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("f.c"),
        "int g(void);\nint f(void) { return g() + 1; }\n",
    )
    .unwrap();
    fs::write(dir.join("g.c"), "int g(void) { return 2; }\n").unwrap();
    dir
}

/// A relocatable ELF64 x86-64 object with no sections: enough to be
/// identified, and rejected, before it is parsed.
fn x86_64_object(dir: &Path) -> &'static str {
    let mut header = vec![0u8; 64];
    header[..4].copy_from_slice(b"\x7fELF");
    header[4] = 2; // ELFCLASS64
    header[5] = 1; // ELFDATA2LSB
    header[6] = 1; // EV_CURRENT
    header[16] = 1; // ET_REL
    header[18] = 62; // EM_X86_64
    header[20] = 1; // e_version
    header[52] = 64; // e_ehsize
    header[58] = 64; // e_shentsize
    fs::write(dir.join("x86_64.o"), header).unwrap();
    "x86_64.o"
}

/// `e_ident[EI_CLASS]` and `e_machine` of an ELF file.
fn class_and_machine(path: &Path) -> (u8, u16) {
    let data = fs::read(path).unwrap();
    (data[4], u16::from_le_bytes([data[18], data[19]]))
}

fn assert_incompatible(output: &Output, with: &str) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success()
            && stderr.contains("x86_64.o: X86_64")
            && stderr.contains(&format!("incompatible with {with} ")),
        "{stderr}"
    );
}

fn aarch64_gcc() -> Option<PathBuf> {
    // The host's own `gcc` only on an AArch64 Linux host: on macOS it is
    // Apple clang, which drives ld64.
    if cfg!(target_arch = "aarch64")
        && let Some(gcc) = in_path("gcc")
        && Command::new(&gcc)
            .arg("-dumpmachine")
            .output()
            .is_ok_and(|out| String::from_utf8_lossy(&out.stdout).contains("linux"))
    {
        return Some(gcc);
    }
    in_path("aarch64-linux-gnu-gcc").or_else(|| in_path("aarch64-unknown-linux-gnu-gcc"))
}

/// A GCC LTO object is an ELF file with a real `e_machine`: listed first,
/// it sets the target, and the x86-64 object after it is rejected.
#[test]
fn gcc_lto_object_names_the_target() {
    let Some(cc) = need(aarch64_gcc(), "AArch64 gcc") else {
        return;
    };
    let dir = scratch("gcc");
    let plugin = run_ok(&dir, &cc, &["-print-prog-name=liblto_plugin.so"]);
    let Some(plugin) = need(
        Some(PathBuf::from(plugin.trim())).filter(|p| p.is_absolute() && p.is_file()),
        "liblto_plugin.so",
    ) else {
        return;
    };
    run_ok(&dir, &cc, &["-O2", "-flto", "-c", "f.c"]);
    let foreign = x86_64_object(&dir);
    let plugin = plugin.to_str().unwrap();
    // Before the IR is compiled: the plugin runs only inside gcc's driver.
    let output = qld(
        &dir,
        &["-plugin", plugin, "-e", "f", "f.o", foreign, "-o", "out"],
    );
    assert_incompatible(&output, "Aarch64");
}

/// Makes `shim/ld` run qld, for `gcc -B shim/`.
#[cfg(unix)]
fn link_shim(dir: &Path) -> Option<PathBuf> {
    let shim = dir.join("shim");
    fs::create_dir_all(&shim).ok()?;
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_qld"), shim.join("ld")).ok()?;
    Some(shim)
}

#[cfg(not(unix))]
fn link_shim(_: &Path) -> Option<PathBuf> {
    None
}

/// The regression the `aarch64-lto-native-archive` fixture runs, without
/// running the program: the libraries GCC's plugin adds after LTO
/// (`libgcc.a`) must be checked against the link's target, not x86-64.
#[test]
fn gcc_lto_link_adds_native_libraries_for_the_target() {
    let Some(cc) = need(aarch64_gcc(), "AArch64 gcc") else {
        return;
    };
    let dir = scratch("gcc-link");
    let Some(shim) = need(link_shim(&dir), "Unix host") else {
        return;
    };
    fs::write(
        dir.join("main.c"),
        "int f(void);\nint main(void) { return f() - 3; }\n",
    )
    .unwrap();
    // The C library for the target, or skip.
    let probe = run(&dir, &cc, &["-O2", "main.c", "f.c", "g.c", "-o", "probe"]);
    if need(probe.status.success().then_some(()), "AArch64 C library").is_none() {
        return;
    }
    run_ok(&dir, &cc, &["-O2", "-flto", "-c", "main.c", "f.c"]);
    run_ok(&dir, &cc, &["-O2", "-c", "g.c"]);
    let shim = format!("-B{}/", shim.display());
    let args = [
        shim.as_str(),
        "-O2",
        "-flto",
        "main.o",
        "f.o",
        "g.o",
        "-o",
        "out",
    ];
    run_ok(&dir, &cc, &args);
    assert_eq!(
        class_and_machine(&dir.join("out")),
        (2, 183),
        "ELFCLASS64, EM_AARCH64"
    );
}

/// `LLVMgold.so` of `clang`'s LLVM: in `lib/` or `lib64/` next to the
/// `bin/` that holds the real `clang`.
fn llvm_gold(clang: &Path) -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("QLD_TEST_LLVMGOLD") {
        return Some(PathBuf::from(path));
    }
    let root = fs::canonicalize(clang)
        .ok()?
        .parent()?
        .parent()?
        .to_path_buf();
    ["lib", "lib64"]
        .iter()
        .map(|lib| root.join(lib).join("LLVMgold.so"))
        .find(|gold| gold.is_file())
}

/// LLVM bitcode names its target in the triple of its symbol table: i386
/// bitcode alone makes an ELF32 i386 executable, and AArch64 bitcode first
/// rejects the x86-64 object after it.
#[test]
fn bitcode_names_the_target() {
    let Some(clang) = need(in_path("clang"), "clang") else {
        return;
    };
    let Some(gold) = need(llvm_gold(&clang), "LLVMgold.so") else {
        return;
    };
    let dir = scratch("bitcode");
    let gold = gold.to_str().unwrap();
    for (triple, source, bitcode) in [
        ("i686-linux-gnu", "g.c", "i386.bc"),
        ("aarch64-linux-gnu", "f.c", "aarch64.bc"),
    ] {
        run_ok(
            &dir,
            &clang,
            &[
                &format!("--target={triple}"),
                "-O2",
                "-flto",
                "-c",
                source,
                "-o",
                bitcode,
            ],
        );
    }
    // No input but the bitcode names the ELF class.
    let output = qld(&dir, &["-plugin", gold, "-e", "g", "i386.bc", "-o", "out"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        class_and_machine(&dir.join("out")),
        (1, 3),
        "ELFCLASS32, EM_386"
    );
    let foreign = x86_64_object(&dir);
    let output = qld(
        &dir,
        &[
            "-plugin",
            gold,
            "-e",
            "f",
            "aarch64.bc",
            foreign,
            "-o",
            "out2",
        ],
    );
    assert_incompatible(&output, "Aarch64");
}

/// `-b binary` inputs are machine-neutral: they link into any target, in
/// its class and byte order, and the output's machine is the target's
/// (`-m`), not x86-64. Needs no tools.
#[test]
fn binary_inputs_take_the_target() {
    let dir = scratch("binary");
    fs::write(dir.join("blob.bin"), b"hello").unwrap();
    for (emulation, machine) in [("aarch64linux", 183), ("elf_x86_64", 62)] {
        let args = [
            "-m", emulation, "-r", "-b", "binary", "blob.bin", "-o", "blob.o",
        ];
        let output = qld(&dir, &args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(class_and_machine(&dir.join("blob.o")), (2, machine));
    }
    // A 32-bit or big-endian target gets an object of its own format
    // (no `-r`: relocatable output is x86-64 only so far).
    for (emulation, class, machine, data) in
        [("elf_i386", 1u8, 3u16, 1u8), ("elf64_s390", 2, 22, 2)]
    {
        let args = ["-m", emulation, "-b", "binary", "blob.bin", "-o", "out"];
        let output = qld(&dir, &args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let bytes = fs::read(dir.join("out")).unwrap();
        assert_eq!((bytes[4], bytes[5]), (class, data), "{emulation}");
        let e_machine = if data == 2 {
            u16::from_be_bytes([bytes[18], bytes[19]])
        } else {
            u16::from_le_bytes([bytes[18], bytes[19]])
        };
        assert_eq!(e_machine, machine, "{emulation}");
    }
}
