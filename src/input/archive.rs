//! `ar` archive reader.
//!
//! Supported layouts:
//!
//! - **GNU / SysV**: short names terminated by `/`, a `//` long-name table
//!   referenced as `/<offset>`, and a `/` symbol index with 32-bit
//!   big-endian member offsets, or `/SYM64/` with 64-bit offsets.
//! - **BSD / Darwin**: `#1/<len>` names stored at the start of the member data,
//!   and a `__.SYMDEF` (or `__.SYMDEF SORTED`) symbol index of `ranlib`
//!   entries, or `__.SYMDEF_64` with 64-bit fields.
//! - **COFF** (`lib.exe`, `llvm-ar --format=coff`): the GNU layout with a
//!   second linker member and NUL-terminated long names. The first linker
//!   member is used as the index; the second and the `/<ECSYMBOLS>/` member
//!   are skipped.
//! - **Thin archives** (`!<thin>\n`): the symbol index and long-name table are
//!   stored inline, but member contents are separate files, named relative to
//!   the directory containing the archive.
//!
//! The reader does not parse member contents. [`Archive::members`] yields each
//! member's name, header offset and byte range (or external path), and
//! [`Archive::member_at`] fetches the member a [`SymbolIndex`] entry points
//! to. An archive without a symbol index reports `None` from
//! [`Archive::symbol_index`], and the caller scans the members instead.
//!
//! [`Archive`] only borrows the archive bytes, so it is `Copy`, `Send` and
//! `Sync`: threads can look up and slice members concurrently.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

use super::read;

/// The archive magic for regular archives.
pub const MAGIC: &[u8; 8] = b"!<arch>\n";
/// The archive magic for thin archives.
pub const THIN_MAGIC: &[u8; 8] = b"!<thin>\n";

/// Size of an `ar` member header.
const HEADER_SIZE: usize = 60;
/// Size of the archive magic.
const MAGIC_SIZE: usize = 8;

/// Which `ar` dialect an archive is written in, judging from its special
/// members and member names.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArchiveKind {
    /// GNU / SysV: `name/`, `//`, `/`, `/SYM64/`.
    Gnu,
    /// BSD / Darwin: `#1/<len>`, `__.SYMDEF`.
    Bsd,
    /// Microsoft COFF: two `/` linker members.
    Coff,
}

/// The encoding of an archive's symbol index.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SymbolIndexKind {
    /// SysV `/` member: 32-bit big-endian offsets.
    Sysv32,
    /// SysV `/SYM64/` member: 64-bit big-endian offsets.
    Sysv64,
    /// BSD `__.SYMDEF`: 32-bit `ranlib` entries.
    Bsd32 {
        /// Whether the fields are big-endian (old PowerPC toolchains) rather
        /// than little-endian.
        big_endian: bool,
    },
    /// BSD `__.SYMDEF_64`: 64-bit `ranlib` entries.
    Bsd64 {
        /// Whether the fields are big-endian rather than little-endian.
        big_endian: bool,
    },
}

impl SymbolIndexKind {
    /// Size of one offset table entry.
    fn entry_size(self) -> usize {
        match self {
            Self::Sysv32 => 4,
            Self::Sysv64 => 8,
            Self::Bsd32 { .. } => 8,
            Self::Bsd64 { .. } => 16,
        }
    }
}

/// An archive's symbol index (armap): which member defines each symbol.
#[derive(Clone, Copy, Debug)]
pub struct SymbolIndex<'a> {
    kind: SymbolIndexKind,
    path: &'a Path,
    /// The offset table, exactly `len * kind.entry_size()` bytes.
    entries: &'a [u8],
    /// The string table.
    strings: &'a [u8],
    /// File offset of `strings`, for error messages.
    strings_offset: usize,
    len: usize,
}

/// One entry of a [`SymbolIndex`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArchiveSymbol<'a> {
    /// The symbol name, as raw bytes.
    pub name: &'a [u8],
    /// Offset of the defining member's header within the archive. Pass it to
    /// [`Archive::member_at`].
    pub member_offset: u64,
}

impl<'a> SymbolIndex<'a> {
    /// The index encoding.
    #[must_use]
    pub fn kind(&self) -> SymbolIndexKind {
        self.kind
    }

    /// The number of entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the index has no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Iterates over the entries in index order.
    ///
    /// A malformed entry yields one `Err` and ends the iteration.
    #[must_use]
    pub fn iter(&self) -> SymbolIter<'a> {
        SymbolIter {
            index: *self,
            next: 0,
            string_pos: 0,
            failed: false,
        }
    }

    fn malformed(&self, offset: usize, what: &str) -> Error {
        Error::malformed(self.path, read::to_u64(offset), what)
    }

    /// Reads the name starting at `start` in the string table: up to the next
    /// NUL, or the end of the table. Returns the name and the position after
    /// its terminator.
    fn name_at(&self, start: usize) -> Result<(&'a [u8], usize)> {
        let error = || self.malformed(self.strings_offset, "archive symbol table name");
        let rest = self.strings.get(start..).ok_or_else(error)?;
        if rest.is_empty() {
            return Err(error());
        }
        let len = rest.iter().position(|&b| b == 0).unwrap_or(rest.len());
        let name = rest.get(..len).ok_or_else(error)?;
        let next = start.saturating_add(len).saturating_add(1);
        Ok((name, next))
    }
}

impl<'a> IntoIterator for &SymbolIndex<'a> {
    type Item = Result<ArchiveSymbol<'a>>;
    type IntoIter = SymbolIter<'a>;

    fn into_iter(self) -> SymbolIter<'a> {
        self.iter()
    }
}

/// Iterator over a [`SymbolIndex`].
#[derive(Clone, Debug)]
pub struct SymbolIter<'a> {
    index: SymbolIndex<'a>,
    next: usize,
    string_pos: usize,
    failed: bool,
}

