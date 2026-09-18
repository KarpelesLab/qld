//! The library API (workstream W36): in-memory inputs and outputs, input
//! providers, cancellation, diagnostic sinks and caller-owned thread pools.
//!
//! The inputs are x86-64 ELF objects built in memory by
//! `examples/support/objects.rs`, so these tests need no toolchain. Running
//! the linked program is only possible on x86-64 Linux; elsewhere the tests
//! check the image without running it.

#[path = "../examples/support/objects.rs"]
mod objects;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use qld::args::{CancelToken, InputAttrs, InputKind, LinkOptions, OutputBuffer, OutputKind};
use qld::diag::{Collect, Diagnostic, DiagnosticSink, Severity};
use qld::input::source::{InputProvider, MemoryFiles};

/// A fresh, empty directory for one test.
fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("api-tests")
        .join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Static executable options linking `main.o` and `answer.o` from memory.
fn in_memory_options(value: u8) -> LinkOptions {
    let mut options = LinkOptions::new();
    options.kind = OutputKind::StaticExecutable;
    options.push_input(
        InputKind::bytes("main.o", objects::main_object()),
        InputAttrs::default(),
    );
    options.push_input(
        InputKind::bytes("answer.o", objects::answer_object(value)),
        InputAttrs::default(),
    );
    options
}

/// Links `options` into a new output buffer and returns the image.
fn link_to_memory(options: &mut LinkOptions) -> qld::Result<Vec<u8>> {
    let buffer = OutputBuffer::new();
    options.output_buffer = Some(buffer.clone());
    qld::link(options, &Collect::new())?;
    Ok(buffer.take().expect("a successful link fills the buffer"))
}

/// Writes `image` to `dir/name`, runs it where the host can, and returns its
/// exit status (`None` when it cannot run here).
fn run_image(dir: &Path, name: &str, image: &[u8]) -> Option<i32> {
    if !cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        return None;
    }
    let path = dir.join(name);
    std::fs::write(&path, image).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    std::process::Command::new(&path).status().unwrap().code()
}

#[test]
fn in_memory_objects_link_to_memory() {
    let dir = scratch("in-memory");
    let mut options = in_memory_options(42);
    // Only a name: nothing is written there.
    let output = dir.join("never-written");
    options.output = Some(output.clone());
    let image = link_to_memory(&mut options).unwrap();
    assert!(
        !output.exists(),
        "an output buffer replaces the output file"
    );
    assert_eq!(image.get(16..18), Some(&2u16.to_le_bytes()[..]), "ET_EXEC");
    assert!(objects::entry_point(&image).is_some_and(|entry| entry != 0));
    if let Some(status) = run_image(&dir, "answer", &image) {
        assert_eq!(status, 42);
    }
}

#[test]
fn memory_output_matches_the_file_output() {
    let dir = scratch("memory-matches-file");
    let mut options = in_memory_options(7);
    options.output = Some(dir.join("a.out"));
    qld::link(&options, &Collect::new()).unwrap();
    let file = std::fs::read(dir.join("a.out")).unwrap();
    let memory = link_to_memory(&mut options).unwrap();
    assert_eq!(file, memory);
}

#[test]
fn relocatable_output_to_memory() {
    let dir = scratch("relocatable");
    let mut options = in_memory_options(1);
    options.kind = OutputKind::Relocatable;
    options.output = Some(dir.join("combined.o"));
    let image = link_to_memory(&mut options).unwrap();
    assert!(!dir.join("combined.o").exists());
    assert_eq!(image.get(16..18), Some(&1u16.to_le_bytes()[..]), "ET_REL");

    // The combined object links on its own.
    let mut again = LinkOptions::new();
    again.kind = OutputKind::StaticExecutable;
    again.push_input(InputKind::bytes("combined.o", image), InputAttrs::default());
    let program = link_to_memory(&mut again).unwrap();
    if let Some(status) = run_image(&dir, "combined", &program) {
        assert_eq!(status, 1);
    }
}

#[test]
fn raw_binary_output_to_memory() {
    let dir = scratch("raw");
    let mut options = in_memory_options(3);
    options.output_format = Some(qld::args::OutputFormat::Binary);
    options.output = Some(dir.join("image.bin"));
    let image = link_to_memory(&mut options).unwrap();
    assert!(!dir.join("image.bin").exists());
    // `.text` of main.o, padding, then answer.o's `mov eax, 3; ret`.
    assert!(image.starts_with(&[0xe8]), "{image:02x?}");
    assert!(image.ends_with(&[0xb8, 3, 0, 0, 0, 0xc3]), "{image:02x?}");
}

