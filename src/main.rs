//! The `qld` command-line linker.
//!
//! This binary is a thin wrapper: it selects a command-line flavor, parses
//! argv and calls [`qld::link`]. Everything it can do is available through
//! the library too, except `--fork`, which is about the process itself.
//!
//! # `--fork`
//!
//! On Unix, `qld` links in a child process by default (`--fork`, as mold
//! and wild do) so that it can return as soon as the output is complete.
//! A large link then spends a while freeing memory and unmapping its inputs
//! (150 ms for a 1.2 GiB output with debug information), and the build
//! does not have to wait for that. `--no-fork` links in the `qld` process
//! itself, as do `--help`, `--version`, and every library call.
//!
//! The child is this executable started again (no `fork()` without
//! `unsafe`), found in the environment variable `QLD_FORK_CHILD`, which
//! holds the parent's process ID so that a `qld` started further down (by
//! an LTO plugin's subprocess, say) does not take itself for the child. The
//! child parses the same argv, links with a
//! [`qld::args::OutputCompleteHook`] and, when the output is complete,
//! sends its exit status to the parent, which exits with it. See [`fork`]
//! for the channels and the failure modes.

use std::ffi::OsString;
use std::process::ExitCode;

use qld::args::{OutputCompleteHook, ParseOutcome};
use qld::diag::{Diagnostic, DiagnosticSink, Stderr};

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().collect();
    #[cfg(unix)]
    if let Some(child) = fork::Child::from_env() {
        let hook = OutputCompleteHook::new(move || child.notify(0));
        let code = run(&args, Launch::Child(hook));
        // A failed link, or a backend that finished without the hook
        // (cannot happen with `qld::link`, which runs it on success).
        child.notify(code);
        return ExitCode::from(code);
    }
    ExitCode::from(run(&args, Launch::MayFork))
}

/// How [`run`] links.
enum Launch {
    /// In a child process if `--fork` is in effect and supported, else here.
    MayFork,
    /// Here, as the child process of `--fork`, running this hook once the
    /// output is complete.
    #[cfg_attr(not(unix), allow(dead_code))]
    Child(OutputCompleteHook),
}

/// Runs the command line and returns the exit status, after reporting any
/// error.
fn run(args: &[OsString], launch: Launch) -> u8 {
    let diagnostics = Stderr::new(qld::PROGRAM_NAME);
    match run_link(args, launch, &diagnostics) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("{}: error: {error}", qld::PROGRAM_NAME);
            1
        }
    }
}

fn run_link(
    args: &[OsString],
    launch: Launch,
    diagnostics: &dyn DiagnosticSink,
) -> qld::Result<u8> {
    let mut options = match qld::parse_gnu(args)? {
        ParseOutcome::Help => {
            print!("{}", qld::args::usage());
            return Ok(0);
        }
        ParseOutcome::Version => {
            println!("{}", qld::version_line());
            return Ok(0);
        }
        ParseOutcome::Link(options) => options,
    };
    match launch {
        Launch::MayFork => {
            #[cfg(unix)]
            if options.fork
                && fork::allowed(args, &options)
                && let Some(code) = fork::link_in_child(args)
            {
                return Ok(code);
            }
        }
        Launch::Child(hook) => options.on_output_complete = Some(hook),
    }
    // GNU ld exits inside the plugin's fatal callback; plugins can
    // misbehave if the linker returns instead. Library callers get
    // an error (see LinkOptions::exit_on_plugin_fatal).
    options.exit_on_plugin_fatal = true;
    if !options.no_warnings {
        for warning in &options.warnings {
            diagnostics.emit(Diagnostic::warning(warning.clone()));
        }
    }
    if options.fatal_warnings && !options.warnings.is_empty() {
        return Err(qld::Error::Reported {
            errors: options.warnings.len(),
        });
    }
    qld::link(&options, diagnostics)?;
    Ok(0)
}

