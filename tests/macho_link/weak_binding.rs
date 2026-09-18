//! Exported weak definitions in legacy (`LC_DYLD_INFO_ONLY`) output: the
//! references are rebased to the image's own definition and listed in the
//! weak binding stream, sorted by name, as ld64 and `ld64.lld` do, so that
//! dyld coalesces them across images (C++ type information, inline
//! functions' static locals).

use std::collections::BTreeSet;

use super::{clang_for, ld64_lld, link_bytes, objdump, os, scratch, skip, syslibroot, tool_works};

const SOURCE: &str = "\
__attribute__((weak)) int shared_value = 3;
__attribute__((weak)) int weak_function(void) { return 1; }
int (*const function_pointer)(void) = weak_function;
int *value_pointer = &shared_value;
int use_them(void) { return weak_function() + shared_value + *value_pointer; }
";

/// `(symbol, section)` of each weak binding `llvm-objdump` lists.
fn weak_binds(file: &std::path::Path) -> Option<BTreeSet<(String, String)>> {
    let text = objdump(&["--macho", "--weak-bind"], file)?;
    Some(
        text.lines()
            .filter_map(|line| {
                let fields: Vec<&str> = line.split_whitespace().collect();
                // segment section address type addend symbol
                let symbol = fields.last()?;
                let section = fields.get(1)?;
                symbol
                    .starts_with('_')
                    .then(|| ((*symbol).to_owned(), (*section).to_owned()))
            })
            .collect(),
    )
}

#[test]
fn legacy_weak_binding() {
    for arch in ["arm64", "x86_64"] {
        if !clang_for(arch) {
            skip(
                "legacy_weak_binding",
                &format!("clang cannot target {arch}-apple-macos"),
            );
            continue;
        }
        let dir = scratch("legacy_weak");
        let source = dir.join("weak.c");
        std::fs::write(&source, SOURCE).unwrap();
        let object = dir.join(format!("weak-{arch}.o"));
        assert!(tool_works(
            "clang",
            &[
                &format!("--target={arch}-apple-macos11"),
                "-O1",
                "-c",
                source.to_str().unwrap(),
                "-o",
                object.to_str().unwrap(),
            ]
        ));
        let root = syslibroot();
        let args = os(&[
            "-arch",
            arch,
            "-dylib",
            "-platform_version",
            "macos",
            "11.0",
            "11.0",
            "-syslibroot",
            root.to_str().unwrap(),
            object.to_str().unwrap(),
            "-lSystem",
        ]);
        let (bytes, _) = link_bytes(&args).unwrap();
        let ours = dir.join(format!("libweak-{arch}.dylib"));
        std::fs::write(&ours, &bytes).unwrap();

        let Some(binds) = weak_binds(&ours) else {
            skip("legacy_weak_binding", "llvm-objdump is not installed");
            continue;
        };
        let names: BTreeSet<&str> = binds.iter().map(|(s, _)| s.as_str()).collect();
        assert_eq!(
            names,
            BTreeSet::from(["_shared_value", "_weak_function"]),
            "{arch}: {binds:?}"
        );
        // No ordinary bind for them: they are rebased to this image.
        let bind = objdump(&["--macho", "--bind"], &ours).unwrap_or_default();
        assert!(!bind.contains("_weak_function"), "{arch}: {bind}");

        if let Some(lld) = ld64_lld() {
            let theirs = dir.join(format!("libweak-{arch}-lld.dylib"));
            let status = std::process::Command::new(lld)
                .args(&args)
                .arg("-o")
                .arg(&theirs)
                .status()
                .unwrap();
            assert!(status.success());
            let reference = weak_binds(&theirs).unwrap();
            let reference_names: BTreeSet<&str> =
                reference.iter().map(|(s, _)| s.as_str()).collect();
            assert_eq!(names, reference_names, "{arch}: qld vs ld64.lld");
        }
    }
}
