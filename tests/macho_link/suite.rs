//! The `-fuse-ld` suite (roadmap M8 exit criterion): C, C++ and
//! Objective-C programs in `tests/data/macho_link/suite/`, built by Apple
//! clang on macOS with `-fuse-ld=<qld>` against the real SDK, and run on
//! arm64 and (under Rosetta) x86_64.
//!
//! Each program checks itself and prints `<name> suite: N checks, 0
//! failures`. Every program comes with a dylib, so each link covers an
//! executable and a library; the C suite also loads a bundle linked with
//! `-bundle_loader`, and the Objective-C suite links a static archive of
//! categories with `-ObjC`. Each is built three ways: plain, with
//! `-dead_strip`, and for macOS 11 (legacy `LC_DYLD_INFO_ONLY` instead of
//! chained fixups).
//!
//! The programs need the SDK's headers, so elsewhere the test only says it
//! skips. Larger downloaded projects (zlib, Lua, SQLite, {fmt}) are built
//! by `tests/projects/macos-suite.sh`.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::{data_dir, host_can_run, make_archive, require_tools, scratch, skip};

/// One way of building every program.
struct Variant {
    name: &'static str,
    flags: &'static [&'static str],
}

const VARIANTS: &[Variant] = &[
    Variant {
        name: "default",
        flags: &[],
    },
    Variant {
        name: "dead_strip",
        flags: &["-Wl,-dead_strip"],
    },
    Variant {
        name: "macos11",
        flags: &["-mmacosx-version-min=11.0"],
    },
];

/// Builds with the clang driver, linking with `linker`.
struct Builder<'a> {
    linker: &'a Path,
    arch: &'a str,
    variant: &'a Variant,
    dir: PathBuf,
}

