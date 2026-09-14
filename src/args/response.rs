//! `@file` response file expansion.
//!
//! GNU tools expand `@file` arguments before option parsing, recursively,
//! using libiberty's `buildargv` quoting rules. lld adds
//! `--rsp-quoting=windows`, and Windows hosts default to Windows quoting.
//!
//! Reading goes through [`FileReader`], so parsing stays hermetic in tests.

use std::io;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Maximum nesting of response files, to stop `@a` containing `@a`.
const MAX_DEPTH: usize = 64;

/// Reads the contents of a response file.
///
/// The parser does all of its reading through this trait, so tests can supply
/// response files from memory. Any `Fn(&Path) -> io::Result<Vec<u8>>` closure
/// implements it.
pub trait FileReader {
    /// Returns the bytes of the file at `path`.
    ///
    /// # Errors
    ///
    /// Returns the I/O error that prevented reading the file.
    fn read_file(&self, path: &Path) -> io::Result<Vec<u8>>;
}

impl<F> FileReader for F
where
    F: Fn(&Path) -> io::Result<Vec<u8>>,
{
    fn read_file(&self, path: &Path) -> io::Result<Vec<u8>> {
        self(path)
    }
}

/// A [`FileReader`] backed by [`std::fs::read`].
#[derive(Clone, Copy, Debug, Default)]
pub struct FsReader;

impl FileReader for FsReader {
    fn read_file(&self, path: &Path) -> io::Result<Vec<u8>> {
        std::fs::read(path)
    }
}

/// A [`FileReader`] that has no files at all. Useful when response files must
/// not be expanded from disk.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoFiles;

impl FileReader for NoFiles {
    fn read_file(&self, _path: &Path) -> io::Result<Vec<u8>> {
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            "response files are disabled",
        ))
    }
}

/// Tokenization rules for response file contents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Quoting {
    /// GNU libiberty rules: whitespace separates arguments; `'…'` and `"…"`
    /// group; a backslash escapes the next character anywhere, even inside
    /// quotes.
    Gnu,
    /// Windows (MSVC CRT) rules: only `"` quotes; backslashes are literal
    /// unless they precede a `"`.
    Windows,
}

impl Quoting {
    /// The host default: [`Quoting::Windows`] on Windows, otherwise
    /// [`Quoting::Gnu`].
    #[must_use]
    pub fn host_default() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else {
            Self::Gnu
        }
    }
}

/// Splits response file contents into arguments.
#[must_use]
pub fn tokenize(contents: &[u8], quoting: Quoting) -> Vec<Vec<u8>> {
    match quoting {
        Quoting::Gnu => tokenize_gnu(contents),
        Quoting::Windows => tokenize_windows(contents),
    }
}

fn is_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

/// libiberty `buildargv`.
fn tokenize_gnu(contents: &[u8]) -> Vec<Vec<u8>> {
    let mut args = Vec::new();
    let mut bytes = contents.iter().copied().peekable();
    loop {
        while bytes.next_if(|&b| is_space(b)).is_some() {}
        if bytes.peek().is_none() {
            break;
        }
        let mut arg = Vec::new();
        let (mut squote, mut dquote, mut escape) = (false, false, false);
        while let Some(&byte) = bytes.peek() {
            if is_space(byte) && !squote && !dquote && !escape {
                break;
            }
            bytes.next();
            if escape {
                escape = false;
                arg.push(byte);
            } else if byte == b'\\' {
                escape = true;
            } else if squote {
                if byte == b'\'' {
                    squote = false;
                } else {
                    arg.push(byte);
                }
            } else if dquote {
                if byte == b'"' {
                    dquote = false;
                } else {
                    arg.push(byte);
                }
            } else if byte == b'\'' {
                squote = true;
            } else if byte == b'"' {
                dquote = true;
            } else {
                arg.push(byte);
            }
        }
        args.push(arg);
    }
    args
}

