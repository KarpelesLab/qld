//! The fixture runner: compiles, links, runs and inspects every
//! `tests/fixtures/*/test.toml`. See `tests/README.md`.
//!
//! Fixtures whose link reports "not implemented yet" are skipped, so this
//! suite stays green while qld's pipeline is being built.

mod common;

use common::fixture::{self, Fixture, JobOptions, Linker};
use common::process;
use common::toml::{self, Value};

fn run_suite(suite: &str, linker: Linker) {
    let fixtures = Fixture::load_all(&fixture::fixtures_root()).unwrap_or_else(|e| panic!("{e}"));
    let fixtures = fixture::filter_from_env(fixtures);
    let jobs = fixture::jobs(&fixtures);
    let options = JobOptions {
        scratch: fixture::scratch_root(&format!("{suite}-{}", linker.slug())),
        linker,
    };
    println!(
        "running {} fixture jobs with {} ({} threads)",
        jobs.len(),
        options.linker.name(),
        process::job_count()
    );
    let results = process::parallel_map(&jobs, process::job_count(), |job| {
        fixture::run_job(&fixtures[job.fixture], job, &options)
    });
    if let Some(failures) = fixture::report(suite, &results) {
        panic!("{failures}");
    }
}

/// Runs the fixtures with qld, or with the linker named by
/// `QLD_FIXTURE_LINKER` (`ld`, `lld`, `mold`, `gold` or a path).
#[test]
fn fixtures() {
    run_suite("fixtures", Linker::from_env("QLD_FIXTURE_LINKER"));
}

/// Validates the fixtures themselves against GNU ld:
/// `cargo test --test fixtures -- --ignored`.
#[test]
#[ignore = "validates the fixtures against GNU ld; run with --ignored"]
fn fixtures_with_gnu_ld() {
    run_suite("validate", Linker::GnuLd);
}

/// Every `test.toml` parses and uses only known keys, on every host.
#[test]
fn fixture_files_are_valid() {
    let fixtures = Fixture::load_all(&fixture::fixtures_root()).unwrap_or_else(|e| panic!("{e}"));
    assert!(fixtures.len() >= 15, "expected at least 15 fixtures");
    for fixture in &fixtures {
        assert!(
            fixture.run.is_some() || !fixture.expect_readelf.is_empty() || fixture.skip.is_some(),
            "{}: a fixture must run something or inspect its output",
            fixture.name
        );
        assert!(
            fixture.expect_stdout.is_none() || fixture.run.is_some(),
            "{}: expect.stdout needs run",
            fixture.name
        );
    }
}

#[test]
fn toml_subset() {
    let doc = r#"
# comment
compile = ["cc -c a.c", 'cc -c b.c',] # trailing comma
link = "-o out a.o"
expect.stdout = "hello\n\tworld \u00e9"
expect.exit = -3
determinism = true
expect.readelf_files."libfoo.so" = [
  "SONAME",   # must
  "!TEXTREL", # must not
]
multi = """
line 1
line 2 \
  continued"""
literal = '''C:\path'''

[diff]
ignore = []
"#;
    let entries = toml::parse(doc).unwrap();
    let get = |key: &str| {
        entries
            .iter()
            .find(|e| e.key.join(".") == key)
            .map(|e| e.value.clone())
            .unwrap_or_else(|| panic!("missing {key}"))
    };
    let s = |v: &str| Value::String(v.to_string());
    assert_eq!(
        get("compile"),
        Value::Array(vec![s("cc -c a.c"), s("cc -c b.c")])
    );
    assert_eq!(get("expect.stdout"), s("hello\n\tworld \u{e9}"));
    assert_eq!(get("expect.exit"), Value::Integer(-3));
    assert_eq!(get("determinism"), Value::Bool(true));
    assert_eq!(
        get("expect.readelf_files.libfoo.so"),
        Value::Array(vec![s("SONAME"), s("!TEXTREL")])
    );
    assert_eq!(get("multi"), s("line 1\nline 2 continued"));
    assert_eq!(get("literal"), s("C:\\path"));
    assert_eq!(get("diff.ignore"), Value::Array(vec![]));

    for bad in [
        "a = ",
        "a = \"unterminated",
        "a = [1, 2",
        "a = 1\na = 2",
        "a = 1\na.b = 2",
        "a = 1.5",
        "[[tables]]",
        "a = { inline = 1 }",
        "= 1",
        "a = \"\\q\"",
        "a = 1 b",
    ] {
        assert!(toml::parse(bad).is_err(), "should reject {bad:?}");
    }
    let err = toml::parse("a = 1\n\nb = @").unwrap_err();
    assert_eq!(err.line, 3);
}

