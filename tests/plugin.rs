//! Integration tests for the LTO plugin host (`qld::plugin`, workstream W17).
//!
//! The plugin interface allows one session per process, and real plugins
//! can be used once per process, so every test runs its plugin work in a
//! child process: the test re-runs this test binary filtered to itself, with
//! `QLD_PLUGIN_TEST_CHILD` naming the scenario. The parent compiles inputs
//! and checks the child's exit status.
//!
//! - `test_plugin_*`: `tests/data/plugin/test_plugin.c`, a small plugin
//!   built with the host C compiler, exercises every callback, including
//!   invalid calls.
//! - `llvm_*`: `LLVMgold.so` with the matching clang (every installed
//!   version), full LTO and ThinLTO.
//! - `gcc_*`: `liblto_plugin.so` with the options `collect2` passes, captured
//!   from `gcc -flto -v`.
//!
//! Missing tools make a test print `SKIPPED:` and pass.

#![cfg(feature = "plugin")]

#[cfg(target_os = "linux")]
mod common;

#[cfg(not(unix))]
#[test]
fn plugins_are_unimplemented_on_this_host() {
    use qld::diag::Collect;
    use qld::plugin::{Session, SessionOptions};

    let diagnostics = Collect::new();
    let mut session = Session::new(SessionOptions::default()).unwrap();
    let error = session
        .load_plugin("LLVMgold.dll".as_ref(), &[], &diagnostics)
        .unwrap_err();
    assert!(matches!(error, qld::Error::Unimplemented(_)), "{error}");
}

#[cfg(target_os = "linux")]
mod linux {
    use std::collections::{BTreeMap, BTreeSet};
    use std::ffi::OsString;
    use std::fmt::Write as _;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use qld::Severity;
    use std::sync::Mutex;

    use qld::diag::{Diagnostic, DiagnosticSink};
    use qld::elf::read::consts::{STB_LOCAL, STV_DEFAULT};
    use qld::elf::read::{Elf64Le, ObjectFile, SectionIndex, Source};
    use qld::plugin::{
        ClaimedFile, FileResolution, InputFile, MessageLevel, OutputKind, SectionKind, Session,
        SessionOptions, SymbolKind, SymbolResolution, SymbolType, Visibility,
    };

    use super::common::tools::{find_program, skip};

    const CHILD: &str = "QLD_PLUGIN_TEST_CHILD";
    const WORK: &str = "QLD_PLUGIN_TEST_WORK";

    // -----------------------------------------------------------------------
    // Harness
    // -----------------------------------------------------------------------

    /// A diagnostic sink that keeps everything, for inspection.
    #[derive(Default)]
    struct Log {
        entries: Mutex<Vec<Diagnostic>>,
    }

    impl Log {
        fn new() -> Self {
            Self::default()
        }

        fn diagnostics(&self) -> Vec<Diagnostic> {
            self.entries.lock().unwrap().clone()
        }
    }

    impl DiagnosticSink for Log {
        fn emit(&self, diagnostic: Diagnostic) {
            self.entries.lock().unwrap().push(diagnostic);
        }

        fn error_count(&self) -> usize {
            self.diagnostics()
                .iter()
                .filter(|d| d.severity == Severity::Error)
                .count()
        }
    }

    /// Whether this process is the child for `scenario`.
    fn is_child(scenario: &str) -> bool {
        std::env::var(CHILD).is_ok_and(|value| value == scenario)
    }

    /// The child's work directory.
    fn child_work() -> PathBuf {
        PathBuf::from(std::env::var_os(WORK).expect("child without a work directory"))
    }

    fn data(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/data/plugin")
            .join(name)
    }

