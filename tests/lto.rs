//! Integration tests for LTO in the ELF driver (`qld::elf::lto`, workstream
//! W18).
//!
//! - `rules_*`: the mapping from what resolution found to the `LDPR_*`
//!   resolution reported to plugins ([`qld::elf::lto::symbol_resolution`]).
//! - `stub_*`: the `qld` binary with `tests/data/lto/stub_plugin.c`, a
//!   plugin built with the host C compiler that claims fake IR files (LLVM
//!   bitcode magic followed by a symbol list), prints the resolutions it is
//!   given, and adds native objects after all symbols are read. They check
//!   the resolutions qld reports end to end, archive members without an
//!   index, the second resolution with generated objects, libraries a
//!   plugin adds, and the diagnostics for IR that cannot be linked.
//! - `no_plugin_*`: IR inputs without `-plugin`, and `-plugin` without IR.
//!
//! The binary is run as a child process because plugins can be used once per
//! process. Missing tools make a test print `SKIPPED:` and pass. Real
//! compilers are covered by the `lto-*` fixtures.

#[cfg(target_os = "linux")]
mod common;

#[cfg(feature = "plugin")]
mod rules {
    use qld::elf::lto::{SymbolFacts, Winner, symbol_resolution};
    use qld::plugin::{SymbolResolution as R, Visibility};

    fn definition() -> SymbolFacts {
        SymbolFacts {
            prevailing: true,
            winner: Winner::Ir,
            ..SymbolFacts::default()
        }
    }

    #[test]
    fn rules_references() {
        let reference = |winner| {
            symbol_resolution(&SymbolFacts {
                undefined: true,
                winner,
                // Irrelevant to references.
                regular_ref: true,
                export_all: true,
                ..SymbolFacts::default()
            })
        };
        assert_eq!(reference(Winner::Undefined), R::Undefined);
        assert_eq!(reference(Winner::Ir), R::ResolvedIr);
        assert_eq!(reference(Winner::Regular), R::ResolvedExec);
        assert_eq!(reference(Winner::Shared), R::ResolvedDyn);
    }

    #[test]
    fn rules_preempted_definitions() {
        let preempted = |winner| {
            symbol_resolution(&SymbolFacts {
                prevailing: false,
                winner,
                ..definition()
            })
        };
        assert_eq!(preempted(Winner::Ir), R::PreemptedIr);
        assert_eq!(preempted(Winner::Regular), R::PreemptedRegular);
        // A discarded COMDAT copy whose kept copy defines nothing.
        assert_eq!(preempted(Winner::Undefined), R::PreemptedRegular);
    }

    #[test]
    fn rules_prevailing_definitions() {
        assert_eq!(symbol_resolution(&definition()), R::PrevailingDefIronly);
        for facts in [
            SymbolFacts {
                regular_ref: true,
                ..definition()
            },
            SymbolFacts {
                relocatable: true,
                ..definition()
            },
            SymbolFacts {
                wrapped: true,
                ..definition()
            },
            // Referenced from a regular object: kept even when hidden.
            SymbolFacts {
                regular_ref: true,
                visibility: Visibility::Hidden,
                ..definition()
            },
        ] {
            assert_eq!(symbol_resolution(&facts), R::PrevailingDef, "{facts:?}");
        }
    }

    #[test]
    fn rules_visible_from_outside() {
        for facts in [
            SymbolFacts {
                export_all: true,
                ..definition()
            },
            SymbolFacts {
                dynamic_ref: true,
                ..definition()
            },
            SymbolFacts {
                listed: true,
                ..definition()
            },
            SymbolFacts {
                export_all: true,
                visibility: Visibility::Protected,
                ..definition()
            },
        ] {
            assert_eq!(
                symbol_resolution(&facts),
                R::PrevailingDefIronlyExp,
                "{facts:?}"
            );
        }
        for facts in [
            SymbolFacts {
                export_all: true,
                visibility: Visibility::Hidden,
                ..definition()
            },
            SymbolFacts {
                dynamic_ref: true,
                visibility: Visibility::Internal,
                ..definition()
            },
            SymbolFacts {
                export_all: true,
                script_local: true,
                ..definition()
            },
        ] {
            assert_eq!(
                symbol_resolution(&facts),
                R::PrevailingDefIronly,
                "{facts:?}"
            );
        }
    }
}