impl<'a> SymbolIter<'a> {
    fn entry(&mut self) -> Result<ArchiveSymbol<'a>> {
        let index = &self.index;
        let size = index.kind.entry_size();
        let at = self
            .next
            .checked_mul(size)
            .ok_or_else(|| index.malformed(0, "archive symbol table"))?;
        let bad_entry = || index.malformed(index.strings_offset, "archive symbol table entry");
        match index.kind {
            SymbolIndexKind::Sysv32 => {
                let member = read::u32_be(index.entries, at).ok_or_else(bad_entry)?;
                let (name, next) = index.name_at(self.string_pos)?;
                self.string_pos = next;
                Ok(ArchiveSymbol {
                    name,
                    member_offset: u64::from(member),
                })
            }
            SymbolIndexKind::Sysv64 => {
                let member = read::u64_be(index.entries, at).ok_or_else(bad_entry)?;
                let (name, next) = index.name_at(self.string_pos)?;
                self.string_pos = next;
                Ok(ArchiveSymbol {
                    name,
                    member_offset: member,
                })
            }
            SymbolIndexKind::Bsd32 { big_endian } => {
                let field = |offset: usize| {
                    let offset = at.checked_add(offset)?;
                    if big_endian {
                        read::u32_be(index.entries, offset)
                    } else {
                        read::u32_le(index.entries, offset)
                    }
                };
                let strx = field(0).ok_or_else(bad_entry)?;
                let member = field(4).ok_or_else(bad_entry)?;
                let strx = read::to_usize(u64::from(strx)).ok_or_else(bad_entry)?;
                let (name, _) = index.name_at(strx)?;
                Ok(ArchiveSymbol {
                    name,
                    member_offset: u64::from(member),
                })
            }
            SymbolIndexKind::Bsd64 { big_endian } => {
                let field = |offset: usize| {
                    let offset = at.checked_add(offset)?;
                    if big_endian {
                        read::u64_be(index.entries, offset)
                    } else {
                        read::u64_le(index.entries, offset)
                    }
                };
                let strx = field(0).ok_or_else(bad_entry)?;
                let member = field(8).ok_or_else(bad_entry)?;
                let strx = read::to_usize(strx).ok_or_else(bad_entry)?;
                let (name, _) = index.name_at(strx)?;
                Ok(ArchiveSymbol {
                    name,
                    member_offset: member,
                })
            }
        }
    }
}

impl<'a> Iterator for SymbolIter<'a> {
    type Item = Result<ArchiveSymbol<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed || self.next >= self.index.len {
            return None;
        }
        let result = self.entry();
        match result {
            Ok(_) => self.next = self.next.saturating_add(1),
            Err(_) => self.failed = true,
        }
        Some(result)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        if self.failed {
            return (0, Some(0));
        }
        (0, Some(self.index.len.saturating_sub(self.next)))
    }
}

/// Where a member's contents are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemberData<'a> {
    /// Stored inside the archive.
    Inline {
        /// File offset of the contents within the archive.
        offset: u64,
        /// The contents.
        bytes: &'a [u8],
    },
    /// A thin archive member: a separate file, named by the member name
    /// relative to `dir`, the directory that contains the archive.
    External {
        /// The directory containing the archive (empty for the current
        /// directory).
        dir: &'a Path,
    },
}

/// One archive member.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Member<'a> {
    /// The member name, with `ar` terminators and padding removed. For thin
    /// archives this is a path relative to the archive's directory.
    pub name: &'a [u8],
    /// Offset of the member header within the archive. Symbol indexes refer
    /// to members by this offset, and it orders members deterministically.
    pub header_offset: u64,
    /// Size of the contents, from the header (minus the name, for BSD `#1/`
    /// names). For thin archives, the size of the external file when the
    /// archive was created.
    pub size: u64,
    /// The contents, or where to find them.
    pub data: MemberData<'a>,
}

impl<'a> Member<'a> {
    /// The contents, for a member stored in the archive.
    #[must_use]
    pub fn bytes(&self) -> Option<&'a [u8]> {
        match self.data {
            MemberData::Inline { bytes, .. } => Some(bytes),
            MemberData::External { .. } => None,
        }
    }

    /// The path of the member's file, for a thin archive member.
    #[must_use]
    pub fn external_path(&self) -> Option<PathBuf> {
        match self.data {
            MemberData::Inline { .. } => None,
            MemberData::External { dir } => Some(dir.join(bytes_to_path(self.name))),
        }
    }

    /// The name for display, with invalid UTF-8 replaced.
    #[must_use]
    pub fn display_name(&self) -> String {
        String::from_utf8_lossy(self.name).into_owned()
    }
}

#[cfg(unix)]
fn bytes_to_path(bytes: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
}

#[cfg(not(unix))]
fn bytes_to_path(bytes: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
}

/// A parsed `ar` archive header.
#[derive(Clone, Copy, Debug)]
struct Header<'a> {
    offset: usize,
    name: &'a [u8],
    size: u64,
    data_start: usize,
}

/// What a header's name field says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RawName<'a> {
    /// `/`: SysV symbol index, or a COFF linker member.
    SymbolIndex,
    /// `/SYM64/`.
    SymbolIndex64,
    /// `//`: the long-name table.
    LongNames,
    /// `/<...>/`: another special member (`/<ECSYMBOLS>/`, …), skipped.
    OtherSpecial,
    /// `/<digits>`: an offset into the long-name table.
    LongName(usize),
    /// `#1/<digits>`: a BSD name of that length at the start of the data.
    BsdName(usize),
    /// An inline name, terminators removed.
    Short(&'a [u8]),
}

/// A parsed `ar` archive.
#[derive(Clone, Copy, Debug)]
pub struct Archive<'a> {
    data: &'a [u8],
    path: &'a Path,
    thin: bool,
    kind: ArchiveKind,
    long_names: Option<(&'a [u8], usize)>,
    index: Option<SymbolIndex<'a>>,
    first_member: usize,
}

fn parse_decimal(field: &[u8]) -> Option<u64> {
    let trimmed = trim_spaces(field);
    if trimmed.is_empty() {
        return None;
    }
    trimmed.iter().try_fold(0u64, |value, &byte| {
        if !byte.is_ascii_digit() {
            return None;
        }
        value
            .checked_mul(10)?
            .checked_add(u64::from(byte.wrapping_sub(b'0')))
    })
}