    /// A fresh work directory for one scenario.
    fn work_dir(name: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("plugin")
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Runs test `test` in a child process as `scenario`, and returns its
    /// output.
    fn run_child(test: &str, scenario: &str, work: &Path, env: &[(&str, OsString)]) -> String {
        let exe = std::env::current_exe().unwrap();
        let output = Command::new(exe)
            .args([
                &format!("linux::{test}"),
                "--exact",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD, scenario)
            .env(WORK, work)
            .envs(env.iter().map(|(k, v)| (k, v)))
            .current_dir(work)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr);
        println!("--- child {scenario} ---\n{stdout}{stderr}");
        assert!(
            output.status.success(),
            "child {scenario} failed ({})",
            output.status
        );
        assert!(
            stdout.contains(&format!("test linux::{test} ..."))
                && stdout.contains("test result: ok. 1 passed"),
            "child {scenario} did not run the test"
        );
        stdout
    }

    fn run(command: &mut Command) -> std::process::Output {
        let output = command
            .output()
            .unwrap_or_else(|e| panic!("{command:?}: {e}"));
        assert!(
            output.status.success(),
            "{command:?} failed:\n{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn infos(diagnostics: &Log) -> Vec<String> {
        diagnostics
            .diagnostics()
            .into_iter()
            .map(|d| format!("{}: {}", d.severity, d.message))
            .collect()
    }

    fn input(path: &Path, handle: u64) -> InputFile {
        let size = std::fs::metadata(path).unwrap().len();
        InputFile::new(path, 0, size, handle)
    }

    // -----------------------------------------------------------------------
    // Resolution and object checks shared by the real-plugin tests
    // -----------------------------------------------------------------------

    /// Global symbols of a native object: (defined, referenced).
    fn native_symbols(path: &Path) -> (BTreeSet<Vec<u8>>, BTreeSet<Vec<u8>>) {
        let bytes = std::fs::read(path).unwrap();
        let object = ObjectFile::<Elf64Le>::parse(&bytes, Source::new(path)).unwrap();
        let mut defined = BTreeSet::new();
        let mut referenced = BTreeSet::new();
        for symbol in object.symbols().globals() {
            let symbol = symbol.unwrap();
            if symbol.section == SectionIndex::Undefined {
                referenced.insert(symbol.name.to_vec());
            } else {
                defined.insert(symbol.name.to_vec());
            }
        }
        (defined, referenced)
    }

    /// Resolves claimed IR symbols against one native object the way a
    /// linker would for this small program: the first IR definition of a name
    /// prevails unless the native object defines it; it is IR-only unless the
    /// native object refers to it; references resolve to IR, the native
    /// object, or (for libc) a shared library.
    fn resolve(files: &[ClaimedFile], native: &Path) -> Vec<Vec<SymbolResolution>> {
        let (native_defined, native_referenced) = native_symbols(native);
        let mut prevailing: BTreeMap<&[u8], (usize, usize)> = BTreeMap::new();
        for (f, file) in files.iter().enumerate() {
            for (s, symbol) in file.symbols.iter().enumerate() {
                if !symbol.kind.is_undefined() {
                    prevailing.entry(&symbol.name).or_insert((f, s));
                }
            }
        }
        files
            .iter()
            .enumerate()
            .map(|(f, file)| {
                file.symbols
                    .iter()
                    .enumerate()
                    .map(|(s, symbol)| {
                        let name = symbol.name.as_slice();
                        if !symbol.kind.is_undefined() {
                            if native_defined.contains(name) {
                                SymbolResolution::PreemptedRegular
                            } else if prevailing.get(name) != Some(&(f, s)) {
                                SymbolResolution::PreemptedIr
                            } else if native_referenced.contains(name) {
                                SymbolResolution::PrevailingDef
                            } else {
                                SymbolResolution::PrevailingDefIronly
                            }
                        } else if prevailing.contains_key(name) {
                            SymbolResolution::ResolvedIr
                        } else if native_defined.contains(name) {
                            SymbolResolution::ResolvedExec
                        } else if symbol.kind == SymbolKind::WeakUndefined {
                            SymbolResolution::Undefined
                        } else {
                            SymbolResolution::ResolvedDyn
                        }
                    })
                    .collect()
            })
            .collect()
    }

    /// One symbol as the LTO output defines it.
    #[derive(Debug, Default)]
    struct OutputSymbol {
        defined: bool,
        exported: bool,
    }

    /// Reads the symbols of the plugin's native objects, merged by name.
    fn output_symbols(files: &[PathBuf]) -> BTreeMap<Vec<u8>, OutputSymbol> {
        let mut symbols: BTreeMap<Vec<u8>, OutputSymbol> = BTreeMap::new();
        for path in files {
            let bytes = std::fs::read(path).unwrap();
            let object = ObjectFile::<Elf64Le>::parse(&bytes, Source::new(path))
                .unwrap_or_else(|e| panic!("LTO output is not a valid object: {e}"));
            for symbol in object.symbols().iter().skip(1) {
                let symbol = symbol.unwrap();
                if symbol.name.is_empty() {
                    continue;
                }
                let entry = symbol_entry(&mut symbols, symbol.name);
                let defined = symbol.section != SectionIndex::Undefined;
                entry.defined |= defined;
                entry.exported |=
                    defined && symbol.binding() != STB_LOCAL && symbol.visibility() == STV_DEFAULT;
            }
        }
        symbols
    }

    fn symbol_entry<'m>(
        symbols: &'m mut BTreeMap<Vec<u8>, OutputSymbol>,
        name: &[u8],
    ) -> &'m mut OutputSymbol {
        symbols.entry(name.to_vec()).or_default()
    }

    /// Checks what the program's LTO objects export: the symbols `main.c`
    /// uses are exported definitions, the IR-only `internal` ones are not
    /// exported (removed, local or hidden), and `native_value` stays a
    /// reference.
    fn check_output(files: &[PathBuf], internal: &[&str]) {
        let symbols = output_symbols(files);
        let names: Vec<String> = symbols
            .iter()
            .map(|(name, s)| format!("{} {:?}", String::from_utf8_lossy(name), s))
            .collect();
        let get = |name: &str| symbols.get(name.as_bytes());
        for name in ["api", "global_data", "weak_fn", "common_var"] {
            assert!(
                get(name).is_some_and(|s| s.exported),
                "{name} should be exported: {names:#?}"
            );
        }
        for &name in internal {
            assert!(
                !get(name).is_some_and(|s| s.exported),
                "IR-only {name} should be internalized: {names:#?}"
            );
        }
        assert!(
            get("native_value").is_some_and(|s| !s.defined),
            "native_value should stay undefined: {names:#?}"
        );
    }

    /// Links `main.o` with the LTO objects using GNU ld and runs the result.
    fn link_and_run(cc: &Path, work: &Path, main: &Path, objects: &[PathBuf]) {
        let program = work.join("prog");
        let mut command = Command::new(cc);
        command
            .args(["-fuse-ld=bfd", "-fno-lto", "-pie", "-o"])
            .arg(&program)
            .arg(main)
            .args(objects);
        run(&mut command);
        let output = run(&mut Command::new(&program));
        assert_eq!(String::from_utf8_lossy(&output.stdout), "174\n");
    }

    /// Copies the plugin's objects into `work`, since cleanup deletes them.
    fn keep_objects(files: &[PathBuf], work: &Path) -> Vec<PathBuf> {
        files
            .iter()
            .enumerate()
            .map(|(i, file)| {
                let copy = work.join(format!("lto-output-{i}.o"));
                std::fs::copy(file, &copy).unwrap();
                copy
            })
            .collect()
    }

    /// A stable text rendering of claimed files and their resolutions.
    fn dump(files: &[ClaimedFile], resolutions: &[Vec<SymbolResolution>]) -> String {
        let mut out = String::new();
        for (file, resolutions) in files.iter().zip(resolutions) {
            writeln!(
                out,
                "{} @{} +{}",
                file.path.display(),
                file.offset,
                file.size
            )
            .unwrap();
            for (symbol, resolution) in file.symbols.iter().zip(resolutions) {
                let text = |bytes: &Option<Vec<u8>>| {
                    bytes
                        .as_ref()
                        .map(|b| String::from_utf8_lossy(b).into_owned())
                };
                writeln!(
                    out,
                    "  {} version={:?} {:?} {:?} size={} comdat={:?} {:?} {:?} -> {resolution:?}",
                    String::from_utf8_lossy(&symbol.name),
                    text(&symbol.version),
                    symbol.kind,
                    symbol.visibility,
                    symbol.size,
                    text(&symbol.comdat_key),
                    symbol.symbol_type,
                    symbol.section_kind,
                )
                .unwrap();
            }
        }
        out
    }

    // -----------------------------------------------------------------------
    // Errors and concurrency
    // -----------------------------------------------------------------------