#[test]
fn ir_errors_name_the_file_and_the_plugin() {
    use qld::elf::inputs::LtoMode;
    use qld::elf::lto::{IrKind, ir_error};

    let text = |kind, mode| ir_error("dir/a.o", kind, mode).to_string();
    let bitcode = text(IrKind::LlvmBitcode, LtoMode::NoPlugin);
    assert!(bitcode.starts_with("dir/a.o: LLVM bitcode"), "{bitcode}");
    assert!(bitcode.contains("-plugin LLVMgold.so"), "{bitcode}");
    let gcc = text(IrKind::GccSlim, LtoMode::NoPlugin);
    assert!(gcc.contains("gcc -flto"), "{gcc}");
    assert!(gcc.contains("liblto_plugin.so"), "{gcc}");
    let unsupported = text(IrKind::GccSlim, LtoMode::Unsupported);
    assert!(unsupported.contains("`plugin`"), "{unsupported}");
    let declined = text(IrKind::LlvmBitcode, LtoMode::Claim);
    assert!(declined.contains("no LTO plugin claimed"), "{declined}");
    let late = text(IrKind::LlvmBitcode, LtoMode::AfterLto);
    assert!(late.contains("not part of LTO"), "{late}");
}

#[cfg(target_os = "linux")]
#[cfg_attr(not(feature = "plugin"), allow(dead_code))]
mod linux {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use super::common::tools::{find_program, skip};

    const QLD: &str = env!("CARGO_BIN_EXE_qld");

