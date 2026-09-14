//! The file table: every input's bytes, kept alive for the whole link.
//!
//! [`FileTable`] is append-only and can be shared across threads. Adding a
//! file takes `&self`, and the bytes it hands out borrow from the table, so
//! parsed structures can hold `&'a [u8]` slices while other threads keep
//! loading (for example thin archive members extracted during symbol
//! resolution).
//!
//! Entries are identified by [`FileId`], assigned in the order entries are
//! added. [`FileTable::load_all`] maps files in parallel but still assigns IDs
//! in input order, so IDs never depend on thread scheduling. Callers that add
//! entries from parallel code (extracted archive members) must add them in a
//! deterministic order themselves, for example sorted by archive position.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rayon::prelude::*;

use crate::error::{Error, Result};
use crate::ids::FileId;

use super::append::AppendVec;
use super::archive::{Archive, Member, MemberData};
use super::identify::{FileFormat, GccLtoProbe, identify_with};
use super::map::{self, Backing};
use super::read;

/// Something to load into a [`FileTable`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// A file on disk.
    Path(PathBuf),
    /// In-memory contents supplied by a library caller.
    Bytes {
        /// Name to use in diagnostics.
        name: PathBuf,
        /// The contents.
        data: Arc<[u8]>,
    },
}

/// One entry of a [`FileTable`]: a whole input file, or an archive member
/// registered as a file of its own.
#[derive(Debug)]
pub struct InputFile {
    path: PathBuf,
    member: Option<String>,
    parent: Option<FileId>,
    backing: Arc<Backing>,
    start: usize,
    end: usize,
    format: FileFormat,
}

impl InputFile {
    /// The file's path. For an archive member, the archive's path; for an
    /// in-memory input, its name.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The member name, when this entry is an archive member.
    #[must_use]
    pub fn member(&self) -> Option<&str> {
        self.member.as_deref()
    }

    /// The archive this member came from.
    #[must_use]
    pub fn parent(&self) -> Option<FileId> {
        self.parent
    }

    /// The contents.
    #[must_use]
    pub fn data(&self) -> &[u8] {
        self.backing
            .bytes()
            .get(self.start..self.end)
            .unwrap_or_default()
    }

    /// The format identified from the contents when the entry was added.
    #[must_use]
    pub fn format(&self) -> FileFormat {
        self.format
    }

    /// Whether the contents are backed by a file mapping (as opposed to a
    /// heap buffer).
    #[must_use]
    pub fn is_mapped(&self) -> bool {
        self.backing.is_mapped()
    }

    /// Creates an [`Error::Malformed`] naming this file (and member).
    pub fn malformed(&self, offset: u64, what: impl Into<String>) -> Error {
        Error::Malformed {
            file: self.path.clone(),
            member: self.member.clone(),
            offset,
            what: what.into(),
        }
    }

    /// Parses this entry as an `ar` archive.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Malformed`] if the contents are not a valid archive.
    pub fn archive(&self) -> Result<Archive<'_>> {
        Archive::parse(&self.path, self.data())
    }
}

/// All inputs of a link, identified by [`FileId`].
///
/// See the [module documentation](self).
#[derive(Debug, Default)]
pub struct FileTable {
    files: AppendVec<InputFile>,
    gcc_lto_probe: Option<GccLtoProbe>,
}

fn too_many_files(path: &Path) -> Error {
    Error::Limit(format!("too many input files (at {})", path.display()))
}

impl FileTable {
    /// Creates an empty table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates an empty table that uses `probe` to recognize GCC LTO IR in ELF
    /// inputs.
    #[must_use]
    pub fn with_gcc_lto_probe(probe: GccLtoProbe) -> Self {
        Self {
            files: AppendVec::new(),
            gcc_lto_probe: Some(probe),
        }
    }