#[test]
fn a_command_line_runs_against_memory_files() {
    let dir = scratch("argv-memory-files");
    let answer = objects::answer_object(9);
    let library = objects::archive(&[("answer.o", &answer, &["answer"])]);
    let files = MemoryFiles::new()
        .with("main.o", objects::main_object())
        .with("/virtual/lib/libanswer.a", library);
    let output = dir.join("prog");
    let argv = [
        "ld",
        "-static",
        "-o",
        output.to_str().unwrap(),
        "-L/virtual/lib",
        "main.o",
        "-lanswer",
    ];
    let qld::ParseOutcome::Link(options) = qld::parse_gnu(&argv).unwrap() else {
        panic!("not a link");
    };
    let mut options = *options;
    options.input_provider = Some(Arc::new(files));
    let image = link_to_memory(&mut options).unwrap();
    if let Some(status) = run_image(&dir, "prog", &image) {
        assert_eq!(status, 9);
    }
}

#[test]
fn input_scripts_find_memory_files() {
    let files = MemoryFiles::new()
        .with("/virtual/main.o", objects::main_object())
        .with("/virtual/answer.o", objects::answer_object(5))
        .with("/virtual/libgroup.a", &b"GROUP(main.o answer.o)\n"[..]);
    let mut options = LinkOptions::new();
    options.kind = OutputKind::StaticExecutable;
    options.search_paths.push("/virtual".into());
    options.input_provider = Some(Arc::new(files));
    options.push_input(InputKind::Library("group".into()), InputAttrs::default());
    let image = link_to_memory(&mut options).unwrap();
    assert!(objects::entry_point(&image).is_some());
}

#[test]
fn a_missing_memory_library_is_reported_as_not_found() {
    let mut options = LinkOptions::new();
    options.kind = OutputKind::StaticExecutable;
    options.search_paths.push("/virtual/nowhere".into());
    options.input_provider = Some(Arc::new(MemoryFiles::new()));
    options.push_input(
        InputKind::Library("qld-api-test-missing".into()),
        InputAttrs::default(),
    );
    let error = link_to_memory(&mut options).unwrap_err();
    assert!(matches!(error, qld::Error::NotFound(_)), "{error}");
}

#[test]
fn a_cancelled_link_fails_and_leaves_no_output() {
    let dir = scratch("cancelled");
    let output = dir.join("a.out");
    std::fs::write(&output, b"previous output").unwrap();
    let mut options = in_memory_options(1);
    options.output = Some(output.clone());
    let token = CancelToken::new();
    options.cancel = Some(token.clone());
    token.cancel();
    let error = qld::link(&options, &Collect::new()).unwrap_err();
    assert!(CancelToken::is_cancellation(&error), "{error}");
    assert_eq!(error.to_string(), "link cancelled");
    assert_eq!(std::fs::read(&output).unwrap(), b"previous output");

    let buffer = OutputBuffer::new();
    options.output_buffer = Some(buffer.clone());
    assert!(qld::link(&options, &Collect::new()).is_err());
    assert!(!buffer.is_filled());
}

/// An input provider that cancels the link when it is first asked for a
/// file, which makes cancellation in the middle of a link deterministic.
#[derive(Debug)]
struct CancelOnRead {
    files: MemoryFiles,
    token: CancelToken,
}

impl InputProvider for CancelOnRead {
    fn read(&self, path: &Path) -> Option<Arc<[u8]>> {
        self.token.cancel();
        self.files.read(path)
    }
}

#[test]
fn cancelling_during_a_link_stops_it() {
    let token = CancelToken::new();
    let mut options = LinkOptions::new();
    options.kind = OutputKind::StaticExecutable;
    options.cancel = Some(token.clone());
    options.input_provider = Some(Arc::new(CancelOnRead {
        files: MemoryFiles::new()
            .with("main.o", objects::main_object())
            .with("answer.o", objects::answer_object(2)),
        token: token.clone(),
    }));
    options.push_input(InputKind::File("main.o".into()), InputAttrs::default());
    options.push_input(InputKind::File("answer.o".into()), InputAttrs::default());
    let buffer = OutputBuffer::new();
    options.output_buffer = Some(buffer.clone());
    let error = qld::link(&options, &Collect::new()).unwrap_err();
    assert!(CancelToken::is_cancellation(&error), "{error}");
    assert!(!buffer.is_filled());
}

#[test]
fn an_unused_token_changes_nothing() {
    let mut plain = in_memory_options(4);
    let mut with_token = in_memory_options(4);
    with_token.cancel = Some(CancelToken::new());
    assert_eq!(
        link_to_memory(&mut plain).unwrap(),
        link_to_memory(&mut with_token).unwrap()
    );
}

