//! Backing storage for input bytes: file mappings, read buffers and shared
//! in-memory data.
//!
//! This is the only place in `input` that uses `unsafe`: creating a read-only
//! file mapping. `memmap2` marks mapping as unsafe because the bytes behind a
//! mapping can change, or disappear (`SIGBUS`), if another process modifies or
//! truncates the file while it is mapped. Like every mmap-based linker, qld
//! accepts that risk and documents it (`docs/development.md`, "`unsafe`
//! policy"): input files are treated as immutable for the duration of a link.

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;
use std::sync::Arc;

use memmap2::Mmap;

/// Files smaller than this are read into a heap buffer instead of mapped.
///
/// Mapping a tiny file costs a system call, a VMA and at least one page fault,
/// which is more than a single `read`.
pub(crate) const SMALL_FILE_LIMIT: u64 = 16 * 1024;

/// The storage behind one loaded input.
#[derive(Debug)]
pub(crate) enum Backing {
    /// A read-only mapping of the whole file.
    Mapped(Mmap),
    /// The file contents, read into memory.
    Owned(Box<[u8]>),
    /// Bytes supplied by a library caller.
    Shared(Arc<[u8]>),
}

impl Backing {
    /// The bytes.
    #[inline]
    pub(crate) fn bytes(&self) -> &[u8] {
        match self {
            Self::Mapped(map) => map,
            Self::Owned(bytes) => bytes,
            Self::Shared(bytes) => bytes,
        }
    }

    /// Whether the bytes come from a file mapping.
    pub(crate) fn is_mapped(&self) -> bool {
        matches!(self, Self::Mapped(_))
    }
}

/// Opens `path` and maps it, or reads it when it is small, not a regular
/// file, or cannot be mapped.
pub(crate) fn load(path: &Path) -> io::Result<Backing> {
    let mut file = File::open(path)?;
    let metadata = file.metadata()?;
    if metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "is a directory",
        ));
    }
    if metadata.is_file() && metadata.len() >= SMALL_FILE_LIMIT {
        // SAFETY: the mapping is read-only and private to this process. The
        // remaining hazard, another process writing to or truncating the file
        // while it is mapped, is accepted and documented for all inputs (see
        // the module comment). The `Mmap` owns the mapping and is only ever
        // exposed as `&[u8]` borrowed from it, so no reference outlives it.
        if let Ok(map) = unsafe { Mmap::map(&file) } {
            return Ok(Backing::Mapped(map));
        }
        // Mapping can fail on some file systems; reading still works.
    }
    let mut buffer = Vec::new();
    if metadata.is_file() {
        // A hint only: the file may change size before we read it.
        buffer.reserve(usize::try_from(metadata.len()).unwrap_or(0));
    }
    file.read_to_end(&mut buffer)?;
    Ok(Backing::Owned(buffer.into_boxed_slice()))
}

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)] // Test code builds fixtures, not parses input.
mod tests {
    use super::*;

    #[test]
    fn small_files_are_read_and_large_files_mapped() {
        let dir = std::env::temp_dir().join(format!("qld-input-map-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let small = dir.join("small");
        let large = dir.join("large");
        std::fs::write(&small, b"hello").unwrap();
        let big = vec![0xa5u8; 64 * 1024];
        std::fs::write(&large, &big).unwrap();

        let loaded = load(&small).unwrap();
        assert!(!loaded.is_mapped());
        assert_eq!(loaded.bytes(), b"hello");

        let loaded = load(&large).unwrap();
        assert_eq!(loaded.bytes(), &big[..]);
        #[cfg(target_os = "linux")]
        assert!(loaded.is_mapped());
        // Windows cannot delete a mapped file.
        drop(loaded);

        assert!(load(&dir).is_err());
        assert!(load(&dir.join("missing")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