/// MSVC CRT style, as LLVM's `TokenizeWindowsCommandLine`.
fn tokenize_windows(contents: &[u8]) -> Vec<Vec<u8>> {
    let mut args = Vec::new();
    let mut i = 0;
    let len = contents.len();
    loop {
        while i < len && contents.get(i).is_some_and(|&b| is_space(b)) {
            i += 1;
        }
        if i >= len {
            break;
        }
        let mut arg = Vec::new();
        let mut quoted = false;
        while let Some(&byte) = contents.get(i) {
            if is_space(byte) && !quoted {
                break;
            }
            if byte == b'\\' {
                let start = i;
                while contents.get(i) == Some(&b'\\') {
                    i += 1;
                }
                let count = i - start;
                if contents.get(i) == Some(&b'"') {
                    arg.extend(std::iter::repeat_n(b'\\', count / 2));
                    if count % 2 == 1 {
                        arg.push(b'"');
                        i += 1;
                    }
                } else {
                    arg.extend(std::iter::repeat_n(b'\\', count));
                }
                continue;
            }
            if byte == b'"' {
                if quoted && contents.get(i + 1) == Some(&b'"') {
                    arg.push(b'"');
                    i += 2;
                    continue;
                }
                quoted = !quoted;
                i += 1;
                continue;
            }
            arg.push(byte);
            i += 1;
        }
        args.push(arg);
    }
    args
}

/// Finds a `--rsp-quoting` option in the unexpanded arguments.
///
/// # Errors
///
/// Returns [`Error::Option`] for a value other than `posix` or `windows`.
pub fn quoting_from_args(args: &[Vec<u8>]) -> Result<Quoting> {
    let mut quoting = Quoting::host_default();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let value = if arg == b"--rsp-quoting" || arg == b"-rsp-quoting" {
            match iter.next() {
                Some(value) => value.as_slice(),
                None => break,
            }
        } else if let Some(value) = arg
            .strip_prefix(b"--rsp-quoting=")
            .or_else(|| arg.strip_prefix(b"-rsp-quoting="))
        {
            value
        } else if arg == b"--" {
            break;
        } else {
            continue;
        };
        quoting = match value {
            b"posix" => Quoting::Gnu,
            b"windows" => Quoting::Windows,
            other => {
                return Err(Error::Option(format!(
                    "invalid response file quoting: {}",
                    String::from_utf8_lossy(other)
                )));
            }
        };
    }
    Ok(quoting)
}

/// Replaces every `@file` argument with the arguments read from that file,
/// recursively.
///
/// # Errors
///
/// Returns [`Error::Io`] naming the file when a response file cannot be read,
/// and [`Error::Option`] when response files nest more than 64 deep.
pub fn expand(
    args: Vec<Vec<u8>>,
    reader: &dyn FileReader,
    quoting: Quoting,
) -> Result<Vec<Vec<u8>>> {
    let mut out = Vec::with_capacity(args.len());
    expand_into(args, reader, quoting, 0, &mut out)?;
    Ok(out)
}

fn expand_into(
    args: Vec<Vec<u8>>,
    reader: &dyn FileReader,
    quoting: Quoting,
    depth: usize,
    out: &mut Vec<Vec<u8>>,
) -> Result<()> {
    for arg in args {
        let Some(name) = arg.strip_prefix(b"@") else {
            out.push(arg);
            continue;
        };
        if name.is_empty() {
            out.push(arg);
            continue;
        }
        if depth >= MAX_DEPTH {
            return Err(Error::Option(format!(
                "response files nested too deeply at {}",
                String::from_utf8_lossy(&arg)
            )));
        }
        let path = bytes_to_path(name);
        let contents = reader
            .read_file(&path)
            .map_err(|source| Error::io(path.clone(), source))?;
        let nested = tokenize(&contents, quoting);
        expand_into(nested, reader, quoting, depth + 1, out)?;
    }
    Ok(())
}

