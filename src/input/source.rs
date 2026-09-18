//! Where input bytes come from: the file system, or a library caller.
//!
//! **Workstream W36.** A library caller supplies inputs that exist only in
//! memory in one of two ways:
//!
//! - **Anonymous inputs**: [`InputKind::Bytes`](crate::args::InputKind::Bytes)
//!   entries in [`LinkOptions::inputs`](crate::args::LinkOptions::inputs),
//!   each an `Arc<[u8]>` with a name for diagnostics. They take part in the
//!   link like a file named on the command line.
//! - **Files by path**: an [`InputProvider`] in
//!   [`LinkOptions::input_provider`](crate::args::LinkOptions::input_provider),
//!   usually [`MemoryFiles`], answers for paths before the file system does.
//!   Inputs named by path, `-l` libraries found in the search directories,
//!   `INPUT`/`GROUP` entries of input scripts and thin archive members are
//!   all looked up there first. This lets a caller run a command line
//!   (parsed with [`parse_gnu`](crate::args::parse_gnu)) against objects it
//!   produced itself, without temporary files.
//!
//! The [`FileTable`] of a link consults the provider when it loads a path
//! ([`FileTable::for_link`]), and implements [`FileSystem`] so that library
//! search sees the same overlay. Provided bytes are shared, never copied.
//!
//! Output to memory and cancellation are link options too:
//! [`OutputBuffer`](crate::args::OutputBuffer) and
//! [`CancelToken`](crate::args::CancelToken).

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::search::{FileSystem, RealFileSystem};
use super::table::FileTable;

/// Answers for paths before the file system does; see the
/// [module documentation](self).
///
/// Paths are compared exactly as the link spells them after sysroot and
/// search-directory resolution: `-L/lib -lfoo` asks for `/lib/libfoo.so`,
/// then `/lib/libfoo.a`, and a relative input `a.o` is asked for as `a.o`.
/// No normalization happens (`./a.o` is not `a.o`).
///
/// Implementations are called from parallel stages, so they must be `Send`
/// and `Sync`; they should be cheap, since library search asks
/// [`InputProvider::contains`] for every candidate name in every search
/// directory.
pub trait InputProvider: Send + Sync + fmt::Debug {
    /// The contents of the file at `path`, or `None` to let the file system
    /// answer.
    fn read(&self, path: &Path) -> Option<Arc<[u8]>>;

    /// Whether [`InputProvider::read`] would return contents for `path`.
    fn contains(&self, path: &Path) -> bool {
        self.read(path).is_some()
    }
}

/// An [`InputProvider`] holding files in memory, keyed by path.
///
/// # Example
///
/// ```
/// use std::sync::Arc;
/// use qld::input::source::{InputProvider, MemoryFiles};
///
/// let files = MemoryFiles::new()
///     .with("main.o", b"\x7fELF...".to_vec())
///     .with("/virtual/lib/libanswer.a", &b"!<arch>\n"[..]);
/// assert!(files.contains("main.o".as_ref()));
///
/// let mut options = qld::LinkOptions::new();
/// options.search_paths.push("/virtual/lib".into());
/// options.input_provider = Some(Arc::new(files));
/// ```
#[derive(Clone, Debug, Default)]
pub struct MemoryFiles {
    files: BTreeMap<PathBuf, Arc<[u8]>>,
}

impl MemoryFiles {
    /// Creates an empty set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds (or replaces) the file at `path`. `data` is anything that
    /// converts into an `Arc<[u8]>`: an `Arc<[u8]>` is shared as is, a
    /// `Vec<u8>` or a `&'static [u8]` is copied once.
    pub fn insert(&mut self, path: impl Into<PathBuf>, data: impl Into<Arc<[u8]>>) {
        self.files.insert(path.into(), data.into());
    }

    /// Adds the file at `path` and returns the set, for chaining.
    #[must_use]
    pub fn with(mut self, path: impl Into<PathBuf>, data: impl Into<Arc<[u8]>>) -> Self {
        self.insert(path, data);
        self
    }

    /// Removes the file at `path` and returns its contents.
    pub fn remove(&mut self, path: &Path) -> Option<Arc<[u8]>> {
        self.files.remove(path)
    }

