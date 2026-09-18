//! Undefined symbols with `-dead_strip`: only those that live code refers
//! to are errors, as in ld64 and `ld64.lld`. Without `-dead_strip` every
//! referenced undefined symbol is one.

use std::process::Command;

use super::{clang_for, ld64_lld, link_bytes, os, scratch, skip, syslibroot, tool_works};

const SOURCE: &str = "\
void missing_dead(void);
void missing_live(int);
void unused(void) { missing_dead(); }
int main(int argc, char **argv) {
    if (argc > 5)
        missing_live(argc);
    return 0;
}
";

const DEAD_ONLY: &str = "\
void missing_dead(void);
void unused(void) { missing_dead(); }
int main(void) { return 0; }
";

fn compile(name: &str, source: &str) -> std::path::PathBuf {
    let dir = scratch("undefined_dead_strip");
    let path = dir.join(format!("{name}.c"));
    std::fs::write(&path, source).unwrap();
    let object = dir.join(format!("{name}.o"));
    assert!(tool_works(
        "clang",
        &[
            "--target=arm64-apple-macos13",
            "-O1",
            "-c",
            path.to_str().unwrap(),
            "-o",
            object.to_str().unwrap(),
        ]
    ));
    object
}

fn args(object: &std::path::Path, extra: &[&str]) -> Vec<std::ffi::OsString> {
    let root = syslibroot();
    let mut args = os(&[
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
    ]);
    args.extend(os(extra));
    args
}

#[test]
fn undefined_symbols_after_dead_stripping() {
    if !clang_for("arm64") {
        skip(
            "undefined_symbols_after_dead_stripping",
            "clang cannot target arm64-apple-macos",
        );
        return;
    }
    let dead_only = compile("dead_only", DEAD_ONLY);
    let both = compile("both", SOURCE);

    // Only dead code refers to it: an error without -dead_strip, fine
    // with it.
    let error = link_bytes(&args(&dead_only, &[])).unwrap_err();
    assert!(error.contains("undefined symbol: _missing_dead"), "{error}");
    let (bytes, _) = link_bytes(&args(&dead_only, &["-dead_strip"])).unwrap();
    assert!(!bytes.is_empty());

    // Live code refers to one: only that one is reported.
    let error = link_bytes(&args(&both, &["-dead_strip"])).unwrap_err();
    assert!(error.contains("undefined symbol: _missing_live"), "{error}");
    assert!(!error.contains("_missing_dead"), "{error}");

    // A symbol the command line asks for is always reported.
    let error = link_bytes(&args(&dead_only, &["-dead_strip", "-u", "_missing_dead"])).unwrap_err();
    assert!(error.contains("undefined symbol: _missing_dead"), "{error}");

    if let Some(lld) = ld64_lld() {
        let out = scratch("undefined_dead_strip").join("dead_only-lld");
        let status = Command::new(&lld)
            .args(args(&dead_only, &["-dead_strip"]))
            .arg("-o")
            .arg(&out)
            .output()
            .unwrap();
        assert!(
            status.status.success(),
            "ld64.lld: {}",
            String::from_utf8_lossy(&status.stderr)
        );
    }
}