    #[test]
    fn missing_plugin_is_a_clear_error() {
        const NAME: &str = "missing_plugin_is_a_clear_error";
        if !is_child(NAME) {
            run_child(NAME, NAME, &work_dir(NAME), &[]);
            return;
        }
        let diagnostics = Log::new();
        let mut session = Session::new(SessionOptions::default()).unwrap();
        let path = Path::new("/nonexistent/qld/LLVMgold.so");
        let error = session.load_plugin(path, &[], &diagnostics).unwrap_err();
        let text = error.to_string();
        assert!(matches!(error, qld::Error::Io { .. }), "{text}");
        assert!(text.contains("/nonexistent/qld/LLVMgold.so"), "{text}");
        assert!(text.contains("No such file"), "{text}");

        // A file that is not a shared library.
        let work = child_work();
        let bogus = work.join("bogus.so");
        std::fs::write(&bogus, b"not a plugin").unwrap();
        let error = session.load_plugin(&bogus, &[], &diagnostics).unwrap_err();
        assert!(error.to_string().contains("bogus.so"), "{error}");
    }

    #[test]
    fn second_session_is_rejected() {
        const NAME: &str = "second_session_is_rejected";
        if !is_child(NAME) {
            run_child(NAME, NAME, &work_dir(NAME), &[]);
            return;
        }
        let first = Session::new(SessionOptions::default()).unwrap();
        let error = Session::new(SessionOptions::default()).unwrap_err();
        assert!(matches!(error, qld::Error::Limit(_)), "{error}");
        assert!(
            error.to_string().contains("one LTO plugin session"),
            "{error}"
        );
        // From another thread too.
        let error = std::thread::spawn(|| Session::new(SessionOptions::default()).is_err())
            .join()
            .unwrap();
        assert!(error);
        drop(first);
        let again = Session::new(SessionOptions::default()).unwrap();
        again.finish(&Log::new()).unwrap();
    }

    // -----------------------------------------------------------------------
    // The test plugin
    // -----------------------------------------------------------------------

    /// Builds `test_plugin.so` in `work`, or returns `None` (after printing a
    /// skip line) when no C compiler is available.
    fn build_test_plugin(work: &Path) -> Option<PathBuf> {
        let Some(cc) = find_program("cc").or_else(|| find_program("gcc")) else {
            skip("no C compiler for the test plugin");
            return None;
        };
        let plugin = work.join("test_plugin.so");
        run(Command::new(cc)
            .args(["-shared", "-fPIC", "-O1", "-pthread", "-o"])
            .arg(&plugin)
            .arg(data("test_plugin.c")));
        // A native object for the section queries.
        let object = work.join("native.o");
        run(
            Command::new(find_program("cc").or_else(|| find_program("gcc")).unwrap())
                .args(["-c", "-O1", "-o"])
                .arg(&object)
                .arg(data("main.c")),
        );
        Some(plugin)
    }

    /// Copies the test plugin to a new file name: a separate library to the
    /// dynamic loader, so it can be used again in the same process.
    fn plugin_copy(work: &Path, name: &str) -> PathBuf {
        let copy = work.join(format!("{name}.so"));
        std::fs::copy(work.join("test_plugin.so"), &copy).unwrap();
        copy
    }

    #[test]
    fn test_plugin_protocol() {
        const NAME: &str = "test_plugin_protocol";
        if !is_child(NAME) {
            let work = work_dir(NAME);
            if build_test_plugin(&work).is_some() {
                run_child(NAME, NAME, &work, &[]);
            }
            return;
        }
        let work = child_work();
        let plugin = work.join("test_plugin.so");
        let ir1 = work.join("one.ir");
        std::fs::write(
            &ir1,
            "QLDTEST\n0 0 exported\n0 3 hidden_def\n1 1 weak_def\n2 0 undef\n3 0 weak_undef\n4 0 common key\n",
        )
        .unwrap();
        let ir2 = work.join("two.ir");
        std::fs::write(&ir2, "QLDTEST\n0 0 unused_member\n").unwrap();
        let text = work.join("plain.txt");
        std::fs::write(&text, "not claimed").unwrap();
        let native = work.join("native.o");

        let diagnostics = Log::new();
        let mut session = Session::new(SessionOptions {
            output_kind: OutputKind::SharedObject,
            output_name: Some(work.join("libout.so")),
            wrap_symbols: vec![b"malloc".to_vec(), b"free".to_vec()],
            ..SessionOptions::default()
        })
        .unwrap();
        let options: Vec<String> = ["negotiate", "bad-calls", "sections", "add-file=/tmp/x.o"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        session
            .load_plugin(&plugin, &options, &diagnostics)
            .unwrap();
        let plugins = session.plugins();
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].identifier.as_deref(), Some("qld-test"));
        assert_eq!(plugins[0].version.as_deref(), Some("1.0"));
        assert_eq!(plugins[0].api_level, Some(1));

        // Handles 10, 11, 12, 13 identify the files to the caller.
        assert!(
            session
                .claim(&input(&text, 10), &diagnostics)
                .unwrap()
                .is_none()
        );
        assert!(
            session
                .claim(&input(&native, 11), &diagnostics)
                .unwrap()
                .is_none()
        );
        let claimed = session
            .claim(&input(&ir1, 12), &diagnostics)
            .unwrap()
            .unwrap()
            .clone();
        assert_eq!(claimed.handle, 12);
        assert_eq!(claimed.plugin, 0);
        let kinds: Vec<_> = claimed
            .symbols
            .iter()
            .map(|s| {
                (
                    String::from_utf8_lossy(&s.name).into_owned(),
                    s.kind,
                    s.visibility,
                    s.comdat_key.clone(),
                    s.size,
                    s.symbol_type,
                    s.section_kind,
                )
            })
            .collect();
        let d = SymbolType::Unknown;
        let k = SectionKind::Default;
        assert_eq!(
            kinds,
            [
                (
                    "exported".into(),
                    SymbolKind::Definition,
                    Visibility::Default,
                    None,
                    0,
                    d,
                    k
                ),
                (
                    "hidden_def".into(),
                    SymbolKind::Definition,
                    Visibility::Hidden,
                    None,
                    0,
                    d,
                    k
                ),
                (
                    "weak_def".into(),
                    SymbolKind::WeakDefinition,
                    Visibility::Protected,
                    None,
                    0,
                    d,
                    k
                ),
                (
                    "undef".into(),
                    SymbolKind::Undefined,
                    Visibility::Default,
                    None,
                    0,
                    d,
                    k
                ),
                (
                    "weak_undef".into(),
                    SymbolKind::WeakUndefined,
                    Visibility::Default,
                    None,
                    0,
                    d,
                    k
                ),
                (
                    "common".into(),
                    SymbolKind::Common,
                    Visibility::Default,
                    Some(b"key".to_vec()),
                    8,
                    d,
                    k
                ),
            ]
        );
        assert!(
            session
                .claim(&input(&ir2, 13), &diagnostics)
                .unwrap()
                .is_some()
        );
        assert_eq!(session.claimed_files().len(), 2);