/// `--fork`: running the link in a child process.
///
/// # Channels
///
/// - **Status**: the child's stdin is one end of a Unix socket pair. The
///   child writes one byte, its exit status, once the output is complete
///   (or when it is about to exit after a failure); the parent reads it
///   and exits with it. The parent shuts down its sending side at once, so
///   anything in the child that reads stdin sees end of file.
/// - **stdout and stderr**: a caller reading a pipe (`ninja`, `make -O`,
///   `$(...)`, a test harness) waits for end of file, which would only
///   come when the child exits. So a stream that is a pipe or a socket is
///   relayed: the child gets a socket, a thread in the parent copies it to
///   the real stream, and the child shuts its sockets down when it reports
///   its status. If stdout and stderr are the same pipe (`2>&1`), the
///   child gets one socket for both, which keeps their relative order.
///   Terminals, files and `/dev/null` are passed to the child as they are:
///   nobody waits for their end of file.
/// - Everything else (the environment, the working directory, other open
///   file descriptors such as a `make` jobserver's, the process group,
///   ignored signals) is inherited as by any child process. The child gets
///   the same `argv[0]`, so flavor selection is unchanged.
///
/// # Failure modes
///
/// - The child cannot be started, or a relay thread cannot be: the link
///   runs in the parent.
/// - The child exits or dies before reporting (a crash, a plugin's fatal
///   error, which exits the process as GNU ld does, a signal): the status
///   socket reaches end of file, the parent waits for the child, and exits
///   with its status, or with 128 + the signal number after an error
///   message if it was killed.
/// - The child reports only after the output file is in place under its
///   final name, the link map is written, stdout is flushed and LTO
///   plugins are cleaned up (`-plugin-save-temps` keeps their files as
///   without `--fork`), so a command started after `qld` returns sees the
///   complete output.
/// - A signal sent to the process group (Ctrl-C in a terminal, `ninja` or
///   `timeout` stopping a job) reaches both processes. A signal sent to the
///   parent alone lets the child finish the link on its own, as with mold
///   and wild.
/// - Paths under `/dev/` or `/proc/` on the command line (`-o /dev/stdout`,
///   an input read from `/dev/stdin`) name the parent's standard streams,
///   which the child does not have: such links run in the parent.
#[cfg(unix)]
mod fork {
    use std::ffi::OsString;
    use std::fs::File;
    use std::io::{self, Read, Write};
    use std::net::Shutdown;
    use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    use std::os::unix::net::UnixStream;
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    use std::path::PathBuf;
    use std::process::{Command, ExitStatus, Stdio};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread::JoinHandle;

    use qld::LinkOptions;

    /// Set in the child's environment: `PID:FLAGS`, the parent's process ID
    /// and which streams are relayed (`o` for stdout, `e` for stderr).
    const ENV: &str = "QLD_FORK_CHILD";

    /// Whether a link may run in a child process: see "Failure modes" in
    /// the [module documentation](self).
    pub fn allowed(args: &[OsString], options: &LinkOptions) -> bool {
        let system = |path: &[u8]| {
            path.windows(5).any(|w| w == b"/dev/") || path.windows(6).any(|w| w == b"/proc/")
        };
        let paths = [Some(options.output_path()), options.map_file.clone()];
        !args.iter().any(|arg| system(arg.as_encoded_bytes()))
            && !paths
                .iter()
                .flatten()
                .any(|path| system(path.as_os_str().as_encoded_bytes()))
    }

    /// A standard stream of the parent, as the child gets it.
    struct Stream {
        /// A pipe or socket, relayed through a socket.
        relay: bool,
        /// Device and inode, to recognize stdout and stderr as one file.
        id: (u64, u64),
    }

    impl Stream {
        fn of(fd: BorrowedFd<'_>) -> Option<Self> {
            let metadata = File::from(fd.try_clone_to_owned().ok()?).metadata().ok()?;
            let kind = metadata.file_type();
            Some(Self {
                relay: kind.is_fifo() || kind.is_socket(),
                id: (metadata.dev(), metadata.ino()),
            })
        }
    }

    /// Runs the link in a child process and returns the exit status the
    /// parent should exit with, or `None` if no child could be started
    /// (the caller then links in this process).
    pub fn link_in_child(args: &[OsString]) -> Option<u8> {
        let stdout = Stream::of(io::stdout().as_fd())?;
        let stderr = Stream::of(io::stderr().as_fd())?;
        let shared = stdout.relay && stderr.relay && stdout.id == stderr.id;
        let (status, status_child) = UnixStream::pair().ok()?;

        let mut command = Command::new(program()?);
        if let Some(arg0) = args.first() {
            command.arg0(arg0);
        }
        command
            .args(args.iter().skip(1))
            .env(
                ENV,
                format!(
                    "{}:{}{}",
                    std::process::id(),
                    if stdout.relay { "o" } else { "" },
                    if stderr.relay { "e" } else { "" }
                ),
            )
            .stdin(Stdio::from(OwnedFd::from(status_child)));

        // Relay threads start before the child, so that a child is never
        // left writing to a socket nobody reads.
        let mut relays = Vec::new();
        if stdout.relay {
            let child = relay(io::stdout().as_fd(), &mut relays)?;
            if shared {
                command.stderr(Stdio::from(OwnedFd::from(child.try_clone().ok()?)));
            }
            command.stdout(Stdio::from(OwnedFd::from(child)));
        }
        if stderr.relay && !shared {
            let child = relay(io::stderr().as_fd(), &mut relays)?;
            command.stderr(Stdio::from(OwnedFd::from(child)));
        }

        let spawned = command.spawn();
        // Close the parent's copies of the child's ends, so that the relays
        // and the status socket see end of file when the child is done.
        drop(command);
        let Ok(mut child) = spawned else {
            join(relays);
            return None;
        };
        let _ = status.shutdown(Shutdown::Write);

        let reported = read_status(&status);
        join(relays);
        Some(match reported {
            Some(code) => code,
            None => exit_code(child.wait()),
        })
    }