fn trim_spaces(field: &[u8]) -> &[u8] {
    let start = field.iter().position(|&b| b != b' ').unwrap_or(field.len());
    let end = field
        .iter()
        .rposition(|&b| b != b' ')
        .map_or(start, |p| p.saturating_add(1));
    field.get(start..end).unwrap_or_default()
}

fn trim_trailing(field: &[u8], byte: u8) -> &[u8] {
    let end = field
        .iter()
        .rposition(|&b| b != byte)
        .map_or(0, |p| p.saturating_add(1));
    field.get(..end).unwrap_or_default()
}

fn is_bsd_symdef(name: &[u8]) -> Option<bool> {
    match name {
        b"__.SYMDEF" | b"__.SYMDEF SORTED" => Some(false),
        b"__.SYMDEF_64" | b"__.SYMDEF_64 SORTED" => Some(true),
        _ => None,
    }
}

impl<'a> Archive<'a> {
    /// Parses the archive magic, symbol index and long-name table in `data`.
    ///
    /// `path` names the archive in errors and is the base for thin archive
    /// member paths. Members are not read until asked for.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Malformed`] if the magic is wrong, or if a special
    /// member (symbol index, long-name table) is truncated or inconsistent.
    pub fn parse(path: &'a Path, data: &'a [u8]) -> Result<Self> {
        let thin = if data.starts_with(MAGIC) {
            false
        } else if data.starts_with(THIN_MAGIC) {
            true
        } else {
            return Err(Error::malformed(path, 0, "archive magic"));
        };
        let mut archive = Archive {
            data,
            path,
            thin,
            kind: ArchiveKind::Gnu,
            long_names: None,
            index: None,
            first_member: MAGIC_SIZE,
        };
        let mut kind = None;
        let mut offset = MAGIC_SIZE;
        while offset < data.len() {
            let header = archive.header_at(offset)?;
            let raw = archive.raw_name(&header)?;
            match raw {
                RawName::SymbolIndex => {
                    if archive.index.is_none() {
                        archive.index = Some(archive.sysv_index(&header, false)?);
                        kind.get_or_insert(ArchiveKind::Gnu);
                    } else {
                        // The second linker member of a COFF archive.
                        kind = Some(ArchiveKind::Coff);
                    }
                }
                RawName::SymbolIndex64 => {
                    if archive.index.is_none() {
                        archive.index = Some(archive.sysv_index(&header, true)?);
                    }
                    kind.get_or_insert(ArchiveKind::Gnu);
                }
                RawName::LongNames => {
                    let (bytes, start) = archive.inline_data(&header)?;
                    archive.long_names = Some((bytes, start));
                    kind.get_or_insert(ArchiveKind::Gnu);
                }
                RawName::OtherSpecial => {}
                RawName::BsdName(_) | RawName::Short(_) => {
                    let (name, _, _) = archive.resolve_name(&header, raw)?;
                    let Some(is64) = is_bsd_symdef(name) else {
                        if kind.is_none() {
                            kind = Some(if matches!(raw, RawName::BsdName(_)) {
                                ArchiveKind::Bsd
                            } else if header.name.contains(&b'/') {
                                ArchiveKind::Gnu
                            } else {
                                ArchiveKind::Bsd
                            });
                        }
                        break;
                    };
                    if archive.index.is_none() {
                        archive.index = Some(archive.bsd_index(&header, raw, is64)?);
                    }
                    kind = Some(ArchiveKind::Bsd);
                }
                RawName::LongName(_) => {
                    kind.get_or_insert(ArchiveKind::Gnu);
                    break;
                }
            }
            offset = archive.next_offset(&header, true)?;
            archive.first_member = offset;
        }
        archive.kind = kind.unwrap_or(ArchiveKind::Gnu);
        Ok(archive)
    }