        let messages = infos(&diagnostics);
        let expect = |line: &str| {
            assert!(
                messages.iter().any(|m| m == line),
                "missing {line:?} in {messages:#?}"
            );
        };
        assert!(
            messages
                .iter()
                .any(|m| m.starts_with("note: onload output=2 ld=244 name=/")
                    && m.ends_with("libout.so"))
        );
        expect("note: api level 1 linker qld");
        expect("note: bad register(null) = 3");
        expect("note: bad add_symbols(-1) = 3");
        expect("note: bad add_symbols(null) = 3");
        expect("note: bad add_symbols(null name) = 3");
        expect("note: bad add_symbols(kind 9) = 3");
        expect("note: bad add_symbols(handle) = 2");
        expect("note: bad get_symbols(pending) = 2");
        expect("note: bad get_view(handle) = 2");
        expect("note: bad get_input_file(null) = 3");
        expect("note: bad release_input_file(handle) = 2");
        expect("note: bad add_input_file(null) = 3");
        expect("note: bad message(null) = 3");
        expect("note: format    42|ab |ff|z|%");
        expect(
            "note: sections /dev/null: status 3"
                .replace("/dev/null", &text.display().to_string())
                .as_str(),
        );
        assert!(
            messages
                .iter()
                .any(|m| m.starts_with("note: section ") && m.contains(" .text type 1 ")),
            "{messages:#?}"
        );
        expect("note: bad section_type(index) = 3");

        let output = session
            .all_symbols_read(
                |file| {
                    if file.handle == 13 {
                        return FileResolution::NotIncluded;
                    }
                    use SymbolResolution::*;
                    FileResolution::Included(vec![
                        PrevailingDef,
                        PrevailingDefIronly,
                        PrevailingDefIronlyExp,
                        ResolvedDyn,
                        Undefined,
                        PreemptedIr,
                    ])
                },
                &diagnostics,
            )
            .unwrap();
        assert_eq!(output.files, [PathBuf::from("/tmp/x.o")]);
        assert_eq!(output.libraries, [OsString::from("m")]);
        assert_eq!(output.library_paths, [PathBuf::from("/qld/test/lib")]);
        assert_eq!(output.errors, 0);

        let messages = infos(&diagnostics);
        let expect = |line: &str| {
            assert!(
                messages.iter().any(|m| m == line),
                "missing {line:?} in {messages:#?}"
            );
        };
        expect(&format!(
            "note: resolve {} status=0 exported=2 hidden_def=3 weak_def=9 undef=8 weak_undef=1 common=5",
            ir1.display()
        ));
        expect(&format!(
            "note: resolve {} status=1 unused_member=4",
            ir2.display()
        ));
        expect("note: too many get_symbols = 1");
        expect("note: wrap malloc");
        expect("note: wrap free");

        // Claiming is over.
        assert!(matches!(
            session.claim(&input(&ir1, 14), &diagnostics),
            Err(qld::Error::Internal(_))
        ));

        let segments = session
            .new_input(&input(&native, 20), &diagnostics)
            .unwrap();
        assert!(segments.is_empty());
        let messages = infos(&diagnostics);
        assert!(messages.contains(&format!("note: new input {}", native.display())));

        session.finish(&diagnostics).unwrap();
        assert!(infos(&diagnostics).contains(&"note: cleanup ran".to_owned()));

