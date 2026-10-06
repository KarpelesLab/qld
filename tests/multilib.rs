//! Incompatible libraries in the search path (workstream W56).
//!
//! GNU `ld` does not stop when `-L` points at a directory of the wrong
//! architecture: it says `skipping incompatible <path> when searching for
//! -l<name>`, goes on to the next candidate, and only reports `cannot find
//! -l<name>` when every candidate was incompatible. These tests build a
//! 32-bit and a 64-bit copy of the same library with `gcc -m32`/`gcc`, run
//! both linkers over the same command lines, and compare the list of
//! skipped candidates and the exit status.
//!
//! A file named directly on the command line is still an error (for an
//! archive, once a member is extracted), and its message must be the
//! architecture one with BFD's names, not a "malformed ELF class" parse
//! error: a 32-bit ELF file is well formed, just for another machine.
//!
//! Tools: `gcc` with `-m32` (Debian/Ubuntu `gcc-multilib`), `ar` and GNU
//! `ld` with the `elf_i386` and `elf_x86_64` emulations. A test prints
//! `SKIPPED:` and passes when one is missing, unless
//! `QLD_REQUIRE_I386_TOOLS=1`.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;

fn in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

fn required(var: &str) -> bool {
    std::env::var_os(var).is_some_and(|v| !v.is_empty() && v != "0")
}

fn run(dir: &Path, program: &Path, args: &[&str]) -> Output {
    Command::new(program)
        .args(args)
        .current_dir(dir)
        .env("LC_ALL", "C")
        .output()
        .unwrap_or_else(|error| panic!("cannot run {}: {error}", program.display()))
}

/// The tools, and the fixture tree every test shares.
struct Fixtures {
    ld: PathBuf,
    /// Root of the tree: `m.o`/`u.o` (64-bit) and `m32.o`/`u32.o` (32-bit)
    /// objects, and the library directories below.
    dir: PathBuf,
}

/// Builds the fixture tree once:
///
/// - `lib32/libpick.so`, `lib32/libpick.a`: 32-bit, defining `helper`;
/// - `lib64/libpick.a`: 64-bit, defining `helper`;
/// - `u.o` (64-bit) and `u32.o` (32-bit) call `helper`.
fn fixtures() -> Result<&'static Fixtures, String> {
    static ONCE: OnceLock<Result<Fixtures, String>> = OnceLock::new();
    ONCE.get_or_init(build).as_ref().map_err(String::clone)
}