impl Builder<'_> {
    /// Runs `driver` (`clang` or `clang++`) with the architecture, the
    /// linker and the variant's flags, then `args`.
    fn run(&self, driver: &str, args: &[&str]) -> Result<(), String> {
        let mut command = Command::new(driver);
        command
            .arg("-arch")
            .arg(self.arch)
            .arg(format!("-fuse-ld={}", self.linker.display()))
            .args(self.variant.flags)
            .arg("-O1")
            .arg("-I")
            .arg(data_dir().join("suite"))
            .args(args)
            .current_dir(&self.dir);
        let output = command
            .output()
            .map_err(|e| format!("cannot run {driver}: {e}"))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(format!(
                "{driver} {}: {}\n{}",
                args.join(" "),
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ))
        }
    }

    fn source(name: &str) -> String {
        data_dir().join("suite").join(name).display().to_string()
    }

    fn path(&self, name: &str) -> String {
        self.dir.join(name).display().to_string()
    }

    /// Runs a built program and checks its summary line.
    fn check(&self, program: &str, args: &[String], summary: &str) -> Result<(), String> {
        let output = Command::new(self.dir.join(program))
            .args(args)
            .current_dir(&self.dir)
            .output()
            .map_err(|e| format!("cannot run {program}: {e}"))?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let passed = output.status.success()
            && stdout
                .lines()
                .any(|l| l.starts_with(summary) && l.ends_with(", 0 failures"));
        if passed {
            Ok(())
        } else {
            Err(format!(
                "{program}: {}\n{stdout}{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ))
        }
    }

    fn c(&self) -> Result<(), String> {
        let lib = self.path("libsuite_c.dylib");
        self.run(
            "clang",
            &[
                "-dynamiclib",
                &Self::source("c_lib.c"),
                "-install_name",
                "@rpath/libsuite_c.dylib",
                "-o",
                &lib,
            ],
        )?;
        let exe = self.path("c_suite");
        self.run(
            "clang",
            &[
                &Self::source("c_suite.c"),
                "-L.",
                "-lsuite_c",
                "-Wl,-rpath,@executable_path",
                "-Wl,-U,_suite_c_missing",
                "-o",
                &exe,
            ],
        )?;
        let bundle = self.path("c_plugin.bundle");
        self.run(
            "clang",
            &[
                "-bundle",
                &Self::source("c_plugin.c"),
                "-bundle_loader",
                &exe,
                "-L.",
                "-lsuite_c",
                "-o",
                &bundle,
            ],
        )?;
        self.check("c_suite", &[bundle], "c suite:")
    }

    fn cxx(&self) -> Result<(), String> {
        let lib = self.path("libsuite_cxx.dylib");
        self.run(
            "clang++",
            &[
                "-std=c++17",
                "-dynamiclib",
                &Self::source("cxx_lib.cpp"),
                "-install_name",
                "@rpath/libsuite_cxx.dylib",
                "-o",
                &lib,
            ],
        )?;
        self.run(
            "clang++",
            &[
                "-std=c++17",
                &Self::source("cxx_suite.cpp"),
                "-L.",
                "-lsuite_cxx",
                "-Wl,-rpath,@executable_path",
                "-o",
                &self.path("cxx_suite"),
            ],
        )?;
        self.check("cxx_suite", &[], "c++ suite:")
    }

    fn objc(&self) -> Result<(), String> {
        let lib = self.path("libsuite_objc.dylib");
        self.run(
            "clang",
            &[
                "-fobjc-arc",
                "-dynamiclib",
                &Self::source("objc_lib.m"),
                "-framework",
                "Foundation",
                "-install_name",
                "@rpath/libsuite_objc.dylib",
                "-o",
                &lib,
            ],
        )?;
        let category = self.path("objc_category.o");
        self.run(
            "clang",
            &[
                "-fobjc-arc",
                "-c",
                &Self::source("objc_category.m"),
                "-o",
                &category,
            ],
        )?;
        let archive = self.dir.join("libsuite_category.a");
        if !make_archive(&archive, &[Path::new(&category)]) {
            return Err("neither llvm-ar nor libtool works".into());
        }
        let main = self.path("objc_suite.o");
        self.run(
            "clang",
            &[
                "-fobjc-arc",
                "-c",
                &Self::source("objc_suite.m"),
                "-o",
                &main,
            ],
        )?;
        let mixed = self.path("objc_mixed.o");
        self.run(
            "clang++",
            &[
                "-std=c++17",
                "-fobjc-arc",
                "-c",
                &Self::source("objc_mixed.mm"),
                "-o",
                &mixed,
            ],
        )?;
        self.run(
            "clang++",
            &[
                &main,
                &mixed,
                "-L.",
                "-lsuite_objc",
                "-lsuite_category",
                "-Wl,-ObjC",
                "-framework",
                "Foundation",
                "-Wl,-rpath,@executable_path",
                "-o",
                &self.path("objc_suite"),
            ],
        )?;
        self.check("objc_suite", &[], "objc suite:")
    }
}

#[test]
fn fuse_ld_suite() {
    if !cfg!(target_os = "macos") {
        assert!(
            !require_tools(),
            "fuse_ld_suite: the suite needs the macOS SDK"
        );
        eprintln!("skipping fuse_ld_suite: needs macOS and its SDK");
        return;
    }
    let root = scratch("suite");
    let linker = root.join("ld64.qld");
    let _ = std::fs::remove_file(&linker);
    #[cfg(unix)]
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_qld"), &linker).unwrap();
    let mut failures = Vec::new();
    let mut passed = 0usize;
    for arch in ["arm64", "x86_64"] {
        if !host_can_run(arch) {
            skip("fuse_ld_suite", &format!("this host cannot run {arch}"));
            continue;
        }
        for variant in VARIANTS {
            let dir = root.join(format!("{arch}-{}", variant.name));
            std::fs::create_dir_all(&dir).unwrap();
            let builder = Builder {
                linker: &linker,
                arch,
                variant,
                dir,
            };
            for (language, result) in [
                ("c", builder.c()),
                ("c++", builder.cxx()),
                ("objc", builder.objc()),
            ] {
                match result {
                    Ok(()) => passed += 1,
                    Err(error) => {
                        failures.push(format!("{arch} {} {language}: {error}", variant.name));
                    }
                }
            }
        }
    }
    eprintln!("fuse_ld_suite: {passed} passed, {} failed", failures.len());
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}