    /// The path the archive was opened with.
    #[must_use]
    pub fn path(&self) -> &'a Path {
        self.path
    }

    /// Whether this is a thin archive.
    #[must_use]
    pub fn is_thin(&self) -> bool {
        self.thin
    }

    /// The dialect the archive appears to be written in.
    #[must_use]
    pub fn kind(&self) -> ArchiveKind {
        self.kind
    }

    /// The symbol index, or `None` when the archive has none and the caller
    /// must scan its members to learn what they define.
    #[must_use]
    pub fn symbol_index(&self) -> Option<SymbolIndex<'a>> {
        self.index
    }

    /// Iterates over the regular members, in archive order, skipping special
    /// members.
    ///
    /// A malformed member yields one `Err` and ends the iteration.
    #[must_use]
    pub fn members(&self) -> Members<'a> {
        Members {
            archive: *self,
            offset: self.first_member,
            done: false,
        }
    }

    /// Returns the regular member whose header starts at `header_offset`, as
    /// named by a [`SymbolIndex`] entry.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Malformed`] if no valid member header is at that
    /// offset, or if it names a special member.
    pub fn member_at(&self, header_offset: u64) -> Result<Member<'a>> {
        let bad = || {
            Error::malformed(
                self.path,
                header_offset,
                "archive member offset in symbol table",
            )
        };
        let offset = read::to_usize(header_offset).ok_or_else(bad)?;
        if offset < self.first_member || offset >= self.data.len() {
            return Err(bad());
        }
        let header = self.header_at(offset)?;
        match self.member_from_header(&header)? {
            Some(member) => Ok(member),
            None => Err(bad()),
        }
    }

    fn malformed(&self, offset: usize, what: &str) -> Error {
        Error::malformed(self.path, read::to_u64(offset), what)
    }

    fn header_at(&self, offset: usize) -> Result<Header<'a>> {
        let raw = read::bytes(self.data, offset, HEADER_SIZE)
            .ok_or_else(|| self.malformed(offset, "archive member header (truncated)"))?;
        let field = |start: usize, len: usize| read::bytes(raw, start, len).unwrap_or_default();
        if field(58, 2) != b"`\n" {
            return Err(self.malformed(offset, "archive member header"));
        }
        let size = parse_decimal(field(48, 10))
            .ok_or_else(|| self.malformed(offset, "archive member size"))?;
        let data_start = offset
            .checked_add(HEADER_SIZE)
            .ok_or_else(|| self.malformed(offset, "archive member header"))?;
        Ok(Header {
            offset,
            name: field(0, 16),
            size,
            data_start,
        })
    }

    fn raw_name(&self, header: &Header<'a>) -> Result<RawName<'a>> {
        let name = trim_trailing(header.name, b' ');
        match name {
            b"/" => return Ok(RawName::SymbolIndex),
            b"//" => return Ok(RawName::LongNames),
            b"/SYM64/" => return Ok(RawName::SymbolIndex64),
            _ => {}
        }
        if let Some(rest) = name.strip_prefix(b"#1/") {
            let len = parse_decimal(rest)
                .and_then(read::to_usize)
                .ok_or_else(|| self.malformed(header.offset, "BSD archive member name length"))?;
            return Ok(RawName::BsdName(len));
        }
        if let Some(rest) = name.strip_prefix(b"/") {
            if rest.starts_with(b"<") && rest.ends_with(b">/") {
                return Ok(RawName::OtherSpecial);
            }
            let offset = parse_decimal(rest.strip_suffix(b"/").unwrap_or(rest))
                .and_then(read::to_usize)
                .ok_or_else(|| self.malformed(header.offset, "archive member name"))?;
            return Ok(RawName::LongName(offset));
        }
        Ok(RawName::Short(name.strip_suffix(b"/").unwrap_or(name)))
    }

    /// Resolves a member name. Returns the name, the file offset where the
    /// member contents start, and the content size.
    fn resolve_name(
        &self,
        header: &Header<'a>,
        raw: RawName<'a>,
    ) -> Result<(&'a [u8], usize, u64)> {
        match raw {
            RawName::Short(name) => Ok((name, header.data_start, header.size)),
            RawName::LongName(offset) => {
                let bad = || self.malformed(header.offset, "archive long member name reference");
                let (table, table_offset) = self.long_names.ok_or_else(bad)?;
                let rest = table
                    .get(offset..)
                    .filter(|r| !r.is_empty())
                    .ok_or_else(bad)?;
                let end = rest
                    .iter()
                    .position(|&b| b == b'\n' || b == 0)
                    .unwrap_or(rest.len());
                let name = rest.get(..end).ok_or_else(bad)?;
                let name = name.strip_suffix(b"/").unwrap_or(name);
                if name.is_empty() {
                    return Err(self.malformed(
                        table_offset.saturating_add(offset),
                        "archive long member name",
                    ));
                }
                Ok((name, header.data_start, header.size))
            }
            RawName::BsdName(len) => {
                let bad = || self.malformed(header.offset, "BSD archive member name");
                let len64 = read::to_u64(len);
                let size = header.size.checked_sub(len64).ok_or_else(bad)?;
                let name = read::bytes(self.data, header.data_start, len).ok_or_else(bad)?;
                let start = header.data_start.checked_add(len).ok_or_else(bad)?;
                Ok((trim_trailing(name, 0), start, size))
            }
            RawName::SymbolIndex
            | RawName::SymbolIndex64
            | RawName::LongNames
            | RawName::OtherSpecial => Err(self.malformed(header.offset, "archive member name")),
        }
    }

    /// The inline contents of a member: bytes and their file offset.
    fn inline_data(&self, header: &Header<'a>) -> Result<(&'a [u8], usize)> {
        let size = read::to_usize(header.size)
            .ok_or_else(|| self.malformed(header.offset, "archive member size"))?;
        let bytes = read::bytes(self.data, header.data_start, size).ok_or_else(|| {
            self.malformed(
                header.offset,
                "archive member size (extends past end of file)",
            )
        })?;
        Ok((bytes, header.data_start))
    }

    /// The offset of the header after `header`. `inline` says whether the
    /// member's contents are stored in the archive.
    fn next_offset(&self, header: &Header<'a>, inline: bool) -> Result<usize> {
        let end = if inline {
            let (bytes, start) = self.inline_data(header)?;
            start.checked_add(bytes.len())
        } else {
            Some(header.data_start)
        }
        .ok_or_else(|| self.malformed(header.offset, "archive member size"))?;
        // Members are padded to an even offset. Tolerate a missing pad byte
        // after the last member.
        let padded = end.checked_add(end & 1).unwrap_or(end);
        Ok(padded.min(self.data.len()).max(end))
    }

    /// Builds the member for a regular header, or `None` for a special member.
    fn member_from_header(&self, header: &Header<'a>) -> Result<Option<Member<'a>>> {
        let raw = self.raw_name(header)?;
        if matches!(
            raw,
            RawName::SymbolIndex
                | RawName::SymbolIndex64
                | RawName::LongNames
                | RawName::OtherSpecial
        ) {
            return Ok(None);
        }
        let (name, start, size) = self.resolve_name(header, raw)?;
        if is_bsd_symdef(name).is_some() {
            return Ok(None);
        }
        let data = if self.thin {
            MemberData::External {
                dir: self.path.parent().unwrap_or(Path::new("")),
            }
        } else {
            let bad = || {
                self.malformed(
                    header.offset,
                    "archive member size (extends past end of file)",
                )
            };
            let len = read::to_usize(size).ok_or_else(bad)?;
            let bytes = read::bytes(self.data, start, len).ok_or_else(bad)?;
            MemberData::Inline {
                offset: read::to_u64(start),
                bytes,
            }
        };
        Ok(Some(Member {
            name,
            header_offset: read::to_u64(header.offset),
            size,
            data,
        }))
    }

    fn sysv_index(&self, header: &Header<'a>, is64: bool) -> Result<SymbolIndex<'a>> {
        let (bytes, start) = self.inline_data(header)?;
        let bad = || self.malformed(start, "archive symbol table");
        let (count, width) = if is64 {
            (read::u64_be(bytes, 0).ok_or_else(bad)?, 8usize)
        } else {
            (u64::from(read::u32_be(bytes, 0).ok_or_else(bad)?), 4usize)
        };
        let count = read::to_usize(count).ok_or_else(bad)?;
        let table_len = count.checked_mul(width).ok_or_else(bad)?;
        let entries = read::bytes(bytes, width, table_len).ok_or_else(bad)?;
        let strings_start = width.checked_add(table_len).ok_or_else(bad)?;
        let strings = bytes.get(strings_start..).ok_or_else(bad)?;
        Ok(SymbolIndex {
            kind: if is64 {
                SymbolIndexKind::Sysv64
            } else {
                SymbolIndexKind::Sysv32
            },
            path: self.path,
            entries,
            strings,
            strings_offset: start.saturating_add(strings_start),
            len: count,
        })
    }

    fn bsd_index(
        &self,
        header: &Header<'a>,
        raw: RawName<'a>,
        is64: bool,
    ) -> Result<SymbolIndex<'a>> {
        let (_, start, size) = self.resolve_name(header, raw)?;
        let bad = || self.malformed(start, "BSD archive symbol table");
        let size = read::to_usize(size).ok_or_else(bad)?;
        let bytes = read::bytes(self.data, start, size).ok_or_else(bad)?;
        let (width, entry) = if is64 { (8usize, 16usize) } else { (4, 8) };
        let layout = |big_endian: bool| -> Option<SymbolIndex<'a>> {
            let field = |offset: usize| -> Option<usize> {
                let value = match (is64, big_endian) {
                    (false, false) => u64::from(read::u32_le(bytes, offset)?),
                    (false, true) => u64::from(read::u32_be(bytes, offset)?),
                    (true, false) => read::u64_le(bytes, offset)?,
                    (true, true) => read::u64_be(bytes, offset)?,
                };
                read::to_usize(value)
            };
            let ranlib_bytes = field(0)?;
            if ranlib_bytes.checked_rem(entry)? != 0 {
                return None;
            }
            let entries = read::bytes(bytes, width, ranlib_bytes)?;
            let strings_size_at = width.checked_add(ranlib_bytes)?;
            let strings_size = field(strings_size_at)?;
            let strings_start = strings_size_at.checked_add(width)?;
            let strings = read::bytes(bytes, strings_start, strings_size)?;
            Some(SymbolIndex {
                kind: if is64 {
                    SymbolIndexKind::Bsd64 { big_endian }
                } else {
                    SymbolIndexKind::Bsd32 { big_endian }
                },
                path: self.path,
                entries,
                strings,
                strings_offset: start.saturating_add(strings_start),
                len: ranlib_bytes.checked_div(entry)?,
            })
        };
        layout(false).or_else(|| layout(true)).ok_or_else(bad)
    }
}