#[test]
fn word_splitting() {
    assert_eq!(
        process::split_words(r#"-o out 'a b.o' "c\"d" e\ f -Wl,-rpath,$ORIGIN"#).unwrap(),
        vec!["-o", "out", "a b.o", "c\"d", "e f", "-Wl,-rpath,$ORIGIN"]
    );
    assert!(process::split_words("'open").is_err());
    assert_eq!(process::shell_quote("a'b"), r"'a'\''b'");
    assert_eq!(process::shell_quote("/usr/bin/cc"), "/usr/bin/cc");
}

/// Under qemu, every program of a `run` chain gets the emulator, after its
/// own environment assignments.
#[test]
fn qemu_runs_every_program_of_a_chain() {
    assert_eq!(
        fixture::qemu_script("./out", "/usr/bin/qemu-aarch64").unwrap(),
        "/usr/bin/qemu-aarch64 ./out"
    );
    assert_eq!(
        fixture::qemu_script("./out && LD_LIBRARY_PATH=. ./b arg || ./c ; ./d", "/q/qemu").unwrap(),
        "/q/qemu ./out && LD_LIBRARY_PATH=. /q/qemu ./b arg || /q/qemu ./c ; /q/qemu ./d"
    );
    // `;` needs spaces around it to separate commands, as `&&` does.
    assert!(fixture::qemu_script("'open", "q").is_err());
}

/// `expect.readelf_arch.<arch>` is parsed per architecture.
#[test]
fn readelf_patterns_per_architecture() {
    let fixtures = Fixture::load_all(&fixture::fixtures_root()).unwrap_or_else(|e| panic!("{e}"));
    let pie = fixtures
        .iter()
        .find(|f| f.name == "pie")
        .expect("the pie fixture");
    let arches: Vec<&str> = pie
        .expect_readelf_arch
        .iter()
        .map(|(arch, ..)| arch.as_str())
        .collect();
    assert_eq!(arches, ["x86_64", "aarch64"]);
    assert!(
        !pie.expect_readelf.iter().any(|p| p.contains("R_X86_64")),
        "architecture-specific patterns belong in expect.readelf_arch"
    );
}

#[test]
fn triples_normalize() {
    use common::tools::Triple;
    let t = |s: &str| Triple::parse(s).unwrap().to_string();
    assert_eq!(t("x86_64-pc-linux-gnu"), "x86_64-linux-gnu");
    assert_eq!(t("x86_64-linux-gnu"), "x86_64-linux-gnu");
    assert_eq!(t("aarch64-unknown-linux-musl"), "aarch64-linux-musl");
    assert_eq!(t("arm64-apple-darwin23.0.0"), "aarch64-darwin");
    assert_eq!(t("riscv64-linux-gnu"), "riscv64-linux-gnu");
    assert!(!Triple::parse("x86_64-w64-mingw32").unwrap().is_linux());
}

#[test]
fn text_diff_is_readable() {
    let diff = common::textdiff::diff("a\nb\nc\n", "a\nx\nc");
    assert!(diff.contains("- b\n"), "{diff}");
    assert!(diff.contains("+ x\n"), "{diff}");
    assert!(diff.contains("No newline at end"), "{diff}");
}