    /// The number of entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.files.len()
    }

    /// Whether the table has no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The entry for `id`, if it exists.
    #[must_use]
    pub fn get(&self, id: FileId) -> Option<&InputFile> {
        self.files.get(id.index())
    }

    /// The contents of `id`, or an empty slice if it does not exist.
    #[must_use]
    pub fn data(&self, id: FileId) -> &[u8] {
        self.get(id).map_or(&[], InputFile::data)
    }

    /// Iterates over all entries in ID order.
    pub fn iter(&self) -> impl Iterator<Item = (FileId, &InputFile)> {
        (0..self.len()).map_while(|index| Some((FileId::new(index), self.files.get(index)?)))
    }

    fn push(&self, file: InputFile) -> Result<FileId> {
        let path = file.path.clone();
        match self.files.push(file) {
            Ok(index) => Ok(FileId::new(index)),
            Err(_) => Err(too_many_files(&path)),
        }
    }

    fn whole(&self, path: PathBuf, backing: Backing) -> InputFile {
        let end = backing.bytes().len();
        let format = identify_with(backing.bytes(), self.gcc_lto_probe);
        InputFile {
            path,
            member: None,
            parent: None,
            backing: Arc::new(backing),
            start: 0,
            end,
            format,
        }
    }

    /// Maps or reads the file at `path` and adds it.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if the file cannot be opened or read.
    pub fn load_path(&self, path: &Path) -> Result<FileId> {
        let backing = map::load(path).map_err(|error| Error::io(path, error))?;
        self.push(self.whole(path.to_path_buf(), backing))
    }

    /// Adds in-memory contents under `name`.
    ///
    /// # Errors
    ///
    /// Fails only when the table is full (more than `u32::MAX` entries).
    pub fn add_bytes(&self, name: impl Into<PathBuf>, data: Arc<[u8]>) -> Result<FileId> {
        self.push(self.whole(name.into(), Backing::Shared(data)))
    }

    /// Adds a [`Source`].
    ///
    /// # Errors
    ///
    /// As [`FileTable::load_path`] and [`FileTable::add_bytes`].
    pub fn load(&self, source: &Source) -> Result<FileId> {
        match source {
            Source::Path(path) => self.load_path(path),
            Source::Bytes { name, data } => self.add_bytes(name.clone(), Arc::clone(data)),
        }
    }

    /// Loads every source, mapping files in parallel on the current rayon
    /// pool, and returns one result per source in the same order.
    ///
    /// Successful sources get consecutive [`FileId`]s in source order, no
    /// matter which file finished mapping first.
    pub fn load_all(&self, sources: &[Source]) -> Vec<Result<FileId>> {
        let loaded: Vec<Result<InputFile>> = sources
            .par_iter()
            .map(|source| match source {
                Source::Path(path) => map::load(path)
                    .map(|backing| self.whole(path.clone(), backing))
                    .map_err(|error| Error::io(path, error)),
                Source::Bytes { name, data } => {
                    Ok(self.whole(name.clone(), Backing::Shared(Arc::clone(data))))
                }
            })
            .collect();
        loaded
            .into_iter()
            .map(|file| file.and_then(|file| self.push(file)))
            .collect()
    }

    /// Registers a member of the archive `archive` as an entry of its own.
    ///
    /// For a regular archive, the entry shares the archive's storage: no bytes
    /// are copied. For a thin archive, the member's file is mapped from disk.
    ///
    /// `member` must come from [`InputFile::archive`] on the same entry.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Malformed`] if `archive` does not exist or the member's
    /// range lies outside it, and [`Error::Io`] if a thin member's file cannot
    /// be loaded.
    pub fn add_member(&self, archive: FileId, member: &Member<'_>) -> Result<FileId> {
        let Some(parent) = self.get(archive) else {
            return Err(Error::malformed(
                "<unknown archive>",
                member.header_offset,
                "archive file ID",
            ));
        };
        let name = member.display_name();
        let file = match member.data {
            MemberData::Inline { offset, bytes } => {
                let bad = || parent.malformed(member.header_offset, "archive member range");
                let offset = read::to_usize(offset).ok_or_else(bad)?;
                let start = parent.start.checked_add(offset).ok_or_else(bad)?;
                let end = start.checked_add(bytes.len()).ok_or_else(bad)?;
                if end > parent.end {
                    return Err(bad());
                }
                let data = parent.backing.bytes().get(start..end).ok_or_else(bad)?;
                if !std::ptr::eq(data, bytes) {
                    // The member was read from some other buffer.
                    return Err(bad());
                }
                InputFile {
                    path: parent.path.clone(),
                    member: Some(name),
                    parent: Some(archive),
                    backing: Arc::clone(&parent.backing),
                    start,
                    end,
                    format: identify_with(data, self.gcc_lto_probe),
                }
            }
            MemberData::External { .. } => {
                let path = member.external_path().unwrap_or_default();
                let backing = map::load(&path).map_err(|error| Error::io(&path, error))?;
                let mut file = self.whole(path, backing);
                file.member = Some(name);
                file.parent = Some(archive);
                file
            }
        };
        self.push(file)
    }
}

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)] // Test code builds fixtures, not parses input.
mod tests {
    use super::*;
    use crate::input::identify::TextKind;

    #[test]
    fn bytes_inputs_and_lookup() {
        let table = FileTable::new();
        assert!(table.is_empty());
        let data: Arc<[u8]> = Arc::from(&b"INPUT(a.o)\n"[..]);
        let id = table.add_bytes("script", data).unwrap();
        assert_eq!(id, FileId::new(0));
        let file = table.get(id).unwrap();
        assert_eq!(file.data(), b"INPUT(a.o)\n");
        assert_eq!(file.format(), FileFormat::Text(TextKind::Other));
        assert_eq!(file.path(), Path::new("script"));
        assert!(!file.is_mapped());
        assert!(table.get(FileId::new(1)).is_none());
        assert_eq!(table.data(FileId::new(7)), b"");
    }

    #[test]
    fn load_all_keeps_source_order_and_reports_failures() {
        let sources: Vec<Source> = (0..50)
            .map(|i| {
                if i == 17 {
                    Source::Path(PathBuf::from("/nonexistent/qld/input/file.o"))
                } else {
                    Source::Bytes {
                        name: PathBuf::from(format!("in{i}")),
                        data: Arc::from(format!("text {i}").into_bytes()),
                    }
                }
            })
            .collect();
        let table = FileTable::new();
        let results = table.load_all(&sources);
        assert_eq!(results.len(), 50);
        assert!(matches!(results[17], Err(Error::Io { .. })));
        let mut expected = 0;
        for (i, result) in results.iter().enumerate() {
            if i == 17 {
                continue;
            }
            let id = *result.as_ref().unwrap();
            assert_eq!(id.index(), expected);
            expected += 1;
            assert_eq!(table.data(id), format!("text {i}").as_bytes());
        }
        assert_eq!(table.iter().count(), 49);
    }
}