/// Iterator over the regular members of an [`Archive`].
#[derive(Clone, Debug)]
pub struct Members<'a> {
    archive: Archive<'a>,
    offset: usize,
    done: bool,
}

impl<'a> Members<'a> {
    fn step(&mut self) -> Result<Option<Member<'a>>> {
        let archive = &self.archive;
        while self.offset < archive.data.len() {
            let header = archive.header_at(self.offset)?;
            let member = archive.member_from_header(&header)?;
            let inline = !archive.thin || member.is_none();
            self.offset = archive.next_offset(&header, inline)?;
            if member.is_some() {
                return Ok(member);
            }
        }
        Ok(None)
    }
}

impl<'a> Iterator for Members<'a> {
    type Item = Result<Member<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        match self.step() {
            Ok(Some(member)) => Some(Ok(member)),
            Ok(None) => {
                self.done = true;
                None
            }
            Err(error) => {
                self.done = true;
                Some(Err(error))
            }
        }
    }
}

impl std::iter::FusedIterator for Members<'_> {}
impl std::iter::FusedIterator for SymbolIter<'_> {}

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)] // Test code builds fixtures, not parses input.
mod tests {
    use super::*;

    /// Serializes one `ar` header.
    fn header(name: &[u8], size: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_SIZE);
        out.extend_from_slice(name);
        out.resize(16, b' ');
        let fields = format!("{:<12}{:<6}{:<6}{:<8}{:<10}`\n", 0, 0, 0, 644, size);
        out.extend_from_slice(fields.as_bytes());
        assert_eq!(out.len(), HEADER_SIZE);
        out
    }

    fn pad(out: &mut Vec<u8>) {
        if out.len() % 2 == 1 {
            out.push(b'\n');
        }
    }

    /// Builds a GNU archive. Names longer than 15 bytes (all names, for thin
    /// archives) go to the `//` table.
    fn gnu_archive(members: &[(&str, &[u8])], symbols: &[(&str, usize)], thin: bool) -> Vec<u8> {
        let mut long_names = Vec::new();
        let mut names = Vec::new();
        for (name, _) in members {
            if thin || name.len() > 15 {
                names.push(format!("/{}", long_names.len()).into_bytes());
                long_names.extend_from_slice(name.as_bytes());
                long_names.extend_from_slice(b"/\n");
            } else {
                names.push(format!("{name}/").into_bytes());
            }
        }
        let symtab_size =
            4 + 4 * symbols.len() + symbols.iter().map(|(s, _)| s.len() + 1).sum::<usize>();
        let mut offset = MAGIC_SIZE;
        if !symbols.is_empty() {
            offset += HEADER_SIZE + symtab_size + symtab_size % 2;
        }
        if !long_names.is_empty() {
            offset += HEADER_SIZE + long_names.len() + long_names.len() % 2;
        }
        let mut member_offsets = Vec::new();
        for (_, data) in members {
            member_offsets.push(offset);
            offset += HEADER_SIZE;
            if !thin {
                offset += data.len() + data.len() % 2;
            }
        }

        let mut out = if thin {
            THIN_MAGIC.to_vec()
        } else {
            MAGIC.to_vec()
        };
        if !symbols.is_empty() {
            out.extend(header(b"/", symtab_size));
            out.extend_from_slice(&u32::try_from(symbols.len()).unwrap().to_be_bytes());
            for (_, member) in symbols {
                let offset = u32::try_from(member_offsets[*member]).unwrap();
                out.extend_from_slice(&offset.to_be_bytes());
            }
            for (name, _) in symbols {
                out.extend_from_slice(name.as_bytes());
                out.push(0);
            }
            pad(&mut out);
        }
        if !long_names.is_empty() {
            out.extend(header(b"//", long_names.len()));
            out.extend_from_slice(&long_names);
            pad(&mut out);
        }
        for (i, ((_, data), name)) in members.iter().zip(&names).enumerate() {
            assert_eq!(out.len(), member_offsets[i]);
            out.extend(header(name, data.len()));
            if !thin {
                out.extend_from_slice(data);
                pad(&mut out);
            }
        }
        out
    }

    /// Builds a BSD archive the way macOS `libtool` does: `#1/<len>` names
    /// padded with NULs, and a `__.SYMDEF SORTED` index.
    fn bsd_archive(
        members: &[(&str, &[u8])],
        symbols: &[(&str, usize)],
        big_endian: bool,
        is64: bool,
    ) -> Vec<u8> {
        let width = if is64 { 8 } else { 4 };
        let word = |value: usize| -> Vec<u8> {
            match (is64, big_endian) {
                (false, false) => u32::try_from(value).unwrap().to_le_bytes().to_vec(),
                (false, true) => u32::try_from(value).unwrap().to_be_bytes().to_vec(),
                (true, false) => (value as u64).to_le_bytes().to_vec(),
                (true, true) => (value as u64).to_be_bytes().to_vec(),
            }
        };
        let padded_name = |name: &[u8]| {
            let mut bytes = name.to_vec();
            bytes.push(0);
            while !bytes.len().is_multiple_of(8) {
                bytes.push(0);
            }
            bytes
        };
        let mut strings = Vec::new();
        let mut strx = Vec::new();
        for (name, _) in symbols {
            strx.push(strings.len());
            strings.extend_from_slice(name.as_bytes());
            strings.push(0);
        }
        while strings.len() % width != 0 {
            strings.push(0);
        }
        let symdef_name = padded_name(if is64 {
            b"__.SYMDEF_64 SORTED"
        } else {
            b"__.SYMDEF SORTED"
        });
        let symdef_body = width + symbols.len() * 2 * width + width + strings.len();
        let symdef_size = symdef_name.len() + symdef_body;

        let mut offset = MAGIC_SIZE + HEADER_SIZE + symdef_size + symdef_size % 2;
        let mut member_offsets = Vec::new();
        let mut records = Vec::new();
        for (name, data) in members {
            member_offsets.push(offset);
            let name = padded_name(name.as_bytes());
            let mut record = header(
                format!("#1/{}", name.len()).as_bytes(),
                name.len() + data.len(),
            );
            record.extend_from_slice(&name);
            record.extend_from_slice(data);
            pad(&mut record);
            offset += record.len();
            records.push(record);
        }

        let mut out = MAGIC.to_vec();
        out.extend(header(
            format!("#1/{}", symdef_name.len()).as_bytes(),
            symdef_size,
        ));
        out.extend_from_slice(&symdef_name);
        out.extend(word(symbols.len() * 2 * width));
        for ((_, member), strx) in symbols.iter().zip(&strx) {
            out.extend(word(*strx));
            out.extend(word(member_offsets[*member]));
        }
        out.extend(word(strings.len()));
        out.extend_from_slice(&strings);
        pad(&mut out);
        for record in records {
            out.extend(record);
        }
        out
    }

    fn all_members<'a>(archive: &Archive<'a>) -> Vec<Member<'a>> {
        archive.members().collect::<Result<Vec<_>>>().unwrap()
    }

    fn all_symbols<'a>(archive: &Archive<'a>) -> Vec<(&'a [u8], u64)> {
        archive
            .symbol_index()
            .unwrap()
            .iter()
            .map(|s| s.map(|s| (s.name, s.member_offset)))
            .collect::<Result<Vec<_>>>()
            .unwrap()
    }

    const MEMBERS: &[(&str, &[u8])] = &[
        ("a.o", b"odd"),
        ("a_very_long_member_name.o", b"even"),
        ("c.o", b""),
    ];
    const SYMBOLS: &[(&str, usize)] = &[("foo", 0), ("bar", 1), ("baz", 0)];

    fn check_members_and_symbols(archive: &Archive<'_>) {
        let members = all_members(archive);
        let names: Vec<&[u8]> = members.iter().map(|m| m.name).collect();
        assert_eq!(names, [&b"a.o"[..], b"a_very_long_member_name.o", b"c.o"]);
        assert_eq!(members[0].bytes(), Some(&b"odd"[..]));
        assert_eq!(members[1].bytes(), Some(&b"even"[..]));
        assert_eq!(members[2].bytes(), Some(&b""[..]));
        let symbols = all_symbols(archive);
        assert_eq!(symbols.len(), 3);
        for ((name, offset), (expected, member)) in symbols.iter().zip(SYMBOLS) {
            assert_eq!(*name, expected.as_bytes());
            assert_eq!(*offset, members[*member].header_offset);
            assert_eq!(archive.member_at(*offset).unwrap(), members[*member]);
        }
    }

    #[test]
    fn gnu_archive_with_index_and_long_names() {
        let data = gnu_archive(MEMBERS, SYMBOLS, false);
        let archive = Archive::parse(Path::new("libx.a"), &data).unwrap();
        assert_eq!(archive.kind(), ArchiveKind::Gnu);
        assert!(!archive.is_thin());
        assert_eq!(
            archive.symbol_index().unwrap().kind(),
            SymbolIndexKind::Sysv32
        );
        check_members_and_symbols(&archive);
    }

    #[test]
    fn bsd_archives_all_encodings() {
        for (big_endian, is64) in [(false, false), (true, false), (false, true), (true, true)] {
            let data = bsd_archive(MEMBERS, SYMBOLS, big_endian, is64);
            let archive = Archive::parse(Path::new("libx.a"), &data).unwrap();
            assert_eq!(archive.kind(), ArchiveKind::Bsd);
            let expected = if is64 {
                SymbolIndexKind::Bsd64 { big_endian }
            } else {
                SymbolIndexKind::Bsd32 { big_endian }
            };
            assert_eq!(archive.symbol_index().unwrap().kind(), expected);
            check_members_and_symbols(&archive);
        }
    }

    #[test]
    fn thin_archive_members_are_external() {
        let data = gnu_archive(MEMBERS, SYMBOLS, true);
        let archive = Archive::parse(Path::new("dir/sub/libthin.a"), &data).unwrap();
        assert!(archive.is_thin());
        let members = all_members(&archive);
        assert_eq!(members.len(), 3);
        assert_eq!(members[1].size, 4);
        assert_eq!(members[1].bytes(), None);
        assert_eq!(
            members[1].external_path(),
            Some(Path::new("dir/sub").join("a_very_long_member_name.o"))
        );
        let symbols = all_symbols(&archive);
        assert_eq!(
            archive.member_at(symbols[1].1).unwrap().name,
            b"a_very_long_member_name.o"
        );

        let archive = Archive::parse(Path::new("libthin.a"), &data).unwrap();
        let member = archive.members().next().unwrap().unwrap();
        assert_eq!(member.external_path(), Some(PathBuf::from("a.o")));
    }

    #[test]
    fn archives_without_index() {
        let data = gnu_archive(MEMBERS, &[], false);
        let archive = Archive::parse(Path::new("x.a"), &data).unwrap();
        assert!(archive.symbol_index().is_none());
        assert_eq!(all_members(&archive).len(), 3);

        let archive = Archive::parse(Path::new("x.a"), MAGIC).unwrap();
        assert!(archive.symbol_index().is_none());
        assert!(archive.members().next().is_none());

        // BSD short names: no trailing slash.
        let mut data = MAGIC.to_vec();
        data.extend(header(b"a.o", 2));
        data.extend_from_slice(b"hi");
        let archive = Archive::parse(Path::new("x.a"), &data).unwrap();
        assert_eq!(archive.kind(), ArchiveKind::Bsd);
        assert_eq!(all_members(&archive)[0].name, b"a.o");
    }

    #[test]
    fn empty_index_is_still_an_index() {
        let mut data = MAGIC.to_vec();
        data.extend(header(b"/", 4));
        data.extend_from_slice(&[0, 0, 0, 0]);
        let archive = Archive::parse(Path::new("x.a"), &data).unwrap();
        let index = archive.symbol_index().unwrap();
        assert!(index.is_empty());
        assert_eq!(index.iter().count(), 0);
    }

    #[test]
    fn missing_final_padding_is_tolerated() {
        let mut data = MAGIC.to_vec();
        data.extend(header(b"a.o/", 3));
        data.extend_from_slice(b"odd");
        let archive = Archive::parse(Path::new("x.a"), &data).unwrap();
        assert_eq!(all_members(&archive)[0].bytes(), Some(&b"odd"[..]));
    }

    #[test]
    fn coff_archive_linker_members_and_nul_names() {
        let member_data = b"obj";
        let long = b"a_very_long_member_name.obj\0";
        let first_size = 4 + 4 + 4;
        let second_size = 4 + 4 + 4 + 2 + 4;
        let member_offset = MAGIC_SIZE
            + HEADER_SIZE
            + first_size
            + HEADER_SIZE
            + second_size
            + HEADER_SIZE
            + long.len();
        let mut data = MAGIC.to_vec();
        data.extend(header(b"/", first_size));
        data.extend_from_slice(&1u32.to_be_bytes());
        data.extend_from_slice(&u32::try_from(member_offset).unwrap().to_be_bytes());
        data.extend_from_slice(b"foo\0");
        data.extend(header(b"/", second_size));
        data.extend_from_slice(&1u32.to_le_bytes());
        data.extend_from_slice(&u32::try_from(member_offset).unwrap().to_le_bytes());
        data.extend_from_slice(&1u32.to_le_bytes());
        data.extend_from_slice(&1u16.to_le_bytes());
        data.extend_from_slice(b"foo\0");
        data.extend(header(b"//", long.len()));
        data.extend_from_slice(long);
        assert_eq!(data.len(), member_offset);
        data.extend(header(b"/0", member_data.len()));
        data.extend_from_slice(member_data);
        pad(&mut data);
        data.extend(header(b"/<ECSYMBOLS>/", 0));

        let archive = Archive::parse(Path::new("x.lib"), &data).unwrap();
        assert_eq!(archive.kind(), ArchiveKind::Coff);
        let members = all_members(&archive);
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].name, b"a_very_long_member_name.obj");
        let symbols = all_symbols(&archive);
        assert_eq!(symbols, [(&b"foo"[..], member_offset as u64)]);
    }

    fn parse_error(data: &[u8]) -> String {
        match Archive::parse(Path::new("bad.a"), data) {
            Err(error) => error.to_string(),
            Ok(archive) => match archive.members().find_map(Result::err) {
                Some(error) => error.to_string(),
                None => panic!("no error"),
            },
        }
    }

    #[test]
    fn malformed_archives_are_errors() {
        assert!(parse_error(b"!<arch>").contains("archive magic"));
        assert!(parse_error(b"not an archive").contains("archive magic"));

        let good = gnu_archive(MEMBERS, SYMBOLS, false);
        // Truncated header.
        assert!(parse_error(&good[..MAGIC_SIZE + 30]).contains("truncated"));
        // Bad terminator.
        let mut bad = good.clone();
        bad[MAGIC_SIZE + 58] = b'x';
        assert!(parse_error(&bad).contains("member header"));
        // Non-numeric size.
        let mut bad = good.clone();
        bad[MAGIC_SIZE + 48] = b'z';
        assert!(parse_error(&bad).contains("member size"));

        // Member size past the end of the file.
        let mut data = MAGIC.to_vec();
        data.extend(header(b"a.o/", 100));
        data.extend_from_slice(b"short");
        assert!(parse_error(&data).contains("past end"));

        // Long name reference without a table, and out of range.
        let mut data = MAGIC.to_vec();
        data.extend(header(b"/0", 0));
        assert!(parse_error(&data).contains("long member name"));
        let mut data = MAGIC.to_vec();
        data.extend(header(b"//", 4));
        data.extend_from_slice(b"ab/\n");
        data.extend(header(b"/99", 0));
        assert!(parse_error(&data).contains("long member name"));
        let mut data = MAGIC.to_vec();
        data.extend(header(b"/abc", 0));
        assert!(parse_error(&data).contains("member name"));

        // BSD name longer than the member.
        let mut data = MAGIC.to_vec();
        data.extend(header(b"#1/20", 4));
        data.extend_from_slice(b"abcd");
        assert!(parse_error(&data).contains("BSD archive member name"));

        // Symbol count larger than the table.
        let mut data = MAGIC.to_vec();
        data.extend(header(b"/", 8));
        data.extend_from_slice(&0xffff_ffffu32.to_be_bytes());
        data.extend_from_slice(&[0; 4]);
        assert!(parse_error(&data).contains("symbol table"));

        // BSD index whose sizes fit in neither byte order.
        let mut data = MAGIC.to_vec();
        data.extend(header(b"__.SYMDEF", 8));
        data.extend_from_slice(&[0x10, 0, 0, 0x10, 0, 0, 0, 0]);
        assert!(parse_error(&data).contains("BSD archive symbol table"));
    }

    #[test]
    fn long_name_reference_may_end_with_a_slash() {
        // Most producers write `/54`, but some pad the reference and close it
        // with a slash (`/54            /`). Both name the same string-table
        // offset.
        let mut data = MAGIC.to_vec();
        data.extend(header(b"//", 16));
        data.extend_from_slice(b"long_name.o/\n   ");
        for name in [&b"/0"[..], &b"/0             /"[..]] {
            let mut archive_data = data.clone();
            archive_data.extend(header(name, 4));
            archive_data.extend_from_slice(b"obj\n");
            let archive = Archive::parse(Path::new("x.a"), &archive_data).unwrap();
            let members = all_members(&archive);
            assert_eq!(members.len(), 1, "{:?}", String::from_utf8_lossy(name));
            assert_eq!(
                members[0].name,
                b"long_name.o",
                "{:?}",
                String::from_utf8_lossy(name)
            );
        }
    }

    #[test]
    fn bad_symbol_entries_and_member_offsets() {
        // One symbol with no name in the string table.
        let mut data = MAGIC.to_vec();
        data.extend(header(b"/", 8));
        data.extend_from_slice(&1u32.to_be_bytes());
        data.extend_from_slice(&0u32.to_be_bytes());
        let archive = Archive::parse(Path::new("x.a"), &data).unwrap();
        let mut iter = archive.symbol_index().unwrap().iter();
        assert!(iter.next().unwrap().is_err());
        assert!(iter.next().is_none());

        let data = gnu_archive(MEMBERS, SYMBOLS, false);
        let archive = Archive::parse(Path::new("x.a"), &data).unwrap();
        // Points at the symbol table member, into the middle of a header, and
        // past the end.
        assert!(archive.member_at(8).is_err());
        let first = all_members(&archive)[0].header_offset;
        assert!(archive.member_at(first + 1).is_err());
        assert!(archive.member_at(data.len() as u64).is_err());
        assert!(archive.member_at(u64::MAX).is_err());

        // BSD symbol with a string index out of range.
        let mut data = bsd_archive(MEMBERS, SYMBOLS, false, false);
        let name_len = 24; // "__.SYMDEF SORTED" plus NUL, padded to 8
        let strx_at = MAGIC_SIZE + HEADER_SIZE + name_len + 4;
        data[strx_at..strx_at + 4].copy_from_slice(&0x7fff_ffffu32.to_le_bytes());
        let archive = Archive::parse(Path::new("x.a"), &data).unwrap();
        let first = archive.symbol_index().unwrap().iter().next().unwrap();
        assert!(first.is_err());
    }

    #[test]
    fn archive_is_send_sync_copy() {
        fn assert_traits<T: Send + Sync + Copy>() {}
        assert_traits::<Archive<'static>>();
        assert_traits::<SymbolIndex<'static>>();
        assert_traits::<Member<'static>>();
    }

    /// A small deterministic PRNG (xorshift64*), so the corruption tests need
    /// no dependency and always exercise the same inputs.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    /// Exercises every reader entry point. Errors are fine; panics are not.
    fn exercise(data: &[u8]) {
        let Ok(archive) = Archive::parse(Path::new("fuzz.a"), data) else {
            return;
        };
        for member in archive.members().take(1000) {
            let Ok(member) = member else { break };
            let _ = member.external_path();
            let _ = member.display_name();
        }
        if let Some(index) = archive.symbol_index() {
            for symbol in index.iter().take(1000) {
                let Ok(symbol) = symbol else { break };
                let _ = archive.member_at(symbol.member_offset);
            }
        }
    }

    #[test]
    fn randomized_truncation_and_corruption_never_panics() {
        let fixtures = [
            gnu_archive(MEMBERS, SYMBOLS, false),
            gnu_archive(MEMBERS, SYMBOLS, true),
            bsd_archive(MEMBERS, SYMBOLS, false, false),
            bsd_archive(MEMBERS, SYMBOLS, true, true),
        ];
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        for fixture in &fixtures {
            for len in 0..=fixture.len() {
                exercise(&fixture[..len]);
            }
            for _ in 0..3000 {
                let mut data = fixture.clone();
                for _ in 0..=rng.below(4) {
                    let at = rng.below(data.len());
                    data[at] = match rng.below(4) {
                        0 => rng.next() as u8,
                        1 => b'9',
                        2 => b' ',
                        _ => data[at] ^ (1 << rng.below(8)),
                    };
                }
                let len = if rng.below(4) == 0 {
                    rng.below(data.len() + 1)
                } else {
                    data.len()
                };
                exercise(&data[..len]);
            }
        }
    }
}