/// Converts argument bytes to a path, losslessly on Unix.
pub(crate) fn bytes_to_path(bytes: &[u8]) -> PathBuf {
    PathBuf::from(super::parse::bytes_to_os(bytes.to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gnu(text: &str) -> Vec<String> {
        tokenize(text.as_bytes(), Quoting::Gnu)
            .into_iter()
            .map(|a| String::from_utf8(a).unwrap())
            .collect()
    }

    fn win(text: &str) -> Vec<String> {
        tokenize(text.as_bytes(), Quoting::Windows)
            .into_iter()
            .map(|a| String::from_utf8(a).unwrap())
            .collect()
    }

    #[test]
    fn gnu_quoting_matches_buildargv() {
        assert_eq!(gnu("  a\tb\n\nc  "), ["a", "b", "c"]);
        assert_eq!(gnu("'a b' \"c d\""), ["a b", "c d"]);
        assert_eq!(gnu(r"a\ b"), ["a b"]);
        assert_eq!(gnu(r"'it\'s'"), ["it's"]);
        assert_eq!(gnu(r#""say \"hi\"""#), [r#"say "hi""#]);
        assert_eq!(gnu("x'y z'w"), ["xy zw"]);
        assert_eq!(gnu("'' \"\""), ["", ""]);
        assert_eq!(gnu("trailing\\"), ["trailing"]);
        assert_eq!(gnu("'unterminated quote"), ["unterminated quote"]);
        assert!(gnu("   \n").is_empty());
    }

    #[test]
    fn windows_quoting_matches_msvc() {
        assert_eq!(win(r"C:\dir\file.o b"), [r"C:\dir\file.o", "b"]);
        assert_eq!(win(r#""a b" c"#), ["a b", "c"]);
        assert_eq!(win(r#"a\"b"#), [r#"a"b"#]);
        assert_eq!(win(r#"a\\"b c""#), [r"a\b c"]);
        assert_eq!(win(r#""a""b""#), [r#"a"b"#]);
    }

    #[test]
    fn expansion_is_recursive_and_bounded() {
        let reader = |path: &Path| -> io::Result<Vec<u8>> {
            match path.to_str() {
                Some("outer") => Ok(b"-a @inner -d".to_vec()),
                Some("inner") => Ok(b"-b 'c c'".to_vec()),
                Some("loop") => Ok(b"@loop".to_vec()),
                _ => Err(io::Error::from(io::ErrorKind::NotFound)),
            }
        };
        let args = vec![b"@outer".to_vec(), b"x".to_vec()];
        let out = expand(args, &reader, Quoting::Gnu).unwrap();
        assert_eq!(
            out,
            [&b"-a"[..], b"-b", b"c c", b"-d", b"x"]
                .iter()
                .map(|a| a.to_vec())
                .collect::<Vec<_>>()
        );
        assert!(expand(vec![b"@loop".to_vec()], &reader, Quoting::Gnu).is_err());
        assert!(matches!(
            expand(vec![b"@missing".to_vec()], &reader, Quoting::Gnu),
            Err(Error::Io { .. })
        ));
        assert_eq!(
            expand(vec![b"@".to_vec()], &reader, Quoting::Gnu).unwrap(),
            vec![b"@".to_vec()]
        );
    }

    #[test]
    fn rsp_quoting_option_is_found_before_expansion() {
        let args = |list: &[&str]| -> Vec<Vec<u8>> {
            list.iter().map(|a| a.as_bytes().to_vec()).collect()
        };
        assert_eq!(
            quoting_from_args(&args(&["--rsp-quoting=windows"])).unwrap(),
            Quoting::Windows
        );
        assert_eq!(
            quoting_from_args(&args(&["--rsp-quoting", "posix"])).unwrap(),
            Quoting::Gnu
        );
        assert!(quoting_from_args(&args(&["--rsp-quoting=dos"])).is_err());
    }
}