    /// A fresh work directory for one test.
    fn work_dir(name: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("lto")
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn cc() -> Option<PathBuf> {
        let found = find_program("cc").or_else(|| find_program("gcc"));
        if found.is_none() {
            skip("no C compiler");
        }
        found
    }

    fn run_ok(command: &mut Command) {
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{command:?} failed:\n{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Builds the stub plugin into `work`.
    fn stub_plugin(cc: &Path, work: &Path) -> PathBuf {
        let plugin = work.join("stub_plugin.so");
        run_ok(
            Command::new(cc)
                .args(["-shared", "-fPIC", "-O1", "-o"])
                .arg(&plugin)
                .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/lto/stub_plugin.c")),
        );
        plugin
    }

    /// Writes a fake IR file the stub plugin claims. Each symbol line is
    /// "<kind> <visibility> <name> [<comdat key>]"; kinds: 0 definition, 1
    /// weak definition, 2 undefined, 3 weak undefined, 4 common;
    /// visibilities: 0 default, 1 protected, 2 internal, 3 hidden.
    fn write_ir(path: &Path, symbols: &[&str]) {
        let mut data = b"BC\xc0\xdeQLDLTO\n".to_vec();
        for line in symbols {
            data.extend_from_slice(line.as_bytes());
            data.push(b'\n');
        }
        std::fs::write(path, data).unwrap();
    }

    /// Assembles `source` (GNU assembler syntax) into `work/<name>.o`.
    fn assemble(cc: &Path, work: &Path, name: &str, source: &str) -> PathBuf {
        let input = work.join(format!("{name}.s"));
        std::fs::write(&input, source).unwrap();
        let object = work.join(format!("{name}.o"));
        run_ok(Command::new(cc).args(["-c", "-o"]).arg(&object).arg(&input));
        object
    }

    /// Writes an archive without a symbol index holding `members`.
    fn write_archive_without_index(path: &Path, members: &[(&str, &Path)]) {
        let mut data = b"!<arch>\n".to_vec();
        for (name, member) in members {
            let contents = std::fs::read(member).unwrap();
            let header = format!(
                "{:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n",
                format!("{name}/"),
                0,
                0,
                0,
                644,
                contents.len()
            );
            assert_eq!(header.len(), 60);
            data.extend_from_slice(header.as_bytes());
            data.extend_from_slice(&contents);
            if contents.len() % 2 == 1 {
                data.push(b'\n');
            }
        }
        std::fs::write(path, data).unwrap();
    }

    struct Link {
        success: bool,
        stderr: String,
    }

    impl Link {
        /// The plugin's notes, without the `qld: note: ` prefix.
        fn notes(&self) -> Vec<&str> {
            self.stderr
                .lines()
                .filter_map(|line| line.strip_prefix("qld: note: "))
                .collect()
        }

        /// Symbol resolutions the plugin was given, by file (its base name
        /// and offset) and symbol; -1 for files reported as not included.
        fn resolutions(&self) -> BTreeMap<(String, String), i32> {
            let mut map = BTreeMap::new();
            for note in self.notes() {
                let Some(rest) = note.strip_prefix("resolve ") else {
                    continue;
                };
                let mut words = rest.split(' ');
                let file = words.next().unwrap();
                let file = Path::new(file)
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                for word in words.skip(1) {
                    let (name, value) = word.rsplit_once('=').unwrap();
                    map.insert((file.clone(), name.to_owned()), value.parse().unwrap());
                }
            }
            map
        }

        fn resolution(&self, file: &str, name: &str) -> i32 {
            *self
                .resolutions()
                .get(&(file.to_owned(), name.to_owned()))
                .unwrap_or_else(|| panic!("no resolution for {file} {name}:\n{}", self.stderr))
        }
    }

    fn link(work: &Path, args: &[&str]) -> Link {
        let output = Command::new(QLD)
            .args(args)
            .current_dir(work)
            .output()
            .unwrap();
        Link {
            success: output.status.success(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }

    // Plugin interface resolutions.
    const UNDEF: i32 = 1;
    const PREVAILING_DEF: i32 = 2;
    const PREVAILING_DEF_IRONLY: i32 = 3;
    const PREEMPTED_REG: i32 = 4;
    const PREEMPTED_IR: i32 = 5;
    const RESOLVED_IR: i32 = 6;
    const RESOLVED_EXEC: i32 = 7;
    const RESOLVED_DYN: i32 = 8;
    const PREVAILING_DEF_IRONLY_EXP: i32 = 9;

    /// The native code "generated" for the executable scenario: defines what
    /// the IR defined that the rest of the link needs.
    const GENERATED: &str = "
        .text
        .globl start_qld
        start_qld: ret
        .globl used_by_native_qld
        used_by_native_qld: ret
        .globl forced_qld
        forced_qld: ret
    ";

    const NATIVE: &str = "
        .text
        .globl native_func_qld
        native_func_qld: call used_by_native_qld
        ret
        .globl preempted_qld
        preempted_qld: ret
        .section .text.native_group_qld,\"axG\",@progbits,native_key_qld,comdat
        .globl group_member_qld
        group_member_qld: ret
    ";

    #[test]
    #[cfg(feature = "plugin")]
    fn stub_executable_resolutions() {
        let Some(cc) = cc() else { return };
        let work = work_dir("executable");
        let plugin = stub_plugin(&cc, &work);
        assemble(&cc, &work, "native", NATIVE);
        assemble(&cc, &work, "generated", GENERATED);
        write_ir(
            &work.join("a.bc"),
            &[
                "0 0 start_qld",
                "0 0 used_by_native_qld",
                "0 0 ir_only_qld",
                "1 0 preempted_qld",
                "1 0 dup_weak_qld",
                "2 0 native_func_qld",
                "3 0 missing_weak_qld",
                "0 0 inline_fn_qld inline_key_qld",
                "0 0 group_member_qld native_key_qld",
                "0 0 forced_qld",
                "0 3 hidden_def_qld",
                "4 0 common_qld",
                "2 0 lazy_used_qld",
            ],
        );
        write_ir(
            &work.join("b.bc"),
            &[
                "2 0 ir_only_qld",
                "1 0 dup_weak_qld",
                "0 0 inline_fn_qld inline_key_qld",
                "2 0 start_qld",
            ],
        );
        write_ir(&work.join("c.bc"), &["0 0 lazy_unused_qld"]);
        write_ir(&work.join("d.bc"), &["0 0 lazy_used_qld"]);
        write_archive_without_index(
            &work.join("libir.a"),
            &[("c.bc", &work.join("c.bc")), ("d.bc", &work.join("d.bc"))],
        );
        let plugin_arg = plugin.to_string_lossy().into_owned();
        let args = [
            "-static",
            "-e",
            "start_qld",
            "-u",
            "forced_qld",
            "-plugin",
            &plugin_arg,
            "-plugin-opt=add-file=generated.o",
            "-o",
            "out",
            "native.o",
            "a.bc",
            "b.bc",
            "libir.a",
        ];
        let result = link(&work, &args);
        assert!(result.success, "{}", result.stderr);

        let a = |name| result.resolution("a.bc@0", name);
        assert_eq!(a("start_qld"), PREVAILING_DEF, "entry point");
        assert_eq!(a("used_by_native_qld"), PREVAILING_DEF, "native reference");
        assert_eq!(a("forced_qld"), PREVAILING_DEF, "-u");
        assert_eq!(a("ir_only_qld"), PREVAILING_DEF_IRONLY);
        assert_eq!(a("hidden_def_qld"), PREVAILING_DEF_IRONLY);
        assert_eq!(a("common_qld"), PREVAILING_DEF_IRONLY);
        assert_eq!(a("dup_weak_qld"), PREVAILING_DEF_IRONLY);
        assert_eq!(a("inline_fn_qld"), PREVAILING_DEF_IRONLY);
        assert_eq!(
            a("preempted_qld"),
            PREEMPTED_REG,
            "strong native definition"
        );
        assert_eq!(
            a("group_member_qld"),
            PREEMPTED_REG,
            "native COMDAT copy kept"
        );
        assert_eq!(a("native_func_qld"), RESOLVED_EXEC);
        assert_eq!(a("missing_weak_qld"), UNDEF);
        assert_eq!(a("lazy_used_qld"), RESOLVED_IR);

        let b = |name| result.resolution("b.bc@0", name);
        assert_eq!(b("ir_only_qld"), RESOLVED_IR);
        assert_eq!(b("dup_weak_qld"), PREEMPTED_IR);
        assert_eq!(b("inline_fn_qld"), PREEMPTED_IR, "IR COMDAT copy kept");
        assert_eq!(b("start_qld"), RESOLVED_IR);

        // The index-less archive: both members were claimed to learn their
        // symbols (known_used=0); d.bc was extracted and claimed again.
        let notes = result.notes();
        let archive = |prefix: &str, needle: &str| {
            notes
                .iter()
                .filter(|n| n.starts_with(prefix) && n.contains("libir.a@") && n.contains(needle))
                .count()
        };
        assert_eq!(archive("claim ", "known_used=0 claimed=1"), 2, "{notes:?}");
        assert_eq!(archive("claim ", "known_used=1 claimed=1"), 1, "{notes:?}");
        // c.bc, and d.bc's first claim, are not included (NO_SYMS); d.bc's
        // second claim is.
        assert_eq!(archive("resolve ", "status=1"), 2, "{notes:?}");
        let included = format!("status=0 lazy_used_qld={PREVAILING_DEF_IRONLY}");
        assert_eq!(archive("resolve ", &included), 1, "{notes:?}");
        assert!(
            notes
                .iter()
                .any(|n| n.starts_with("new-input ") && n.ends_with("generated.o"))
        );
        assert!(notes.contains(&"cleanup"), "{notes:?}");

        // The same link on one thread and on many: same claims, same
        // resolutions, same output.
        let first = std::fs::read(work.join("out")).unwrap();
        for threads in ["--threads=1", "--threads=8"] {
            let mut args = args.to_vec();
            args.push(threads);
            let again = link(&work, &args);
            assert!(again.success, "{}", again.stderr);
            assert_eq!(again.notes(), result.notes(), "{threads}");
            assert_eq!(std::fs::read(work.join("out")).unwrap(), first, "{threads}");
        }
    }

    #[test]
    #[cfg(feature = "plugin")]
    fn stub_member_needed_only_after_lto_is_an_error() {
        let Some(cc) = cc() else { return };
        let work = work_dir("after-lto");
        let plugin = stub_plugin(&cc, &work);
        assemble(
            &cc,
            &work,
            "generated",
            ".text\n.globl start_qld\nstart_qld: call lazy_unused_qld\nret\n",
        );
        write_ir(&work.join("a.bc"), &["0 0 start_qld"]);
        write_ir(&work.join("c.bc"), &["0 0 lazy_unused_qld"]);
        write_archive_without_index(&work.join("libir.a"), &[("c.bc", &work.join("c.bc"))]);
        let plugin = format!("-plugin={}", plugin.display());
        let result = link(
            &work,
            &[
                "-static",
                "-e",
                "start_qld",
                &plugin,
                "-plugin-opt=add-file=generated.o",
                "-o",
                "out",
                "a.bc",
                "libir.a",
            ],
        );
        assert!(!result.success);
        assert!(
            result.stderr.contains("libir.a(c.bc)") && result.stderr.contains("not part of LTO"),
            "{}",
            result.stderr
        );
    }

    #[test]
    #[cfg(feature = "plugin")]
    fn stub_shared_and_relocatable_resolutions() {
        let Some(cc) = cc() else { return };
        let work = work_dir("shared");
        let plugin = stub_plugin(&cc, &work);
        assemble(&cc, &work, "generated", ".text\n");
        write_ir(
            &work.join("s.bc"),
            &[
                "0 0 exported_qld",
                "0 1 protected_qld",
                "0 3 hidden_qld",
                "0 0 script_local_qld",
            ],
        );
        std::fs::write(
            work.join("lib.map"),
            "VERS_1 { global: *; local: script_local_qld; };\n",
        )
        .unwrap();
        let plugin = format!("-plugin={}", plugin.display());
        let shared = link(
            &work,
            &[
                "-shared",
                "--version-script=lib.map",
                &plugin,
                "-plugin-opt=add-file=generated.o",
                "-plugin-opt=output-kind",
                "-o",
                "libs.so",
                "s.bc",
            ],
        );
        assert!(shared.success, "{}", shared.stderr);
        assert!(shared.notes().contains(&"onload output-kind=2"));
        let s = |name| shared.resolution("s.bc@0", name);
        assert_eq!(s("exported_qld"), PREVAILING_DEF_IRONLY_EXP);
        assert_eq!(s("protected_qld"), PREVAILING_DEF_IRONLY_EXP);
        assert_eq!(s("hidden_qld"), PREVAILING_DEF_IRONLY);
        assert_eq!(s("script_local_qld"), PREVAILING_DEF_IRONLY);

        let relocatable = link(
            &work,
            &[
                "-r",
                &plugin,
                "-plugin-opt=add-file=generated.o",
                "-plugin-opt=output-kind",
                "-o",
                "r.o",
                "s.bc",
            ],
        );
        assert!(relocatable.success, "{}", relocatable.stderr);
        assert!(relocatable.notes().contains(&"onload output-kind=0"));
        for name in [
            "exported_qld",
            "protected_qld",
            "hidden_qld",
            "script_local_qld",
        ] {
            assert_eq!(
                relocatable.resolution("s.bc@0", name),
                PREVAILING_DEF,
                "{name}"
            );
        }
    }

    #[test]
    #[cfg(feature = "plugin")]
    fn stub_dynamic_executable_resolutions() {
        let Some(cc) = cc() else { return };
        let work = work_dir("dynamic");
        let plugin = stub_plugin(&cc, &work);
        std::fs::write(
            work.join("dso.c"),
            "extern int dso_needs_qld(void);\nint dso_func_qld(void) { return dso_needs_qld(); }\n\
             int also_in_dso_qld(void) { return 3; }\n",
        )
        .unwrap();
        run_ok(
            Command::new(&cc)
                .args(["-shared", "-fPIC", "-nostdlib", "-o", "libdso.so", "dso.c"])
                .current_dir(&work),
        );
        // Two --as-needed libraries only IR refers to: one unversioned, one
        // with a version script.
        std::fs::write(
            work.join("asn.c"),
            "int asn_plain_qld(void) { return 1; }\n",
        )
        .unwrap();
        std::fs::write(
            work.join("asv.c"),
            "int asv_versioned_qld(void) { return 2; }\n",
        )
        .unwrap();
        std::fs::write(work.join("asv.map"), "ASV_1 { global: *; };\n").unwrap();
        run_ok(
            Command::new(&cc)
                .args(["-shared", "-fPIC", "-nostdlib", "-o", "libasn.so", "asn.c"])
                .current_dir(&work),
        );
        run_ok(
            Command::new(&cc)
                .args([
                    "-shared",
                    "-fPIC",
                    "-nostdlib",
                    "-Wl,--version-script=asv.map",
                ])
                .args(["-o", "libasv.so", "asv.c"])
                .current_dir(&work),
        );
        std::fs::create_dir_all(work.join("extra")).unwrap();
        std::fs::copy(work.join("libdso.so"), work.join("extra/libextra_qld.so")).unwrap();
        assemble(
            &cc,
            &work,
            "generated",
            ".text\n.globl start_qld\nstart_qld: ret\n.globl dso_needs_qld\ndso_needs_qld: ret\n",
        );
        write_ir(
            &work.join("e.bc"),
            &[
                "0 0 start_qld",
                "0 0 dso_needs_qld",
                "2 0 dso_func_qld",
                "0 0 plain_qld",
                "0 3 hidden_needed_qld",
                "0 0 also_in_dso_qld",
                "2 0 asn_plain_qld",
                "2 0 asv_versioned_qld",
            ],
        );
        let plugin = format!("-plugin={}", plugin.display());
        let base = [
            "-e",
            "start_qld",
            "-dynamic-linker",
            "/lib64/ld-linux-x86-64.so.2",
            plugin.as_str(),
            "-plugin-opt=add-file=generated.o",
            "-plugin-opt=library-path=extra",
            "-plugin-opt=add-library=extra_qld",
            "-plugin-opt=add-library=dso",
            "-plugin-opt=add-library=does_not_exist_qld",
            "-o",
            "out",
            "e.bc",
            "-L.",
            "--as-needed",
            "-lasn",
            "-lasv",
            // Libraries the plugin adds take the state of the last input.
            "--no-as-needed",
            "-ldso",
        ];
        let result = link(&work, &base);
        assert!(result.success, "{}", result.stderr);
        let e = |name| result.resolution("e.bc@0", name);
        assert_eq!(e("start_qld"), PREVAILING_DEF);
        assert_eq!(
            e("dso_needs_qld"),
            PREVAILING_DEF_IRONLY_EXP,
            "referenced by libdso.so"
        );
        assert_eq!(e("dso_func_qld"), RESOLVED_DYN);
        assert_eq!(e("plain_qld"), PREVAILING_DEF_IRONLY);
        assert_eq!(e("hidden_needed_qld"), PREVAILING_DEF_IRONLY);
        // Exported so that the library binds to the executable's copy.
        assert_eq!(e("also_in_dso_qld"), PREVAILING_DEF_IRONLY_EXP);
        // As in GNU ld, an IR reference keeps an --as-needed library for an
        // unversioned symbol, but not for a versioned one.
        assert_eq!(e("asn_plain_qld"), RESOLVED_DYN);
        assert_eq!(e("asv_versioned_qld"), UNDEF);
        // The library the plugin added, found in its directory, is linked;
        // libdso.so is not added twice; a library not found is skipped.
        let output = std::fs::read(work.join("out")).unwrap();
        let contains = |needle: &[u8]| output.windows(needle.len()).any(|w| w == needle);
        assert!(contains(b"libextra_qld.so\0"));

        let mut exported = base.to_vec();
        exported.push("--export-dynamic");
        let result = link(&work, &exported);
        assert!(result.success, "{}", result.stderr);
        assert_eq!(
            result.resolution("e.bc@0", "plain_qld"),
            PREVAILING_DEF_IRONLY_EXP
        );
        assert_eq!(
            result.resolution("e.bc@0", "hidden_needed_qld"),
            PREVAILING_DEF_IRONLY
        );
    }

    #[test]
    #[cfg(feature = "plugin")]
    fn stub_wrap_resolutions() {
        let Some(cc) = cc() else { return };
        let work = work_dir("wrap");
        let plugin = stub_plugin(&cc, &work);
        assemble(
            &cc,
            &work,
            "generated",
            ".text\n.globl start_qld\nstart_qld: ret\n",
        );
        write_ir(
            &work.join("w.bc"),
            &[
                "0 0 start_qld",
                "0 0 wrapped_qld",
                "0 0 __wrap_wrapped_qld",
                "0 0 unrelated_qld",
            ],
        );
        write_ir(
            &work.join("u.bc"),
            &["2 0 wrapped_qld", "2 0 __real_wrapped_qld"],
        );
        let plugin = format!("-plugin={}", plugin.display());
        let result = link(
            &work,
            &[
                "-static",
                "-e",
                "start_qld",
                "--wrap=wrapped_qld",
                &plugin,
                "-plugin-opt=add-file=generated.o",
                "--unresolved-symbols=ignore-all",
                "-o",
                "out",
                "w.bc",
                "u.bc",
            ],
        );
        assert!(result.success, "{}", result.stderr);
        assert!(result.notes().contains(&"wrap wrapped_qld"));
        let w = |name| result.resolution("w.bc@0", name);
        assert_eq!(w("wrapped_qld"), PREVAILING_DEF);
        assert_eq!(w("__wrap_wrapped_qld"), PREVAILING_DEF);
        assert_eq!(w("unrelated_qld"), PREVAILING_DEF_IRONLY);
        // u.bc's references were redirected: to the wrapper, and to the
        // real symbol.
        assert_eq!(result.resolution("u.bc@0", "wrapped_qld"), RESOLVED_IR);
        assert_eq!(
            result.resolution("u.bc@0", "__real_wrapped_qld"),
            RESOLVED_IR
        );
    }

    #[test]
    #[cfg(feature = "plugin")]
    fn stub_plugin_errors_fail_the_link() {
        let Some(cc) = cc() else { return };
        let work = work_dir("errors");
        let plugin = stub_plugin(&cc, &work);
        write_ir(&work.join("a.bc"), &["0 0 start_qld"]);
        let plugin = format!("-plugin={}", plugin.display());
        let result = link(
            &work,
            &[
                "-e",
                "start_qld",
                &plugin,
                "-plugin-opt=error",
                "-o",
                "out",
                "a.bc",
            ],
        );
        assert!(!result.success);
        assert!(
            result
                .stderr
                .contains("code generation failed in the stub plugin"),
            "{}",
            result.stderr
        );

        // Bitcode the plugin does not claim.
        std::fs::write(work.join("other.bc"), b"BC\xc0\xde not for this plugin").unwrap();
        let result = link(
            &work,
            &["-e", "start_qld", &plugin, "-o", "out", "a.bc", "other.bc"],
        );
        assert!(!result.success);
        assert!(
            result
                .stderr
                .contains("other.bc: no LTO plugin claimed this LLVM bitcode input"),
            "{}",
            result.stderr
        );
    }

    #[test]
    fn no_plugin_ir_input_is_an_error_naming_the_file() {
        let work = work_dir("no-plugin");
        std::fs::write(work.join("fake.o"), b"BC\xc0\xde\x35\x14\x00\x00").unwrap();
        let result = link(&work, &["-o", "out", "fake.o"]);
        assert!(!result.success);
        assert!(
            result
                .stderr
                .contains("fake.o: LLVM bitcode input needs an LTO plugin"),
            "{}",
            result.stderr
        );
    }

    #[test]
    fn no_plugin_gcc_slim_object_is_an_error_naming_the_file() {
        let Some(gcc) = find_program("gcc") else {
            skip("gcc not found");
            return;
        };
        let work = work_dir("no-plugin-gcc");
        std::fs::write(work.join("slim.c"), "int slim_qld(void) { return 1; }\n").unwrap();
        let compiled = Command::new(&gcc)
            .args(["-flto", "-c", "slim.c"])
            .current_dir(&work)
            .output()
            .unwrap();
        if !compiled.status.success() || !is_gcc_slim(&work.join("slim.o")) {
            skip("gcc does not produce slim LTO objects");
            return;
        }
        let result = link(&work, &["-e", "slim_qld", "-o", "out", "slim.o"]);
        assert!(!result.success);
        assert!(
            result
                .stderr
                .contains("slim.o: GCC LTO input needs an LTO plugin"),
            "{}",
            result.stderr
        );
    }

    fn is_gcc_slim(path: &Path) -> bool {
        let data = std::fs::read(path).unwrap_or_default();
        data.windows(14).any(|w| w == b"__gnu_lto_slim")
    }

    #[test]
    fn no_plugin_loaded_without_ir_inputs() {
        let Some(cc) = cc() else { return };
        let work = work_dir("no-ir");
        let plugin = stub_plugin(&cc, &work);
        assemble(
            &cc,
            &work,
            "native",
            ".text\n.globl start_qld\nstart_qld: ret\n.data\n.globl value_qld\nvalue_qld: .quad start_qld\n",
        );
        let plain = link(&work, &["-e", "start_qld", "-o", "plain", "native.o"]);
        assert!(plain.success, "{}", plain.stderr);
        // A plugin that exists (and would report onload) and one that does
        // not: neither is loaded when nothing needs it, and the output is
        // the same byte for byte.
        let plugin = format!("-plugin={}", plugin.display());
        for (name, args) in [
            ("stub", vec![plugin.as_str(), "-plugin-opt=output-kind"]),
            ("missing", vec!["-plugin", "/nonexistent/qld/plugin.so"]),
        ] {
            let mut all = vec!["-e", "start_qld", "-o", name, "native.o"];
            all.extend(args);
            let result = link(&work, &all);
            assert!(result.success, "{}", result.stderr);
            assert!(result.notes().is_empty(), "{}", result.stderr);
            assert_eq!(
                std::fs::read(work.join(name)).unwrap(),
                std::fs::read(work.join("plain")).unwrap(),
                "{name}"
            );
        }
    }
}