/// A sink that counts diagnostics by severity and keeps the messages.
#[derive(Default)]
struct Counting {
    warnings: AtomicUsize,
    errors: AtomicUsize,
    messages: std::sync::Mutex<Vec<String>>,
}

impl DiagnosticSink for Counting {
    fn emit(&self, diagnostic: Diagnostic) {
        match diagnostic.severity {
            Severity::Error => &self.errors,
            _ => &self.warnings,
        }
        .fetch_add(1, Ordering::Relaxed);
        self.messages.lock().unwrap().push(diagnostic.message);
    }

    fn error_count(&self) -> usize {
        self.errors.load(Ordering::Relaxed)
    }
}

#[test]
fn a_custom_sink_receives_the_errors() {
    let mut options = LinkOptions::new();
    options.kind = OutputKind::StaticExecutable;
    options.output_buffer = Some(OutputBuffer::new());
    options.push_input(
        InputKind::bytes("main.o", objects::main_object()),
        InputAttrs::default(),
    );
    let sink = Counting::default();
    let error = qld::link(&options, &sink).unwrap_err();
    assert!(
        matches!(error, qld::Error::Reported { errors: 1 }),
        "{error}"
    );
    assert_eq!(sink.error_count(), 1);
    let messages = sink.messages.lock().unwrap();
    assert!(
        messages
            .iter()
            .any(|m| m.contains("undefined symbol: answer")),
        "{messages:?}"
    );
}

#[test]
fn links_run_in_the_callers_pool_and_concurrently() {
    use rayon::prelude::*;
    let expected = link_to_memory(&mut in_memory_options(11)).unwrap();
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(3)
        .build()
        .unwrap();
    let images: Vec<Vec<u8>> = pool.install(|| {
        (0..6)
            .into_par_iter()
            .map(|_| {
                assert!(rayon::current_thread_index().is_some());
                link_to_memory(&mut in_memory_options(11)).unwrap()
            })
            .collect()
    });
    assert!(images.iter().all(|image| *image == expected));
}

/// An input provider that records the size of the rayon pool the link is
/// reading its inputs in.
#[derive(Debug)]
struct PoolSize {
    files: MemoryFiles,
    threads: AtomicUsize,
}

impl InputProvider for PoolSize {
    fn read(&self, path: &Path) -> Option<Arc<[u8]>> {
        self.threads
            .store(rayon::current_num_threads(), Ordering::Relaxed);
        self.files.read(path)
    }
}

/// A pool the caller installed is the caller's to size: the driver runs in
/// it as it is, however large, and creates no pool of its own. With
/// `--threads` (`LinkOptions::threads`) the pool belongs to the link, and
/// the driver may still narrow the stages that do not scale.
#[test]
fn a_large_caller_pool_is_used_as_it_is() {
    // One more than the 16 threads the ELF driver would otherwise narrow to.
    const THREADS: usize = 17;
    let provider = Arc::new(PoolSize {
        files: MemoryFiles::new()
            .with("main.o", objects::main_object())
            .with("answer.o", objects::answer_object(7)),
        threads: AtomicUsize::new(0),
    });
    let mut options = LinkOptions::new();
    options.kind = OutputKind::StaticExecutable;
    options.input_provider = Some(provider.clone());
    options.push_input(InputKind::File("main.o".into()), InputAttrs::default());
    options.push_input(InputKind::File("answer.o".into()), InputAttrs::default());
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(THREADS)
        .build()
        .unwrap();
    let image = pool.install(|| link_to_memory(&mut options)).unwrap();
    assert!(objects::entry_point(&image).is_some());
    assert_eq!(provider.threads.load(Ordering::Relaxed), THREADS);
}

#[test]
fn output_buffers_and_tokens_compare_by_identity() {
    let buffer = OutputBuffer::new();
    assert_eq!(buffer, buffer.clone());
    assert_ne!(buffer, OutputBuffer::new());
    assert!(buffer.take().is_none());
    buffer.store(vec![1, 2]);
    assert!(buffer.clone().is_filled());
    assert_eq!(buffer.take(), Some(vec![1, 2]));
    assert!(!buffer.is_filled());

    let token = CancelToken::new();
    assert_eq!(token, token.clone());
    assert_ne!(token, CancelToken::new());
    assert!(token.check().is_ok());
    token.clone().cancel();
    assert!(token.is_cancelled());
    assert!(!CancelToken::is_cancellation(&qld::Error::Internal(
        "link cancelled".into()
    )));
}
