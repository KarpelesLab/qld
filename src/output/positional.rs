//! Positional reads and writes (`pread`/`pwrite` on Unix, `ReadFile` and
//! `WriteFile` with an offset on Windows), through std only.
//!
//! Positional calls take `&File` and do not depend on a shared file cursor,
//! so several threads can write disjoint ranges of the same file at once.

use std::fs::File;
use std::io;

/// Whether this platform supports positional I/O. Where it does not, the
/// write backing is never selected.
pub(super) const SUPPORTED: bool = cfg!(any(unix, windows));

/// Writes all of `buf` at `offset`.
#[cfg(unix)]
pub(super) fn write_all_at(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::write_all_at(file, buf, offset)
}

/// Fills `buf` from `offset`; a short file is an `UnexpectedEof` error.
#[cfg(unix)]
pub(super) fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(file, buf, offset)
}

/// Writes all of `buf` at `offset`.
#[cfg(windows)]
pub(super) fn write_all_at(file: &File, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match file.seek_write(buf, offset) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => {
                buf = buf.get(n..).unwrap_or(&[]);
                offset = offset.saturating_add(n as u64);
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Fills `buf` from `offset`; a short file is an `UnexpectedEof` error.
#[cfg(windows)]
pub(super) fn read_exact_at(file: &File, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match file.seek_read(buf, offset) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => {
                buf = std::mem::take(&mut buf).get_mut(n..).unwrap_or(&mut []);
                offset = offset.saturating_add(n as u64);
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Positional I/O is not available on this platform.
#[cfg(not(any(unix, windows)))]
pub(super) fn write_all_at(_file: &File, _buf: &[u8], _offset: u64) -> io::Result<()> {
    Err(io::ErrorKind::Unsupported.into())
}

/// Positional I/O is not available on this platform.
#[cfg(not(any(unix, windows)))]
pub(super) fn read_exact_at(_file: &File, _buf: &mut [u8], _offset: u64) -> io::Result<()> {
    Err(io::ErrorKind::Unsupported.into())
}