    /// The contents of the file at `path`.
    #[must_use]
    pub fn get(&self, path: &Path) -> Option<&Arc<[u8]>> {
        self.files.get(path)
    }

    /// The number of files.
    #[must_use]
    pub fn len(&self) -> usize {
        self.files.len()
    }

    /// Whether there are no files.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// The paths, in sorted order.
    pub fn paths(&self) -> impl Iterator<Item = &Path> {
        self.files.keys().map(PathBuf::as_path)
    }
}

impl InputProvider for MemoryFiles {
    fn read(&self, path: &Path) -> Option<Arc<[u8]>> {
        self.files.get(path).map(Arc::clone)
    }

    fn contains(&self, path: &Path) -> bool {
        self.files.contains_key(path)
    }
}

impl<P: Into<PathBuf>, D: Into<Arc<[u8]>>> FromIterator<(P, D)> for MemoryFiles {
    fn from_iter<I: IntoIterator<Item = (P, D)>>(iter: I) -> Self {
        let mut files = Self::new();
        for (path, data) in iter {
            files.insert(path, data);
        }
        files
    }
}

/// Library search through a link's file table sees the table's
/// [`InputProvider`] first, then the file system.
impl FileSystem for FileTable {
    fn is_file(&self, path: &Path) -> bool {
        self.provider()
            .is_some_and(|provider| provider.contains(path))
            || RealFileSystem.is_file(path)
    }
}

/// A reference to a file system is a file system, so a `&FileTable` or a
/// `&dyn FileSystem` can stand wherever one is expected.
impl<T: FileSystem + ?Sized> FileSystem for &T {
    fn is_file(&self, path: &Path) -> bool {
        (**self).is_file(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::{CancelToken, LinkOptions};
    use crate::input::table::Source;

    #[test]
    fn memory_files_answer_by_exact_path() {
        let files: MemoryFiles = [("a.o", b"one".to_vec()), ("/lib/libx.a", b"two".to_vec())]
            .into_iter()
            .collect();
        assert_eq!(files.len(), 2);
        assert_eq!(files.read(Path::new("a.o")).as_deref(), Some(&b"one"[..]));
        assert!(files.contains(Path::new("/lib/libx.a")));
        assert!(!files.contains(Path::new("./a.o")));
        assert_eq!(
            files.paths().collect::<Vec<_>>(),
            [Path::new("/lib/libx.a"), Path::new("a.o")]
        );
    }

    #[test]
    fn the_file_table_loads_provided_paths_without_copying() {
        let data: Arc<[u8]> = Arc::from(&b"INPUT(b.o)\n"[..]);
        let mut options = LinkOptions::new();
        options.input_provider = Some(Arc::new(
            MemoryFiles::new().with("/virtual/a.ld", Arc::clone(&data)),
        ));
        let table = FileTable::for_link(&options);
        assert!(table.is_file(Path::new("/virtual/a.ld")));
        assert!(!table.is_file(Path::new("/virtual/b.o")));
        let id = table.load_path(Path::new("/virtual/a.ld")).unwrap();
        let file = table.get(id).unwrap();
        assert!(std::ptr::eq(file.data(), &*data));
        assert_eq!(file.path(), Path::new("/virtual/a.ld"));
        let loaded = table.load_all(&[
            Source::Path("/virtual/a.ld".into()),
            Source::Path("/virtual/missing.o".into()),
        ]);
        assert!(loaded[0].is_ok());
        assert!(loaded[1].is_err());
    }

    #[test]
    fn a_cancelled_table_loads_nothing() {
        let token = CancelToken::new();
        let mut options = LinkOptions::new();
        options.cancel = Some(token.clone());
        options.input_provider = Some(Arc::new(MemoryFiles::new().with("a", vec![1u8])));
        let table = FileTable::for_link(&options);
        assert!(table.load_path(Path::new("a")).is_ok());
        token.cancel();
        let error = table.load_path(Path::new("a")).unwrap_err();
        assert!(CancelToken::is_cancellation(&error));
        assert_eq!(error.to_string(), "link cancelled");
        let all = table.load_all(&[Source::Path("a".into())]);
        assert!(
            all.iter()
                .all(|r| r.as_ref().is_err_and(CancelToken::is_cancellation))
        );
    }
}
