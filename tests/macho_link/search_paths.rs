//! Library search paths with `-syslibroot`: only absolute `-L` directories
//! are looked up under the SDK, as in ld64 and lld. Apple clang passes
//! `-syslibroot` on every link, so `-L.` must stay the current directory
//! (a build's own `libsqlite3.dylib` must win over the SDK's).

use std::path::PathBuf;

use super::{clang_for, link_bytes, os, scratch, skip, syslibroot, tool_works};

#[test]
fn relative_library_paths_are_not_rerooted() {
    let arch = "arm64";
    if !clang_for(arch) {
        skip(
            "relative_library_paths_are_not_rerooted",
            "clang cannot target arm64-apple-macos",
        );
        return;
    }
    let dir = scratch("search_paths");
    let source = dir.join("lib.c");
    std::fs::write(&source, "int local_library(void) { return 7; }\n").unwrap();
    let object = dir.join("lib.o");
    assert!(tool_works(
        "clang",
        &[
            "--target=arm64-apple-macos13",
            "-c",
            source.to_str().unwrap(),
            "-o",
            object.to_str().unwrap(),
        ]
    ));
    let main_source = dir.join("main.c");
    std::fs::write(
        &main_source,
        "int local_library(void);\nint main(void) { return local_library(); }\n",
    )
    .unwrap();
    let main = dir.join("main.o");
    assert!(tool_works(
        "clang",
        &[
            "--target=arm64-apple-macos13",
            "-c",
            main_source.to_str().unwrap(),
            "-o",
            main.to_str().unwrap(),
        ]
    ));

    // The library, in a directory named relative to the working directory
    // (the package root, where cargo runs tests).
    let local = dir.join("local");
    std::fs::create_dir_all(&local).unwrap();
    let root = syslibroot();
    let (dylib, _) = link_bytes(&os(&[
        "-arch",
        arch,
        "-dylib",
        "-platform_version",
        "macos",
        "13.0",
        "13.0",
        "-syslibroot",
        root.to_str().unwrap(),
        "-install_name",
        "@rpath/libqldrel.dylib",
        object.to_str().unwrap(),
        "-lSystem",
    ]))
    .unwrap();
    std::fs::write(local.join("libqldrel.dylib"), dylib).unwrap();
    let relative: PathBuf = local
        .strip_prefix(std::env::current_dir().unwrap())
        .expect("the scratch directory is under the working directory")
        .to_path_buf();

    // A syslibroot with the same relative directory, empty: the relative
    // `-L` must not be looked up there. (No libSystem: the executable is
    // only linked, not run.)
    let sysroot = dir.join("root");
    std::fs::create_dir_all(sysroot.join(&relative)).unwrap();
    let (bytes, _) = link_bytes(&os(&[
        "-arch",
        arch,
        "-platform_version",
        "macos",
        "13.0",
        "13.0",
        "-syslibroot",
        sysroot.to_str().unwrap(),
        main.to_str().unwrap(),
        &format!("-L{}", relative.display()),
        "-lqldrel",
    ]))
    .unwrap_or_else(|e| panic!("relative -L under -syslibroot: {e}"));
    assert!(!bytes.is_empty());
}