    /// The executable to start: this one.
    fn program() -> Option<PathBuf> {
        let path = std::env::current_exe().ok()?;
        // If the file at that path was replaced since this process started
        // (qld linking a new qld over itself), `/proc/self/exe` still names
        // this executable. It is not the first choice because the child's
        // process name would be `exe`.
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            let id = |path: &std::path::Path| {
                std::fs::metadata(path)
                    .ok()
                    .map(|metadata| (metadata.dev(), metadata.ino()))
            };
            let proc = PathBuf::from("/proc/self/exe");
            if let Some(this) = id(&proc)
                && id(&path) != Some(this)
            {
                return Some(proc);
            }
        }
        Some(path)
    }

    /// Starts a thread that copies a new socket to `target` until end of
    /// file, and returns the child's end of the socket.
    fn relay(target: BorrowedFd<'_>, relays: &mut Vec<JoinHandle<()>>) -> Option<UnixStream> {
        let mut target = File::from(target.try_clone_to_owned().ok()?);
        let (mut source, child) = UnixStream::pair().ok()?;
        let thread = std::thread::Builder::new()
            .name("qld-relay".into())
            .spawn(move || {
                let mut buffer = vec![0u8; 64 << 10];
                loop {
                    match source.read(&mut buffer) {
                        Ok(0) => return,
                        Ok(n) => {
                            // If the real stream is gone, stop reading: the
                            // child then gets the same write error it would
                            // have got without `--fork`.
                            if target.write_all(buffer.get(..n).unwrap_or(&[])).is_err() {
                                return;
                            }
                        }
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                        Err(_) => return,
                    }
                }
            })
            .ok()?;
        relays.push(thread);
        Some(child)
    }

    fn join(relays: Vec<JoinHandle<()>>) {
        for relay in relays {
            let _ = relay.join();
        }
    }

    /// The status byte the child sent, or `None` at end of file.
    fn read_status(mut status: &UnixStream) -> Option<u8> {
        let mut byte = [0u8; 1];
        loop {
            match status.read(&mut byte) {
                Ok(1) => return Some(byte[0]),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Ok(_) | Err(_) => return None,
            }
        }
    }

    /// The exit status for a child that exited without reporting.
    fn exit_code(status: io::Result<ExitStatus>) -> u8 {
        let program = qld::PROGRAM_NAME;
        match status {
            Ok(status) => {
                if let Some(code) = status.code() {
                    return u8::try_from(code & 0xff).unwrap_or(1);
                }
                let Some(signal) = status.signal() else {
                    return 1;
                };
                let core = if status.core_dumped() {
                    " (core dumped)"
                } else {
                    ""
                };
                eprintln!("{program}: error: the link process was killed by signal {signal}{core}");
                u8::try_from(signal.saturating_add(128)).unwrap_or(1)
            }
            Err(error) => {
                eprintln!("{program}: error: cannot wait for the link process: {error}");
                1
            }
        }
    }

    /// The child side: this process was started by [`link_in_child`].
    #[derive(Clone, Copy)]
    pub struct Child {
        relay_stdout: bool,
        relay_stderr: bool,
    }

    impl Child {
        /// `Some` if this process is the child of a `qld` parent.
        pub fn from_env() -> Option<Self> {
            let value = std::env::var(ENV).ok()?;
            let (pid, flags) = value.split_once(':')?;
            if pid.parse::<u32>().ok()? != std::os::unix::process::parent_id() {
                return None;
            }
            // The status socket; anything else means the variable was not
            // set for this process.
            let stdin = File::from(io::stdin().as_fd().try_clone_to_owned().ok()?);
            if !stdin.metadata().ok()?.file_type().is_socket() {
                return None;
            }
            Some(Self {
                relay_stdout: flags.contains('o'),
                relay_stderr: flags.contains('e'),
            })
        }

        /// Sends the exit status to the parent, after flushing stdout and
        /// ending the relayed streams. Only the first call does anything:
        /// nothing may be printed after it.
        pub fn notify(self, code: u8) {
            static SENT: AtomicBool = AtomicBool::new(false);
            if SENT.swap(true, Ordering::AcqRel) {
                return;
            }
            let _ = io::stdout().flush();
            let _ = io::stderr().flush();
            if self.relay_stdout {
                shut_down(io::stdout().as_fd());
            }
            if self.relay_stderr {
                shut_down(io::stderr().as_fd());
            }
            if let Ok(fd) = io::stdin().as_fd().try_clone_to_owned() {
                let _ = UnixStream::from(fd).write_all(&[code]);
            }
        }
    }

    /// Ends the parent's relay of a stream: shutting the socket down (not
    /// just closing this descriptor) also ends it for copies of it, such
    /// as plugin subprocesses', and for the descriptor itself.
    fn shut_down(fd: BorrowedFd<'_>) {
        if let Ok(fd) = fd.try_clone_to_owned() {
            let _ = UnixStream::from(fd).shutdown(Shutdown::Write);
        }
    }
}
