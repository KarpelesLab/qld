//! Integration tests for `qld::hints`, on shared objects and archives built
//! with the host's `cc` and `ar` at test time. Tests skip, with a message,
//! when the tools are missing.
//!
//! `system_library_index` (ignored) builds the index over the host's
//! `/usr/lib64` (or `/usr/lib`) and reports how long that took.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

use qld::hints::{
    Hint, Hinter, LibraryIndex, LibraryKind, LinkedLibrary, NearMissKind, SearchScope, Undefined,
    render,
};

/// A scratch directory, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("qld-hints-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.0.join(relative)
    }

    fn write(&self, relative: &str, text: &str) -> PathBuf {
        let path = self.path(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn have(tool: &str) -> bool {
    Command::new(tool)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn run(command: &mut Command) {
    let output = command.output().expect("spawn");
    assert!(
        output.status.success(),
        "{command:?} failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Compiles `source` into a shared object at `output`.
fn shared(scratch: &Scratch, source: &str, output: &str, extra: &[&str]) -> PathBuf {
    let c = scratch.write(&format!("src/{}.c", output.replace('/', "_")), source);
    let out = scratch.path(output);
    std::fs::create_dir_all(out.parent().unwrap()).unwrap();
    run(Command::new("cc")
        .args(["-shared", "-fPIC", "-o"])
        .arg(&out)
        .arg(&c)
        .args(extra));
    out
}

/// Compiles each `(member name, source)` and archives them at `output`.
fn archive(scratch: &Scratch, members: &[(&str, &str)], output: &str) -> PathBuf {
    let out = scratch.path(output);
    std::fs::create_dir_all(out.parent().unwrap()).unwrap();
    let mut objects = Vec::new();
    for (name, source) in members {
        let c = scratch.write(&format!("src/{name}.c"), source);
        let o = scratch.path(&format!("src/{name}.o"));
        run(Command::new("cc")
            .args(["-c", "-fPIC", "-o"])
            .arg(&o)
            .arg(&c));
        objects.push(o);
    }
    run(Command::new("ar").arg("rcs").arg(&out).args(&objects));
    out
}

fn tools() -> bool {
    // The fixtures are ELF shared objects and archives: `cc` must produce ELF,
    // which it doesn't on Windows (MinGW) or macOS (Mach-O).
    if !cfg!(target_os = "linux") {
        eprintln!("skipping: the host compiler does not produce ELF");
        return false;
    }
    if have("cc") && have("ar") {
        true
    } else {
        eprintln!("skipping: cc or ar not installed");
        false
    }
}

/// The fixture tree shared by most tests.
struct Fixture {
    scratch: Scratch,
    scope: SearchScope,
}

fn fixture(name: &str) -> Fixture {
    let scratch = Scratch::new(name);
    let d1 = "dir1";
    let d2 = "dir2";
    shared(
        &scratch,
        "int foo_func(void) { return 1; }",
        &format!("{d1}/libfoo.so"),
        &["-Wl,-soname,libfoo.so.1"],
    );
    archive(
        &scratch,
        &[("bar", "int bar_func(void) { return 2; }")],
        &format!("{d2}/libbar.a"),
    );
    shared(
        &scratch,
        "int dup_func(void) { return 3; }",
        &format!("{d1}/libdup.so"),
        &[],
    );
    archive(
        &scratch,
        &[("dup", "int dup_func(void) { return 3; }")],
        &format!("{d1}/libdup.a"),
    );
    shared(
        &scratch,
        "int grp_func(void) { return 4; }",
        &format!("{d1}/libgrp_impl.so.2"),
        &["-Wl,-soname,libgrp_impl.so.2"],
    );
    scratch.write(
        &format!("{d1}/libgrp.so"),
        "/* script */\nGROUP ( libgrp_impl.so.2 )\n",
    );
    let map = scratch.write(
        "src/ver.map",
        "VER_1 { global: vfunc; };\nVER_2 { global: vfunc; } VER_1;\n",
    );
    shared(
        &scratch,
        "int vfunc_old(void) { return 1; }\n\
         int vfunc_new(void) { return 2; }\n\
         __asm__(\".symver vfunc_old,vfunc@VER_1\");\n\
         __asm__(\".symver vfunc_new,vfunc@@VER_2\");\n",
        &format!("{d1}/libver.so"),
        &[&format!("-Wl,--version-script={}", map.display())],
    );
    shared(
        &scratch,
        "int weird_func(void) { return 5; }",
        &format!("{d2}/weird.so"),
        &[],
    );
    archive(
        &scratch,
        &[("other", "int other_func(void) { return 6; }")],
        &format!("{d1}/libshadow.a"),
    );
    shared(
        &scratch,
        "int shadow_func(void) { return 7; }",
        &format!("{d2}/libshadow.so"),
        &[],
    );
    shared(
        &scratch,
        "int dyn_func(void) { return 8; }",
        &format!("{d1}/libdyn.so"),
        &[],
    );
    archive(
        &scratch,
        &[("nodyn", "int not_dyn_func(void) { return 9; }")],
        &format!("{d1}/libdyn.a"),
    );
    let scope = SearchScope {
        search_paths: vec![scratch.path(d1), scratch.path(d2)],
        sysroot: None,
    };
    Fixture { scratch, scope }
}

fn library_of(hint: &Hint) -> (&str, &str, LibraryKind) {
    match hint {
        Hint::MissingLibrary { library, .. }
        | Hint::DroppedAsNeeded { library, .. }
        | Hint::StaticOnly { library, .. } => (&library.flag, &library.object, library.kind),
        other => panic!("not a library hint: {other:?}"),
    }
}

#[test]
fn suggests_missing_libraries() {
    if !tools() {
        return;
    }
    let f = fixture("missing");
    let hinter = Hinter::new(f.scope.clone(), Vec::new());
    let undefined = [
        Undefined::new(b"foo_func"),
        Undefined::new(b"bar_func"),
        Undefined::new(b"dup_func"),
        Undefined::new(b"grp_func"),
        Undefined::new(b"weird_func"),
        Undefined::new(b"shadow_func"),
        Undefined::new(b"nowhere_func"),
    ];
    let hints = hinter.hints(&undefined, &[]);
    assert_eq!(hints.len(), undefined.len());

    assert_eq!(
        library_of(&hints[0][0]),
        ("-lfoo", "libfoo.so.1", LibraryKind::Shared)
    );
    assert_eq!(
        render(&hints[0][0], true),
        format!(
            "'foo_func' is defined in libfoo.so.1 ({}); did you forget -lfoo?",
            f.scratch.path("dir1/libfoo.so").display()
        )
    );
    assert_eq!(
        library_of(&hints[1][0]),
        ("-lbar", "libbar.a(bar.o)", LibraryKind::Static)
    );
    // Shared before static in the same directory, and one per library name.
    assert_eq!(hints[2].len(), 1);
    assert_eq!(
        library_of(&hints[2][0]),
        ("-ldup", "libdup.so", LibraryKind::Shared)
    );
    // Through a linker script.
    assert_eq!(
        library_of(&hints[3][0]),
        ("-lgrp", "libgrp_impl.so.2", LibraryKind::Shared)
    );
    // Names -l cannot spell, and names another directory shadows.
    assert_eq!(library_of(&hints[4][0]).0, "-l:weird.so");
    assert_eq!(library_of(&hints[5][0]).0, "-l:libshadow.so");
    assert!(hints[6].is_empty());
}

#[test]
fn explains_versions() {
    if !tools() {
        return;
    }
    let f = fixture("versions");
    let hinter = Hinter::new(f.scope.clone(), Vec::new());
    let hints = hinter.hints(
        &[
            Undefined::parse(b"vfunc@VER_3"),
            Undefined::parse(b"vfunc@VER_1"),
            Undefined::new(b"vfunc"),
        ],
        &[],
    );
    match &hints[0][..] {
        [
            Hint::VersionMismatch {
                wanted, available, ..
            },
        ] => {
            assert_eq!(wanted.as_deref(), Some("VER_3"));
            let versions: Vec<(Option<&str>, bool)> = available
                .iter()
                .map(|v| (v.library.version.as_deref(), v.default))
                .collect();
            assert_eq!(
                versions,
                vec![(Some("VER_1"), false), (Some("VER_2"), true)]
            );
            let text = render(&hints[0][0], true);
            assert_eq!(
                text,
                format!(
                    "'vfunc' is not defined at version VER_3; available: VER_1, VER_2 (default) in {}",
                    f.scratch.path("dir1/libver.so").display()
                )
            );
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(library_of(&hints[1][0]).0, "-lver");
    match &hints[2][0] {
        Hint::MissingLibrary { library, .. } => {
            assert_eq!(library.version.as_deref(), Some("VER_2"))
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn explains_linked_libraries() {
    if !tools() {
        return;
    }
    let f = fixture("linked");
    let linked = vec![
        LinkedLibrary {
            path: f.scratch.path("dir1/libfoo.so"),
            dropped_as_needed: true,
            static_only: false,
        },
        LinkedLibrary {
            path: f.scratch.path("dir1/libdyn.a"),
            dropped_as_needed: false,
            static_only: true,
        },
        LinkedLibrary::new(f.scratch.path("dir2/libbar.a")),
    ];
    let hinter = Hinter::new(f.scope.clone(), linked);
    let hints = hinter.hints(
        &[
            Undefined::new(b"foo_func"),
            Undefined::new(b"dyn_func"),
            Undefined::new(b"bar_func"),
        ],
        &[],
    );
    assert!(
        matches!(hints[0][..], [Hint::DroppedAsNeeded { .. }]),
        "{:?}",
        hints[0]
    );
    assert!(render(&hints[0][0], true).contains("--as-needed"));
    match &hints[1][..] {
        [
            Hint::StaticOnly {
                linked, library, ..
            },
        ] => {
            assert_eq!(linked, &f.scratch.path("dir1/libdyn.a"));
            assert_eq!(library.flag, "-ldyn");
        }
        other => panic!("{other:?}"),
    }
    // Linked and defining the symbol: nothing to say about libraries.
    assert!(hints[2].is_empty(), "{:?}", hints[2]);
}

#[test]
fn near_misses_through_the_hinter() {
    let hinter = Hinter::new(SearchScope::default(), Vec::new());
    let defined: Vec<&[u8]> = vec![b"_Z8cpp_funcv", b"_start_it", b"main"];
    let hints = hinter.hints(
        &[Undefined::new(b"cpp_func"), Undefined::new(b"start_it")],
        &defined,
    );
    match &hints[0][..] {
        [Hint::NearMiss { near, .. }] => {
            assert_eq!(near.kind, NearMissKind::CppDefinition);
            assert_eq!(
                render(&hints[0][0], true),
                "did you mean to declare cpp_func() as extern \"C\"?"
            );
        }
        other => panic!("{other:?}"),
    }
    assert!(
        matches!(&hints[1][..], [Hint::NearMiss { near, .. }] if near.kind == NearMissKind::Underscore)
    );
}

#[test]
fn sysroot_prefixes() {
    if !tools() {
        return;
    }
    let scratch = Scratch::new("sysroot");
    shared(
        &scratch,
        "int rooted(void) { return 1; }",
        "root/usr/lib/libroot.so",
        &[],
    );
    let scope = SearchScope {
        search_paths: vec![PathBuf::from("=/usr/lib")],
        sysroot: Some(scratch.path("root")),
    };
    let hints = Hinter::new(scope, Vec::new()).hints(&[Undefined::new(b"rooted")], &[]);
    assert_eq!(library_of(&hints[0][0]).0, "-lroot");
}

#[test]
fn malformed_libraries_are_skipped() {
    if !tools() {
        return;
    }
    let scratch = Scratch::new("malformed");
    let good = shared(
        &scratch,
        "int good_func(void) { return 1; }",
        "lib/libgood.so",
        &[],
    );
    let bytes = std::fs::read(&good).unwrap();
    let ar = archive(
        &scratch,
        &[("m", "int arch_func(void) { return 2; }")],
        "lib/libarch.a",
    );
    let ar_bytes = std::fs::read(&ar).unwrap();
    // Truncations and byte flips of a real shared object and archive, plus
    // scripts that loop or name missing files.
    let mut seed = 0x1234_5678_9abc_def1u64;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for i in 0..40 {
        let source = if i % 2 == 0 { &bytes } else { &ar_bytes };
        let mut corrupt = source.clone();
        if i % 3 == 0 {
            corrupt.truncate((next() as usize) % source.len());
        } else {
            for _ in 0..8 {
                let at = (next() as usize) % corrupt.len();
                corrupt[at] = next() as u8;
            }
        }
        std::fs::write(scratch.path(&format!("lib/libbad{i}.so")), corrupt).unwrap();
    }
    scratch.write("lib/libloop.so", "INPUT ( libloop.so )");
    scratch.write(
        "lib/libmissing.so",
        "GROUP ( /nonexistent/libx.so -lnothere )",
    );
    scratch.write("lib/libgarbage.so", "GROUP ( ( ( ");
    let scope = SearchScope {
        search_paths: vec![scratch.path("lib")],
        sysroot: None,
    };
    let hints = Hinter::new(scope.clone(), Vec::new()).hints(
        &[Undefined::new(b"good_func"), Undefined::new(b"arch_func")],
        &[],
    );
    assert!(!hints[0].is_empty() && !hints[1].is_empty());
    // Corrupt copies that still parse may define the symbols too, and sort
    // first; the intact libraries must be in the index either way.
    let flags = |hints: &[Hint]| -> Vec<String> {
        hints.iter().map(|h| library_of(h).0.to_string()).collect()
    };
    let index = LibraryIndex::build(&scope, &[], &[b"good_func", b"arch_func"]);
    let defines = |name: &[u8], file: &str| {
        index.definitions(name).iter().any(|d| {
            index.objects()[d.object]
                .path
                .file_name()
                .is_some_and(|n| n == file)
        })
    };
    assert!(
        defines(b"good_func", "libgood.so"),
        "{:?}",
        flags(&hints[0])
    );
    assert!(defines(b"arch_func", "libarch.a"), "{:?}", flags(&hints[1]));
}

#[test]
fn deterministic_across_thread_counts() {
    if !tools() {
        return;
    }
    let f = fixture("determinism");
    let names: Vec<&[u8]> = vec![
        b"foo_func",
        b"dup_func",
        b"grp_func",
        b"vfunc",
        b"shadow_func",
    ];
    let undefined: Vec<Undefined<'_>> = names.iter().map(|n| Undefined::new(n)).collect();
    let run_with = |threads: usize| {
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .unwrap()
            .install(|| Hinter::new(f.scope.clone(), Vec::new()).hints(&undefined, &names))
    };
    let one = run_with(1);
    for threads in [2, 8] {
        assert_eq!(one, run_with(threads));
    }
}

fn system_lib_dir() -> Option<&'static Path> {
    ["/usr/lib64", "/usr/lib/x86_64-linux-gnu", "/usr/lib"]
        .into_iter()
        .map(Path::new)
        .find(|p| p.join("libm.so").exists() || p.join("libc.so").exists())
}

#[test]
#[ignore = "scans the host's system library directory; run with --ignored"]
fn system_library_index() {
    let Some(dir) = system_lib_dir() else {
        eprintln!("skipping: no system library directory");
        return;
    };
    let scope = SearchScope {
        search_paths: vec![dir.to_path_buf()],
        sysroot: None,
    };
    let undefined = [
        Undefined::new(b"cos"),
        Undefined::parse(b"memcpy@GLIBC_2.99"),
        Undefined::new(b"pthread_create"),
        Undefined::new(b"deflate"),
        Undefined::new(b"_ZNSt8ios_base4InitC1Ev"),
    ];
    let names: Vec<&[u8]> = undefined.iter().map(|u| u.name).collect();
    let start = Instant::now();
    let index = LibraryIndex::build(&scope, &[], &names);
    let elapsed = start.elapsed();
    eprintln!(
        "indexed {} entries ({} distinct files) in {} in {:.1} ms on {} threads",
        index.entries().len(),
        index.objects().len(),
        dir.display(),
        elapsed.as_secs_f64() * 1000.0,
        rayon::current_num_threads()
    );
    let start = Instant::now();
    let hints = Hinter::new(scope, Vec::new()).hints(&undefined, &[]);
    eprintln!(
        "full hints in {:.1} ms",
        start.elapsed().as_secs_f64() * 1000.0
    );
    for (symbol, hints) in undefined.iter().zip(&hints) {
        eprintln!("{}:", String::from_utf8_lossy(symbol.name));
        for hint in hints {
            eprintln!("  >>> note: {}", render(hint, true));
        }
    }
    if dir.join("libm.so").exists() {
        assert!(
            hints[0].iter().any(
                |h| matches!(h, Hint::MissingLibrary { library, .. } if library.flag == "-lm")
            ),
            "{:?}",
            hints[0]
        );
    }
}
