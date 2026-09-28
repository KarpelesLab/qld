//! Motorola 68000 ELF tests (workstream W55).
//!
//! The m68k backend's relocations are checked end to end by `tests/hunk.rs`,
//! which compares qld's AmigaOS load files with vlink's byte for byte: every
//! relocated longword and every PC-relative displacement in those fixtures
//! has to hold what vlink computed. What is left for this suite is the ELF
//! side of the same backend, which needs no external tool:
//!
//! - a static m68k executable is an ELF32 big-endian `EM_68K` `ET_EXEC`
//!   whose entry is `_start`, with the sections in address order;
//! - `-r` combines the objects into one m68k relocatable, and linking that
//!   gives the same load file as linking the objects directly, which is a
//!   strong check on the rewritten relocations;
//! - dynamic output is refused with a diagnostic that says so, rather than
//!   written without a PLT.
//!
//! The inputs are the committed objects of the Hunk corpus
//! (`tests/data/hunk/`), which vasm produced; see `tests/hunk.rs` for how
//! they are regenerated.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const DATA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/hunk");
const SCRATCH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/target/tmp/qld-tests/m68k");

fn scratch(what: &str) -> PathBuf {
    let dir = Path::new(SCRATCH).join(what);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("cannot create {}: {e}", dir.display()));
    dir
}

fn qld(args: &[&Path]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_qld"))
        .args(args)
        .env("LC_ALL", "C")
        .output()
        .unwrap_or_else(|e| panic!("cannot run qld: {e}"))
}

fn qld_ok(args: &[&Path]) {
    let output = qld(args);
    assert!(
        output.status.success(),
        "qld failed:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The two objects of the `basic` case, in link order.
fn basic() -> (PathBuf, PathBuf) {
    let dir = Path::new(DATA).join("basic");
    (dir.join("main.o"), dir.join("helper.o"))
}

fn be16(bytes: &[u8], at: usize) -> u16 {
    let slice = bytes.get(at..at + 2).expect("in range");
    u16::from_be_bytes([slice[0], slice[1]])
}

fn be32(bytes: &[u8], at: usize) -> u32 {
    let slice = bytes.get(at..at + 4).expect("in range");
    u32::from_be_bytes([slice[0], slice[1], slice[2], slice[3]])
}

#[test]
fn static_executable_is_elf32_big_endian_m68k() {
    let dir = scratch("static");
    let out = dir.join("prog.elf");
    let (main, helper) = basic();
    qld_ok(&[
        Path::new("-static"),
        Path::new("--threads=2"),
        Path::new("-o"),
        &out,
        &main,
        &helper,
    ]);
    let image = fs::read(&out).expect("output");
    assert_eq!(image.get(..4), Some(&b"\x7fELF"[..]), "not an ELF file");
    assert_eq!(image.get(4), Some(&1), "not ELFCLASS32");
    assert_eq!(image.get(5), Some(&2), "not ELFDATA2MSB");
    assert_eq!(be16(&image, 16), 2, "not ET_EXEC");
    assert_eq!(be16(&image, 18), 4, "e_machine is not EM_68K");
    let entry = be32(&image, 24);
    assert!(entry != 0, "no entry point");
    // The entry is the start of `.text`, which is where `_start` is: the
    // first program header's virtual address plus the header space.
    let phoff = be32(&image, 28) as usize;
    let phentsize = be16(&image, 42) as usize;
    let phnum = be16(&image, 44) as usize;
    assert!(phnum >= 2, "expected loadable segments, got {phnum}");
    let mut previous = 0u32;
    let mut entry_is_mapped = false;
    for index in 0..phnum {
        let at = phoff + index * phentsize;
        // PT_LOAD.
        if be32(&image, at) != 1 {
            continue;
        }
        let vaddr = be32(&image, at + 8);
        let memsz = be32(&image, at + 20);
        assert!(
            vaddr >= previous,
            "PT_LOAD segments are not in address order"
        );
        previous = vaddr;
        entry_is_mapped |= entry >= vaddr && entry - vaddr < memsz;
    }
    assert!(
        entry_is_mapped,
        "the entry point 0x{entry:x} is in no PT_LOAD"
    );
}

#[test]
fn relocatable_output_relinks_to_the_same_load_file() {
    let dir = scratch("relocatable");
    let combined = dir.join("combined.o");
    let (main, helper) = basic();
    qld_ok(&[
        Path::new("-r"),
        Path::new("--threads=2"),
        Path::new("-o"),
        &combined,
        &main,
        &helper,
    ]);
    let image = fs::read(&combined).expect("combined object");
    assert_eq!(be16(&image, 16), 1, "-r output is not ET_REL");
    assert_eq!(be16(&image, 18), 4, "-r output is not EM_68K");
    let out = dir.join("out.hunk");
    qld_ok(&[
        Path::new("--oformat"),
        Path::new("amigahunk"),
        Path::new("--threads=2"),
        Path::new("-o"),
        &out,
        &combined,
    ]);
    let direct = fs::read(Path::new(DATA).join("basic").join("expected.hunk")).expect("reference");
    let through_r = fs::read(&out).expect("load file");
    assert_eq!(
        through_r, direct,
        "linking the -r output gives a different load file than linking the objects"
    );
}

#[test]
fn dynamic_output_is_refused() {
    let dir = scratch("dynamic");
    let out = dir.join("libx.so");
    let (main, helper) = basic();
    let output = qld(&[
        Path::new("-shared"),
        Path::new("--threads=2"),
        Path::new("-o"),
        &out,
        &main,
        &helper,
    ]);
    assert!(!output.status.success(), "qld wrote a dynamic m68k output");
    let message = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        message.contains("m68k") && message.contains("not implemented"),
        "unexpected diagnostic:\n{message}"
    );
}
