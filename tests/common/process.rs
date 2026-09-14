//! Running child processes with a timeout, shell-style word splitting and
//! quoting, and a parallel job runner built on std threads.

use std::io::Read;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

/// The result of running a child process.
#[derive(Debug)]
pub struct Output {
    /// Exit status (after a kill, if it timed out).
    pub status: ExitStatus,
    /// Captured standard output.
    pub stdout: Vec<u8>,
    /// Captured standard error.
    pub stderr: Vec<u8>,
    /// Whether the process was killed for exceeding the timeout.
    pub timed_out: bool,
}

impl Output {
    /// Whether the process exited with status 0 in time.
    pub fn success(&self) -> bool {
        self.status.success() && !self.timed_out
    }

    /// Standard output as (lossy) UTF-8.
    pub fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    /// Standard error as (lossy) UTF-8.
    pub fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }

    /// "exit code 1", "killed by signal 11" or "timed out".
    pub fn describe_status(&self) -> String {
        if self.timed_out {
            return "timed out".to_string();
        }
        if let Some(code) = self.status.code() {
            return format!("exit code {code}");
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            if let Some(signal) = self.status.signal() {
                return format!("killed by signal {signal}");
            }
        }
        format!("{}", self.status)
    }
}

/// Runs a command to completion, capturing stdout and stderr, killing it
/// after `timeout`. Standard input is closed.
pub fn run(command: &mut Command, timeout: Duration) -> std::io::Result<Output> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn()?;
    let stdout = child.stdout.take().map(drain);
    let stderr = child.stderr.take().map(drain);

    let start = Instant::now();
    let mut delay = Duration::from_millis(1);
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            timed_out = true;
            break child.wait()?;
        }
        thread::sleep(delay);
        delay = (delay * 2).min(Duration::from_millis(20));
    };
    let join = |handle: Option<thread::JoinHandle<Vec<u8>>>| {
        handle
            .map(|h| h.join().unwrap_or_default())
            .unwrap_or_default()
    };
    Ok(Output {
        status,
        stdout: join(stdout),
        stderr: join(stderr),
        timed_out,
    })
}

fn drain(mut pipe: impl Read + Send + 'static) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = pipe.read_to_end(&mut buffer);
        buffer
    })
}

/// Builds `sh -c <script>` running in `dir` with a C locale.
pub fn shell(sh: &Path, script: &str, dir: &Path) -> Command {
    let mut command = Command::new(sh);
    command
        .arg("-c")
        .arg(script)
        .current_dir(dir)
        .env("LC_ALL", "C");
    command
}

/// Splits a command line into words using POSIX shell quoting rules
/// (single quotes, double quotes, backslash). No expansion is performed.
pub fn split_words(text: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut in_word = false;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' | '\n' | '\r' => {
                if in_word {
                    words.push(std::mem::take(&mut current));
                    in_word = false;
                }
            }
            '\'' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => current.push(c),
                        None => return Err(format!("unterminated ' in {text:?}")),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(c @ ('"' | '\\' | '$' | '`')) => current.push(c),
                            Some('\n') => {}
                            Some(c) => {
                                current.push('\\');
                                current.push(c);
                            }
                            None => return Err(format!("unterminated \" in {text:?}")),
                        },
                        Some(c) => current.push(c),
                        None => return Err(format!("unterminated \" in {text:?}")),
                    }
                }
            }
            '\\' => {
                in_word = true;
                match chars.next() {
                    Some('\n') => {}
                    Some(c) => current.push(c),
                    None => return Err(format!("trailing backslash in {text:?}")),
                }
            }
            c => {
                in_word = true;
                current.push(c);
            }
        }
    }
    if in_word {
        words.push(current);
    }
    Ok(words)
}

/// Quotes a word for `sh` if it contains anything but safe characters.
pub fn shell_quote(word: &str) -> String {
    let safe = |c: char| c.is_ascii_alphanumeric() || "-_./=:,+@%".contains(c);
    if !word.is_empty() && word.chars().all(safe) {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}

/// Number of parallel jobs: `QLD_TEST_JOBS`, or the available parallelism.
pub fn job_count() -> usize {
    std::env::var("QLD_TEST_JOBS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or_else(|| thread::available_parallelism().map_or(4, |n| n.get()))
}

/// Applies `job` to every item on up to `jobs` threads. Results come back in
/// the order of `items`, whatever order the jobs finish in.
pub fn parallel_map<T: Sync, R: Send>(
    items: &[T],
    jobs: usize,
    job: impl Fn(&T) -> R + Sync,
) -> Vec<R> {
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<Option<R>>> = Mutex::new((0..items.len()).map(|_| None).collect());
    thread::scope(|scope| {
        for _ in 0..jobs.clamp(1, items.len().max(1)) {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, Ordering::SeqCst);
                    let Some(item) = items.get(index) else { break };
                    let result = job(item);
                    results.lock().unwrap_or_else(|e| e.into_inner())[index] = Some(result);
                }
            });
        }
    });
    results
        .into_inner()
        .unwrap_or_else(|e| e.into_inner())
        .into_iter()
        .map(|r| r.expect("every job ran"))
        .collect()
}