        // The same library cannot be used twice in one process.
        let mut session = Session::new(SessionOptions::default()).unwrap();
        let error = session.load_plugin(&plugin, &[], &diagnostics).unwrap_err();
        assert!(matches!(error, qld::Error::Limit(_)), "{error}");
    }

    #[test]
    fn test_plugin_variants() {
        const NAME: &str = "test_plugin_variants";
        if !is_child(NAME) {
            let work = work_dir(NAME);
            if build_test_plugin(&work).is_some() {
                run_child(NAME, NAME, &work, &[]);
            }
            return;
        }
        let work = child_work();
        let ir = work.join("one.ir");
        std::fs::write(&ir, "QLDTEST\n0 0 f\n4 0 c\n2 0 u\n").unwrap();
        let strings = |values: &[&str]| values.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let all_ironly_exp = |file: &ClaimedFile| {
            FileResolution::Included(vec![
                SymbolResolution::PrevailingDefIronlyExp;
                file.symbols.len()
            ])
        };

        // get_symbols version 1 hides IRONLY_EXP; version 2 reports it.
        for (version, expected) in [(1, 2), (2, 9)] {
            let diagnostics = Log::new();
            let mut session = Session::new(SessionOptions::default()).unwrap();
            let plugin = plugin_copy(&work, &format!("get-symbols-{version}"));
            let option = format!("get-symbols={version}");
            session
                .load_plugin(&plugin, &strings(&[&option]), &diagnostics)
                .unwrap();
            session
                .claim(&input(&ir, 1), &diagnostics)
                .unwrap()
                .unwrap();
            session
                .all_symbols_read(all_ironly_exp, &diagnostics)
                .unwrap();
            let line = format!(
                "note: resolve {} status=0 f={expected} c={expected} u={expected}",
                ir.display()
            );
            assert!(
                infos(&diagnostics).contains(&line),
                "{:#?}",
                infos(&diagnostics)
            );
            session.finish(&diagnostics).unwrap();
        }

        // Version 2 of add_symbols carries symbol types, and v2 claim
        // handlers learn whether the file is known to be used.
        {
            let diagnostics = Log::new();
            let mut session = Session::new(SessionOptions::default()).unwrap();
            let plugin = plugin_copy(&work, "v2");
            session
                .load_plugin(
                    &plugin,
                    &strings(&["claim-v2", "symbols-v2", "threads"]),
                    &diagnostics,
                )
                .unwrap();
            let mut file = input(&ir, 1);
            file.known_used = false;
            let claimed = session.claim(&file, &diagnostics).unwrap().unwrap();
            let types: Vec<_> = claimed
                .symbols
                .iter()
                .map(|s| (s.symbol_type, s.section_kind))
                .collect();
            assert_eq!(
                types,
                [
                    (SymbolType::Function, SectionKind::Default),
                    (SymbolType::Variable, SectionKind::Bss),
                    (SymbolType::Function, SectionKind::Default),
                ]
            );
            assert!(
                infos(&diagnostics)
                    .contains(&format!("note: claim_v2 {} known_used=0", ir.display())),
                "{:#?}",
                infos(&diagnostics)
            );
            session
                .all_symbols_read(all_ironly_exp, &diagnostics)
                .unwrap();
            assert!(
                infos(&diagnostics).contains(&"warning: warning from a plugin thread".to_owned())
            );
            // Dropping instead of finishing still runs cleanup.
            drop(session);
        }

        // A fatal message from onload is an error, and poisons the session.
        {
            let diagnostics = Log::new();
            let mut session = Session::new(SessionOptions::default()).unwrap();
            let plugin = plugin_copy(&work, "fatal");
            let error = session
                .load_plugin(&plugin, &strings(&["fatal-onload"]), &diagnostics)
                .unwrap_err();
            assert!(
                matches!(error, qld::Error::Reported { errors: 1 }),
                "{error}"
            );
            let fatal = diagnostics
                .diagnostics()
                .into_iter()
                .find(|d| d.severity == Severity::Error)
                .unwrap();
            assert_eq!(fatal.message, "fatal from qld-test (code 42)");
            let error = session.claim(&input(&ir, 1), &diagnostics).unwrap_err();
            assert!(matches!(error, qld::Error::Internal(_)), "{error}");
        }

        // The fatal hook sees the message first.
        {
            use std::sync::atomic::{AtomicUsize, Ordering};
            static SEEN: AtomicUsize = AtomicUsize::new(0);
            fn hook(message: &qld::plugin::PluginMessage) {
                assert_eq!(message.level, MessageLevel::Fatal);
                SEEN.fetch_add(1, Ordering::SeqCst);
            }
            let diagnostics = Log::new();
            let mut session = Session::new(SessionOptions {
                fatal_hook: Some(hook),
                ..SessionOptions::default()
            })
            .unwrap();
            let plugin = plugin_copy(&work, "fatal-hook");
            assert!(
                session
                    .load_plugin(&plugin, &strings(&["fatal-onload"]), &diagnostics)
                    .is_err()
            );
            assert_eq!(SEEN.load(Ordering::SeqCst), 1);
        }

        // An error status from onload.
        {
            let diagnostics = Log::new();
            let mut session = Session::new(SessionOptions::default()).unwrap();
            let plugin = plugin_copy(&work, "onload-error");
            let error = session
                .load_plugin(&plugin, &strings(&["onload-error"]), &diagnostics)
                .unwrap_err();
            assert!(matches!(error, qld::Error::Reported { .. }), "{error}");
            assert!(
                infos(&diagnostics)
                    .iter()
                    .any(|m| m.contains("plugin failed to load"))
            );
        }

        // Non-fatal errors are counted; a failing handler is an error.
        {
            let diagnostics = Log::new();
            let mut session = Session::new(SessionOptions::default()).unwrap();
            let plugin = plugin_copy(&work, "error-message");
            session
                .load_plugin(&plugin, &strings(&["error-message"]), &diagnostics)
                .unwrap();
            session.claim(&input(&ir, 1), &diagnostics).unwrap();
            let output = session
                .all_symbols_read(all_ironly_exp, &diagnostics)
                .unwrap();
            assert_eq!(output.errors, 1);
            assert!(infos(&diagnostics).contains(&"error: an error from the plugin".to_owned()));
        }
        {
            let diagnostics = Log::new();
            let mut session = Session::new(SessionOptions::default()).unwrap();
            let plugin = plugin_copy(&work, "asr-error");
            session
                .load_plugin(&plugin, &strings(&["asr-error"]), &diagnostics)
                .unwrap();
            session.claim(&input(&ir, 1), &diagnostics).unwrap();
            let error = session
                .all_symbols_read(all_ironly_exp, &diagnostics)
                .unwrap_err();
            assert!(matches!(error, qld::Error::Reported { .. }), "{error}");
        }

        // Resolution lists must match the symbol count.
        {
            let diagnostics = Log::new();
            let mut session = Session::new(SessionOptions::default()).unwrap();
            let plugin = plugin_copy(&work, "bad-resolutions");
            session.load_plugin(&plugin, &[], &diagnostics).unwrap();
            session.claim(&input(&ir, 1), &diagnostics).unwrap();
            let error = session
                .all_symbols_read(|_| FileResolution::Included(Vec::new()), &diagnostics)
                .unwrap_err();
            assert!(matches!(error, qld::Error::Internal(_)), "{error}");
        }

        // Files that do not exist cannot be claimed.
        {
            let diagnostics = Log::new();
            let mut session = Session::new(SessionOptions::default()).unwrap();
            let plugin = plugin_copy(&work, "missing-input");
            session.load_plugin(&plugin, &[], &diagnostics).unwrap();
            let error = session
                .claim(
                    &InputFile::new(work.join("missing.o"), 0, 10, 1),
                    &diagnostics,
                )
                .unwrap_err();
            assert!(matches!(error, qld::Error::Io { .. }), "{error}");
        }
    }

    // -----------------------------------------------------------------------
    // LLVM
    // -----------------------------------------------------------------------

    struct Llvm {
        version: u32,
        clang: PathBuf,
        nm: PathBuf,
        gold: PathBuf,
    }

    /// Installed LLVM versions with clang, llvm-nm and LLVMgold.so, in the
    /// Gentoo (`/usr/lib/llvm/N`) or Debian (`/usr/lib/llvm-N`) layout.
    fn llvm_toolchains() -> Vec<Llvm> {
        let mut found = Vec::new();
        for version in 14..=30 {
            for prefix in [
                format!("/usr/lib/llvm/{version}"),
                format!("/usr/lib/llvm-{version}"),
            ] {
                let prefix = PathBuf::from(prefix);
                let clang = prefix.join("bin/clang");
                let nm = prefix.join("bin/llvm-nm");
                let gold = ["lib64/LLVMgold.so", "lib/LLVMgold.so"]
                    .iter()
                    .map(|p| prefix.join(p))
                    .find(|p| p.is_file());
                if let (true, true, Some(gold)) = (clang.is_file(), nm.is_file(), gold) {
                    found.push(Llvm {
                        version,
                        clang,
                        nm,
                        gold,
                    });
                    break;
                }
            }
        }
        found
    }

    /// Compiles the IR files and the native `main.o` with `clang`.
    fn compile_llvm(llvm: &Llvm, work: &Path, lto: &str) {
        for (source, extra) in [
            ("lto_a.c", "-fcommon"),
            ("lto_b.cpp", ""),
            ("lto_c.cpp", ""),
        ] {
            let object = work.join(source.replace(['.'], "_") + ".o");
            let mut command = Command::new(&llvm.clang);
            command
                .args([lto, "-O2", "-fPIE", "-c", "-o"])
                .arg(&object)
                .arg(data(source));
            if !extra.is_empty() {
                command.arg(extra);
            }
            run(&mut command);
        }
        run(Command::new(&llvm.clang)
            .args(["-O2", "-fPIE", "-c", "-o"])
            .arg(work.join("main.o"))
            .arg(data("main.c")));
    }

    fn llvm_test(test: &str, thin: bool) {
        let toolchains = llvm_toolchains();
        if toolchains.is_empty() {
            skip("no clang with a matching LLVMgold.so");
            return;
        }
        if find_program("gcc").is_none() {
            skip("no gcc driver to run GNU ld");
            return;
        }
        for llvm in &toolchains {
            let scenario = format!("{test}-{}", llvm.version);
            let work = work_dir(&scenario);
            compile_llvm(llvm, &work, if thin { "-flto=thin" } else { "-flto" });
            run_child(
                test,
                &scenario,
                &work,
                &[
                    ("QLD_PLUGIN_TEST_GOLD", llvm.gold.clone().into()),
                    ("QLD_PLUGIN_TEST_NM", llvm.nm.clone().into()),
                ],
            );
        }
    }

    /// The child side of the LLVM tests.
    fn llvm_child(thin: bool) {
        let work = child_work();
        let gold = PathBuf::from(std::env::var_os("QLD_PLUGIN_TEST_GOLD").unwrap());
        let nm = PathBuf::from(std::env::var_os("QLD_PLUGIN_TEST_NM").unwrap());
        let diagnostics = Log::new();
        let mut session = Session::new(SessionOptions {
            output_kind: OutputKind::Pie,
            output_name: Some(work.join("prog")),
            ..SessionOptions::default()
        })
        .unwrap();
        let mut options = vec!["O2".to_owned(), "mcpu=x86-64".to_owned()];
        if thin {
            options.extend([
                "thinlto".to_owned(),
                "jobs=2".to_owned(),
                format!("cache-dir={}", work.join("cache").display()),
            ]);
        }
        session.load_plugin(&gold, &options, &diagnostics).unwrap();

        let objects = ["lto_a_c.o", "lto_b_cpp.o", "lto_c_cpp.o"].map(|name| work.join(name));
        for (handle, object) in objects.iter().enumerate() {
            let claimed = session
                .claim(&input(object, handle as u64), &diagnostics)
                .unwrap()
                .unwrap_or_else(|| panic!("{} not claimed", object.display()));
            check_against_nm(&nm, object, claimed);
        }
        let main = work.join("main.o");
        assert!(
            session
                .claim(&input(&main, 99), &diagnostics)
                .unwrap()
                .is_none()
        );

        let files = session.claimed_files().to_vec();
        let comdat = files[1]
            .symbols
            .iter()
            .find(|s| s.name == b"_Z13comdat_inlinei")
            .unwrap();
        assert!(comdat.comdat_key.is_some(), "{comdat:?}");
        let resolutions = resolve(&files, &main);
        std::fs::write(work.join("claimed.txt"), dump(&files, &resolutions)).unwrap();

        let mut remaining = resolutions.clone().into_iter();
        let output = session
            .all_symbols_read(
                |_| FileResolution::Included(remaining.next().unwrap()),
                &diagnostics,
            )
            .unwrap();
        assert_eq!(output.errors, 0, "{:#?}", infos(&diagnostics));
        assert!(
            !output.files.is_empty(),
            "no objects: {:#?}",
            infos(&diagnostics)
        );
        if thin {
            assert_eq!(output.files.len(), 3, "{:?}", output.files);
        }
        println!("LTO objects: {:?}", output.files);
        let kept = keep_objects(&output.files, &work);
        // ThinLTO compiles modules separately, so IR-only symbols used across
        // modules (`from_b`, `from_c`) stay global; the others are internal.
        let mut internal = vec!["ir_helper", "internal_data", "_Z13comdat_inlinei"];
        if !thin {
            internal.extend(["from_b", "from_c"]);
        }
        check_output(&kept, &internal);
        session.finish(&diagnostics).unwrap();

        link_and_run(&find_program("gcc").unwrap(), &work, &main, &kept);
    }

    /// Compares claimed symbols with `llvm-nm` on the bitcode.
    fn check_against_nm(nm: &Path, object: &Path, claimed: &ClaimedFile) {
        let output = run(Command::new(nm).arg("-P").arg(object));
        let mut expected = BTreeSet::new();
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let mut fields = line.split_whitespace();
            let (Some(name), Some(kind)) = (fields.next(), fields.next()) else {
                continue;
            };
            let kind = match kind {
                "U" => SymbolKind::Undefined,
                "w" | "v" => SymbolKind::WeakUndefined,
                "W" | "V" => SymbolKind::WeakDefinition,
                "C" => SymbolKind::Common,
                k if k.chars().all(|c| c.is_ascii_uppercase()) => SymbolKind::Definition,
                _ => continue,
            };
            expected.insert((name.to_owned(), kind));
        }
        let actual: BTreeSet<_> = claimed
            .symbols
            .iter()
            .map(|s| (String::from_utf8_lossy(&s.name).into_owned(), s.kind))
            .collect();
        assert_eq!(actual, expected, "{}", object.display());
    }

    #[test]
    fn llvm_full_lto() {
        const NAME: &str = "llvm_full_lto";
        if std::env::var(CHILD).is_ok_and(|s| s.starts_with(NAME)) {
            llvm_child(false);
        } else {
            llvm_test(NAME, false);
        }
    }

    #[test]
    fn llvm_thin_lto() {
        const NAME: &str = "llvm_thin_lto";
        if std::env::var(CHILD).is_ok_and(|s| s.starts_with(NAME)) {
            llvm_child(true);
        } else {
            llvm_test(NAME, true);
        }
    }

    #[test]
    fn llvm_fatal_option_is_an_error() {
        const NAME: &str = "llvm_fatal_option_is_an_error";
        if !is_child(NAME) {
            let Some(llvm) = llvm_toolchains().pop() else {
                skip("no LLVMgold.so");
                return;
            };
            run_child(
                NAME,
                NAME,
                &work_dir(NAME),
                &[("QLD_PLUGIN_TEST_GOLD", llvm.gold.into())],
            );
            return;
        }
        let gold = PathBuf::from(std::env::var_os("QLD_PLUGIN_TEST_GOLD").unwrap());
        let diagnostics = Log::new();
        let mut session = Session::new(SessionOptions::default()).unwrap();
        let error = session
            .load_plugin(&gold, &["O9".to_owned()], &diagnostics)
            .unwrap_err();
        assert!(matches!(error, qld::Error::Reported { .. }), "{error}");
        assert!(
            infos(&diagnostics)
                .iter()
                .any(|m| m.starts_with("error: ") && m.contains("Optimization level")),
            "{:#?}",
            infos(&diagnostics)
        );
    }

    #[test]
    fn claimed_symbols_are_deterministic() {
        const NAME: &str = "claimed_symbols_are_deterministic";
        if std::env::var(CHILD).is_ok_and(|s| s.starts_with(NAME)) {
            // Claim and resolve only: the dump is the result.
            let work = child_work();
            let gold = PathBuf::from(std::env::var_os("QLD_PLUGIN_TEST_GOLD").unwrap());
            let diagnostics = Log::new();
            let mut session = Session::new(SessionOptions::default()).unwrap();
            session.load_plugin(&gold, &[], &diagnostics).unwrap();
            let shared = work.parent().unwrap().join("inputs");
            for (handle, name) in ["lto_a_c.o", "lto_b_cpp.o", "lto_c_cpp.o"]
                .iter()
                .enumerate()
            {
                session
                    .claim(&input(&shared.join(name), handle as u64), &diagnostics)
                    .unwrap()
                    .unwrap();
            }
            let files = session.claimed_files().to_vec();
            let resolutions = resolve(&files, &shared.join("main.o"));
            std::fs::write(work.join("claimed.txt"), dump(&files, &resolutions)).unwrap();
            return;
        }
        let Some(llvm) = llvm_toolchains().pop() else {
            skip("no clang with a matching LLVMgold.so");
            return;
        };
        let inputs = work_dir(&format!("{NAME}/inputs"));
        compile_llvm(&llvm, &inputs, "-flto");
        let mut dumps = Vec::new();
        for run in ["first", "second"] {
            let scenario = format!("{NAME}/{run}");
            let work = work_dir(&scenario);
            run_child(
                NAME,
                &scenario,
                &work,
                &[("QLD_PLUGIN_TEST_GOLD", llvm.gold.clone().into())],
            );
            dumps.push(std::fs::read_to_string(work.join("claimed.txt")).unwrap());
        }
        assert!(dumps[0].contains("api"), "{}", dumps[0]);
        assert_eq!(dumps[0], dumps[1]);
    }

    // -----------------------------------------------------------------------
    // GCC
    // -----------------------------------------------------------------------

    /// What `collect2` would give the linker, captured from `gcc -v`.
    struct Collect2 {
        plugin: PathBuf,
        options: Vec<String>,
        collect_gcc: String,
        collect_gcc_options: String,
    }

    fn capture_collect2(stderr: &str) -> Option<Collect2> {
        let mut collect_gcc = None;
        let mut collect_gcc_options = None;
        for line in stderr.lines() {
            if let Some(value) = line.strip_prefix("COLLECT_GCC=") {
                collect_gcc = Some(value.to_owned());
            } else if let Some(value) = line.strip_prefix("COLLECT_GCC_OPTIONS=") {
                collect_gcc_options = Some(value.to_owned());
            } else if line.contains("collect2") && line.contains(" -plugin ") {
                let words: Vec<&str> = line.split_whitespace().collect();
                let plugin = words
                    .iter()
                    .position(|&w| w == "-plugin")
                    .and_then(|i| words.get(i + 1))?;
                let options = words
                    .iter()
                    .filter_map(|w| w.strip_prefix("-plugin-opt="))
                    .map(str::to_owned)
                    .collect();
                return Some(Collect2 {
                    plugin: PathBuf::from(plugin),
                    options,
                    collect_gcc: collect_gcc?,
                    collect_gcc_options: collect_gcc_options?,
                });
            }
        }
        None
    }

    #[test]
    fn gcc_lto() {
        const NAME: &str = "gcc_lto";
        if std::env::var(CHILD).is_ok_and(|s| s.starts_with(NAME)) {
            gcc_child();
            return;
        }
        let mut drivers: Vec<PathBuf> = (13..=16)
            .filter_map(|v| find_program(format!("gcc-{v}")))
            .collect();
        if drivers.is_empty() {
            drivers.extend(find_program("gcc"));
        }
        if drivers.is_empty() {
            skip("no gcc");
            return;
        }
        for gcc in drivers {
            let scenario = format!("{NAME}-{}", gcc.file_name().unwrap().to_string_lossy());
            let work = work_dir(&scenario);
            for (source, extra) in [
                ("lto_a.c", "-fcommon"),
                ("lto_b.cpp", "-O2"),
                ("lto_c.cpp", "-O2"),
            ] {
                let object = work.join(source.replace(['.'], "_") + ".o");
                run(Command::new(&gcc)
                    .args(["-flto", "-O2", "-fPIE", extra, "-c", "-o"])
                    .arg(&object)
                    .arg(data(source)));
            }
            run(Command::new(&gcc)
                .args(["-fno-lto", "-O2", "-fPIE", "-c", "-o"])
                .arg(work.join("main.o"))
                .arg(data("main.c")));

            // Link once through the driver to learn collect2's arguments.
            let probe = run(Command::new(&gcc)
                .current_dir(&work)
                .args(["-flto", "-O2", "-v", "-pie", "-fuse-ld=bfd", "-o", "probe"])
                .args(["lto_a_c.o", "lto_b_cpp.o", "lto_c_cpp.o", "main.o"]));
            let stderr = String::from_utf8_lossy(&probe.stderr);
            let Some(collect2) = capture_collect2(&stderr) else {
                skip(format!(
                    "{}: no plugin in collect2's command line",
                    gcc.display()
                ));
                continue;
            };
            let options: Vec<String> = collect2
                .options
                .iter()
                .map(|option| {
                    if option.starts_with("-fresolution=") {
                        format!("-fresolution={}", work.join("probe.res").display())
                    } else {
                        option.clone()
                    }
                })
                .collect();
            let gcc_options = collect2.collect_gcc_options.replace(" '-v'", "");
            println!("plugin {} options {options:?}", collect2.plugin.display());
            run_child(
                NAME,
                &scenario,
                &work,
                &[
                    ("COLLECT_GCC", collect2.collect_gcc.into()),
                    ("COLLECT_GCC_OPTIONS", gcc_options.into()),
                    ("QLD_PLUGIN_TEST_GCC", gcc.clone().into()),
                    ("QLD_PLUGIN_TEST_GCC_PLUGIN", collect2.plugin.into()),
                    ("QLD_PLUGIN_TEST_GCC_OPTIONS", options.join("\x1f").into()),
                ],
            );
        }
    }

    fn gcc_child() {
        let work = child_work();
        let var = |name: &str| std::env::var_os(name).unwrap();
        let gcc = PathBuf::from(var("QLD_PLUGIN_TEST_GCC"));
        let plugin = PathBuf::from(var("QLD_PLUGIN_TEST_GCC_PLUGIN"));
        let options: Vec<String> = var("QLD_PLUGIN_TEST_GCC_OPTIONS")
            .to_string_lossy()
            .split('\x1f')
            .map(str::to_owned)
            .collect();
        assert!(
            qld::plugin::options::missing_environment(
                qld::plugin::PluginFlavor::detect(&plugin, &options),
                &options,
                |name| std::env::var_os(name),
            )
            .is_empty()
        );

        let diagnostics = Log::new();
        let mut session = Session::new(SessionOptions {
            output_kind: OutputKind::Pie,
            output_name: Some(work.join("prog")),
            ..SessionOptions::default()
        })
        .unwrap();
        session
            .load_plugin(&plugin, &options, &diagnostics)
            .unwrap();
        println!("plugins: {:?}", session.plugins());

        let objects = ["lto_a_c.o", "lto_b_cpp.o", "lto_c_cpp.o"].map(|name| work.join(name));
        for (handle, object) in objects.iter().enumerate() {
            assert!(
                session
                    .claim(&input(object, handle as u64), &diagnostics)
                    .unwrap()
                    .is_some(),
                "{} not claimed: {:#?}",
                object.display(),
                infos(&diagnostics)
            );
        }
        let main = work.join("main.o");
        assert!(
            session
                .claim(&input(&main, 99), &diagnostics)
                .unwrap()
                .is_none()
        );

        let files = session.claimed_files().to_vec();
        let find = |file: usize, name: &str| {
            files[file]
                .symbols
                .iter()
                .find(|s| s.name == name.as_bytes())
                .unwrap_or_else(|| panic!("{name} not in {:#?}", files[file].symbols))
                .clone()
        };
        assert_eq!(find(0, "api").kind, SymbolKind::Definition);
        assert_eq!(find(0, "global_data").kind, SymbolKind::Definition);
        assert_eq!(find(0, "common_var").kind, SymbolKind::Common);
        assert_eq!(find(0, "weak_fn").kind, SymbolKind::WeakDefinition);
        assert_eq!(find(0, "native_value").kind, SymbolKind::Undefined);
        assert_eq!(find(0, "maybe_missing").kind, SymbolKind::WeakUndefined);
        let comdat = find(1, "_Z13comdat_inlinei");
        assert_eq!(comdat.kind, SymbolKind::WeakDefinition);
        assert!(comdat.comdat_key.is_some(), "{comdat:?}");
        if session.plugins()[0].api_level == Some(1) {
            // Negotiated plugins use add_symbols_v2, which carries types.
            assert_eq!(find(0, "api").symbol_type, SymbolType::Function);
            assert_eq!(find(0, "global_data").symbol_type, SymbolType::Variable);
        }

        let resolutions = resolve(&files, &main);
        let mut remaining = resolutions.into_iter();
        let output = session
            .all_symbols_read(
                |_| FileResolution::Included(remaining.next().unwrap()),
                &diagnostics,
            )
            .unwrap_or_else(|e| panic!("{e}: {:#?}", infos(&diagnostics)));
        assert_eq!(output.errors, 0, "{:#?}", infos(&diagnostics));
        assert!(
            !output.files.is_empty(),
            "no objects: {:#?}",
            infos(&diagnostics)
        );
        assert!(
            output.libraries.contains(&OsString::from("c")),
            "pass-through libraries: {:?}",
            output.libraries
        );
        println!("LTO objects: {:?}", output.files);
        let kept = keep_objects(&output.files, &work);
        check_output(
            &kept,
            &[
                "ir_helper",
                "internal_data",
                "from_b",
                "from_c",
                "_Z13comdat_inlinei",
            ],
        );
        session.finish(&diagnostics).unwrap();
        for file in &output.files {
            assert!(!file.exists(), "cleanup left {}", file.display());
        }

        link_and_run(&gcc, &work, &main, &kept);
    }
}