fn build() -> Result<Fixtures, String> {
    if !cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        return Err("host is not x86-64 Linux".into());
    }
    let cc = in_path("gcc").ok_or("no gcc")?;
    let ar = in_path("ar").ok_or("no ar")?;
    let ld = in_path("ld").ok_or("no ld")?;

    let dir = common::scratch::scratch_dir("multilib", "fixtures");
    for sub in ["lib32", "lib64"] {
        fs::create_dir_all(dir.join(sub)).map_err(|e| e.to_string())?;
    }
    fs::write(dir.join("helper.c"), b"int helper(void) { return 7; }\n")
        .map_err(|e| e.to_string())?;
    fs::write(
        dir.join("main.c"),
        b"extern int helper(void); int main(void) { return helper() - 7; }\n",
    )
    .map_err(|e| e.to_string())?;

    let compile = |args: &[&str]| -> Result<(), String> {
        let output = run(&dir, &cc, args);
        if output.status.success() {
            Ok(())
        } else {
            Err(format!(
                "gcc {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr)
            ))
        }
    };
    compile(&["-c", "helper.c", "-o", "helper64.o"])?;
    compile(&["-c", "main.c", "-o", "u.o"])?;
    // The 32-bit half needs gcc-multilib; without it the whole file skips.
    compile(&["-m32", "-c", "helper.c", "-fPIC", "-o", "helper32.o"])?;
    compile(&["-m32", "-c", "main.c", "-o", "u32.o"])?;
    compile(&["-m32", "-shared", "-o", "lib32/libpick.so", "helper32.o"])?;

    for (archive, object) in [
        ("lib32/libpick.a", "helper32.o"),
        ("lib64/libpick.a", "helper64.o"),
    ] {
        let output = run(&dir, &ar, &["rcs", archive, object]);
        if !output.status.success() {
            return Err(format!(
                "ar rcs {archive} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }
    }
    Ok(Fixtures { ld, dir })
}

macro_rules! fixtures {
    () => {
        match fixtures() {
            Ok(fixtures) => fixtures,
            Err(why) => {
                assert!(
                    !required("QLD_REQUIRE_I386_TOOLS"),
                    "QLD_REQUIRE_I386_TOOLS is set but the fixtures could not be built: {why}"
                );
                println!("SKIPPED: {why}");
                return;
            }
        }
    };
}

impl Fixtures {
    fn qld(&self, args: &[&str]) -> Output {
        run(&self.dir, Path::new(env!("CARGO_BIN_EXE_qld")), args)
    }

    fn gnu(&self, args: &[&str]) -> Output {
        run(&self.dir, &self.ld, args)
    }
}

/// The `skipping incompatible …` lines of `output`, without the program
/// name and severity that each linker puts in front of them, in order.
///
/// GNU `ld` repeats the whole search once more after `cannot find`, to
/// report why each candidate failed; those repeats are dropped so that the
/// two linkers can be compared on what they skipped.
fn skipped(output: &Output) -> Vec<String> {
    let text = String::from_utf8_lossy(&output.stderr).into_owned();
    let mut lines = Vec::new();
    for line in text.lines() {
        let Some(at) = line.find("skipping incompatible ") else {
            continue;
        };
        if line.contains("cannot find") {
            break;
        }
        let message = line.get(at..).unwrap_or_default().to_string();
        if lines.contains(&message) {
            continue;
        }
        lines.push(message);
    }
    lines
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Every candidate is incompatible: both linkers skip both of them and then
/// report the library as not found.
#[test]
fn every_candidate_incompatible() {
    let fixtures = fixtures!();
    let args = [
        "-m",
        "elf_x86_64",
        "-e",
        "main",
        "-o",
        "out",
        "u.o",
        "-Llib32",
        "-lpick",
    ];
    let ours = fixtures.qld(&args);
    let theirs = fixtures.gnu(&args);
    assert_eq!(
        skipped(&ours),
        vec![
            "skipping incompatible lib32/libpick.so when searching for -lpick".to_string(),
            "skipping incompatible lib32/libpick.a when searching for -lpick".to_string(),
        ],
        "qld said: {}",
        stderr(&ours)
    );
    assert_eq!(skipped(&ours), skipped(&theirs));
    assert!(!ours.status.success(), "qld should have failed");
    assert!(!theirs.status.success(), "GNU ld should have failed");
    assert!(
        stderr(&ours).contains("cannot find -lpick"),
        "qld said: {}",
        stderr(&ours)
    );
}

/// The search goes on past an incompatible directory and links the library
/// it finds in the next one.
#[test]
fn search_continues_into_the_next_directory() {
    let fixtures = fixtures!();
    let args = [
        "-m",
        "elf_x86_64",
        "-e",
        "main",
        "-o",
        "out",
        "u.o",
        "-Llib32",
        "-Llib64",
        "-lpick",
    ];
    let ours = fixtures.qld(&args);
    let theirs = fixtures.gnu(&args);
    assert!(ours.status.success(), "qld said: {}", stderr(&ours));
    assert!(theirs.status.success(), "GNU ld said: {}", stderr(&theirs));
    assert_eq!(skipped(&ours), skipped(&theirs));
    assert_eq!(skipped(&ours).len(), 2);
}

/// `-Bstatic` leaves only `libpick.a` a candidate, so only it is skipped.
#[test]
fn static_only_skips_only_the_archive() {
    let fixtures = fixtures!();
    let args = [
        "-m",
        "elf_x86_64",
        "-e",
        "main",
        "-o",
        "out",
        "u.o",
        "-Bstatic",
        "-Llib32",
        "-Llib64",
        "-lpick",
    ];
    let ours = fixtures.qld(&args);
    let theirs = fixtures.gnu(&args);
    assert!(ours.status.success(), "qld said: {}", stderr(&ours));
    assert_eq!(
        skipped(&ours),
        vec!["skipping incompatible lib32/libpick.a when searching for -lpick".to_string()]
    );
    assert_eq!(skipped(&ours), skipped(&theirs));
}

/// `-l:name` searches for that exact file, and skips it the same way.
#[test]
fn exact_library_names_are_skipped_too() {
    let fixtures = fixtures!();
    let args = [
        "-m",
        "elf_x86_64",
        "-e",
        "main",
        "-o",
        "out",
        "u.o",
        "-Llib32",
        "-Llib64",
        "-l:libpick.a",
    ];
    let ours = fixtures.qld(&args);
    let theirs = fixtures.gnu(&args);
    assert!(ours.status.success(), "qld said: {}", stderr(&ours));
    assert_eq!(
        skipped(&ours),
        vec!["skipping incompatible lib32/libpick.a when searching for -l:libpick.a".to_string()]
    );
    assert_eq!(skipped(&ours), skipped(&theirs));
}

/// A `GROUP ( -lpick )` in a linker script searches the same path.
#[test]
fn script_group_skips_incompatible_libraries() {
    let fixtures = fixtures!();
    fs::write(fixtures.dir.join("group.ld"), b"GROUP ( -lpick )\n").unwrap();
    let args = [
        "-m",
        "elf_x86_64",
        "-e",
        "main",
        "-o",
        "out",
        "u.o",
        "-Llib32",
        "-Llib64",
        "group.ld",
    ];
    let ours = fixtures.qld(&args);
    let theirs = fixtures.gnu(&args);
    assert!(ours.status.success(), "qld said: {}", stderr(&ours));
    assert_eq!(skipped(&ours), skipped(&theirs));
    assert_eq!(skipped(&ours).len(), 2);
}

/// The other direction: a 64-bit library in a 32-bit link.
#[test]
fn a_32_bit_link_skips_a_64_bit_library() {
    let fixtures = fixtures!();
    let args = [
        "-m", "elf_i386", "-e", "main", "-o", "out32", "u32.o", "-Llib64", "-Llib32", "-lpick",
    ];
    let ours = fixtures.qld(&args);
    let theirs = fixtures.gnu(&args);
    assert!(ours.status.success(), "qld said: {}", stderr(&ours));
    assert_eq!(
        skipped(&ours),
        vec!["skipping incompatible lib64/libpick.a when searching for -lpick".to_string()]
    );
    assert_eq!(skipped(&ours), skipped(&theirs));
}

/// A file named on the command line is an error, not a skipped candidate,
/// and the message names the architecture rather than the ELF class field:
/// a 32-bit file is well formed, just built for another machine.
#[test]
fn a_named_file_is_an_error_about_the_architecture() {
    let fixtures = fixtures!();
    for (name, culprit) in [
        ("lib32/libpick.so", "lib32/libpick.so"),
        ("lib32/libpick.a", "lib32/libpick.a(helper32.o)"),
    ] {
        let ours = fixtures.qld(&["-m", "elf_x86_64", "-e", "main", "-o", "out", "u.o", name]);
        let text = stderr(&ours);
        assert!(!ours.status.success(), "qld linked {name}: {text}");
        // BFD's names, and for the archive the member that was extracted,
        // as GNU ld says it (GNU ld words the shared object differently:
        // `file in wrong format`).
        let expected = format!(
            "i386 architecture of input file `{culprit}' is incompatible with i386:x86-64 output"
        );
        assert!(text.contains(&expected), "qld said, for {name}: {text}");
        if name.ends_with(".a") {
            let theirs =
                fixtures.gnu(&["-m", "elf_x86_64", "-e", "main", "-o", "out", "u.o", name]);
            assert!(
                stderr(&theirs).contains(&expected),
                "GNU ld said: {}",
                stderr(&theirs)
            );
        }
        assert!(
            !text.contains("malformed"),
            "qld called {name} malformed: {text}"
        );
        assert!(
            skipped(&ours).is_empty(),
            "qld skipped a named file: {text}"
        );
    }
}

/// A named incompatible archive that nothing extracts from is accepted, as
/// GNU ld only looks at the members it pulls in. `u.o` takes `helper` from
/// the 64-bit archive first, so the 32-bit one is never used.
#[test]
fn a_named_archive_is_only_checked_when_extracted() {
    let fixtures = fixtures!();
    let args = [
        "-m",
        "elf_x86_64",
        "-e",
        "main",
        "-o",
        "out",
        "u.o",
        "lib64/libpick.a",
        "lib32/libpick.a",
    ];
    let ours = fixtures.qld(&args);
    let theirs = fixtures.gnu(&args);
    assert!(theirs.status.success(), "GNU ld: {}", stderr(&theirs));
    assert!(ours.status.success(), "qld: {}", stderr(&ours));
    assert!(!stderr(&ours).contains("incompatible"), "{}", stderr(&ours));

    // With `--whole-archive` every member is extracted.
    let args = [
        "-m",
        "elf_x86_64",
        "-e",
        "main",
        "-o",
        "out",
        "u.o",
        "lib64/libpick.a",
        "--whole-archive",
        "lib32/libpick.a",
    ];
    let ours = fixtures.qld(&args);
    let theirs = fixtures.gnu(&args);
    let message = "i386 architecture of input file `lib32/libpick.a(helper32.o)' is incompatible";
    assert!(
        stderr(&theirs).contains(message),
        "GNU ld: {}",
        stderr(&theirs)
    );
    assert!(!ours.status.success(), "qld linked it");
    assert!(stderr(&ours).contains(message), "qld: {}", stderr(&ours));
}

/// A thin archive found by search is skipped by its first member's
/// architecture like a regular one, and one named directly reports its
/// member by the member's own path.
#[test]
fn thin_archives_are_checked_like_regular_ones() {
    let fixtures = fixtures!();
    let thin = fixtures.dir.join("thin32");
    fs::create_dir_all(&thin).unwrap();
    let _ = fs::remove_file(thin.join("libpick.a"));
    let ar = in_path("ar").expect("ar was found for the fixtures");
    let made = run(&thin, &ar, &["rcsT", "libpick.a", "../helper32.o"]);
    assert!(made.status.success(), "{}", stderr(&made));

    let args = [
        "-m",
        "elf_x86_64",
        "-e",
        "main",
        "-o",
        "out",
        "u.o",
        "-Lthin32",
        "-Llib64",
        "-lpick",
    ];
    let ours = fixtures.qld(&args);
    let theirs = fixtures.gnu(&args);
    assert!(ours.status.success(), "qld: {}", stderr(&ours));
    assert_eq!(
        skipped(&ours),
        vec!["skipping incompatible thin32/libpick.a when searching for -lpick".to_string()],
        "qld said: {}",
        stderr(&ours)
    );
    assert_eq!(skipped(&ours), skipped(&theirs));

    let args = [
        "-m",
        "elf_x86_64",
        "-e",
        "main",
        "-o",
        "out",
        "u.o",
        "thin32/libpick.a",
    ];
    let ours = fixtures.qld(&args);
    let theirs = fixtures.gnu(&args);
    let message = "i386 architecture of input file `thin32/../helper32.o' is incompatible \
                   with i386:x86-64 output";
    assert!(
        stderr(&theirs).contains(message),
        "GNU ld: {}",
        stderr(&theirs)
    );
    assert!(!ours.status.success(), "qld linked it");
    assert!(stderr(&ours).contains(message), "qld: {}", stderr(&ours));
}
