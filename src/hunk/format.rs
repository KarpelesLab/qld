//! The AmigaDOS Hunk container: block numbers, encoding and decoding.
//!
//! A Hunk file is a stream of big-endian 32-bit longwords. Every block
//! starts with its type; a load file starts with [`HUNK_HEADER`], which
//! names the hunks and their sizes, and is followed by one block per hunk
//! ([`HUNK_CODE`], [`HUNK_DATA`] or [`HUNK_BSS`]), each optionally trailed
//! by relocation ([`HUNK_RELOC32`], [`HUNK_RELOC32SHORT`]) and symbol
//! ([`HUNK_SYMBOL`]) blocks and closed by [`HUNK_END`].
//!
//! Sizes are counted in longwords, not bytes, and the two high bits of a
//! size in the header hold the memory attribute the loader allocates the
//! hunk with ([`MemFlags`]).
//!
//! Names (in [`HUNK_SYMBOL`], [`HUNK_NAME`] and [`HUNK_UNIT`]) are stored
//! as a longword count followed by that many longwords of characters,
//! padded with zero bytes; a zero count ends a list.

#![deny(clippy::arithmetic_side_effects)]

use std::path::Path;

use crate::error::{Error, Result};

/// `HUNK_UNIT`: names an object module (Hunk object files).
pub const HUNK_UNIT: u32 = 999;
/// `HUNK_NAME`: names the hunk that follows.
pub const HUNK_NAME: u32 = 1000;
/// `HUNK_CODE`: executable contents.
pub const HUNK_CODE: u32 = 1001;
/// `HUNK_DATA`: initialized data.
pub const HUNK_DATA: u32 = 1002;
/// `HUNK_BSS`: zero-filled space, with no contents in the file.
pub const HUNK_BSS: u32 = 1003;
/// `HUNK_RELOC32`: 32-bit absolute relocations, longword counts and offsets.
pub const HUNK_RELOC32: u32 = 1004;
/// `HUNK_RELOC16`: 16-bit absolute relocations (object files).
pub const HUNK_RELOC16: u32 = 1005;
/// `HUNK_RELOC8`: 8-bit absolute relocations (object files).
pub const HUNK_RELOC8: u32 = 1006;
/// `HUNK_EXT`: external definitions and references (object files).
pub const HUNK_EXT: u32 = 1007;
/// `HUNK_SYMBOL`: the symbol table of one hunk.
pub const HUNK_SYMBOL: u32 = 1008;
/// `HUNK_DEBUG`: opaque debugging information.
pub const HUNK_DEBUG: u32 = 1009;
/// `HUNK_END`: ends a hunk.
pub const HUNK_END: u32 = 1010;
/// `HUNK_HEADER`: starts a load file.
pub const HUNK_HEADER: u32 = 1011;
/// `HUNK_OVERLAY`: the overlay table of an overlaid load file.
pub const HUNK_OVERLAY: u32 = 1013;
/// `HUNK_BREAK`: ends one overlay node.
pub const HUNK_BREAK: u32 = 1014;
/// `HUNK_DREL32`: 32-bit relocations with 16-bit offsets. In a load file
/// this number is [`HUNK_RELOC32SHORT`]'s older spelling, which is what
/// vlink writes for OS 2.0 compatibility.
pub const HUNK_DREL32: u32 = 1015;
/// `HUNK_DREL16`.
pub const HUNK_DREL16: u32 = 1016;
/// `HUNK_DREL8`.
pub const HUNK_DREL8: u32 = 1017;
/// `HUNK_LIB`.
pub const HUNK_LIB: u32 = 1018;
/// `HUNK_INDEX`.
pub const HUNK_INDEX: u32 = 1019;
/// `HUNK_RELOC32SHORT`: 32-bit relocations whose counts and offsets are
/// 16-bit words (AmigaOS 3.0 and later).
pub const HUNK_RELOC32SHORT: u32 = 1020;
/// `HUNK_RELRELOC32`: 32-bit PC-relative relocations.
pub const HUNK_RELRELOC32: u32 = 1021;
/// `HUNK_ABSRELOC16`: 16-bit absolute relocations in a load file.
pub const HUNK_ABSRELOC16: u32 = 1022;

/// The bit that marks a hunk's memory attribute as `MEMF_CHIP`.
pub const HUNKF_CHIP: u32 = 1 << 30;
/// The bit that marks a hunk's memory attribute as `MEMF_FAST`.
pub const HUNKF_FAST: u32 = 1 << 31;
/// Both attribute bits set: an explicit `MEMF_*` longword follows the size.
pub const HUNKF_MEMTYPE: u32 = HUNKF_CHIP | HUNKF_FAST;

/// The memory a hunk is loaded into.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MemFlags {
    /// Any memory (`MEMF_PUBLIC`).
    #[default]
    Any,
    /// Chip memory (`MEMF_CHIP`), which the custom chips can address.
    Chip,
    /// Fast memory (`MEMF_FAST`).
    Fast,
    /// An explicit `MEMF_*` mask, stored in a longword of its own.
    Explicit(u32),
}

impl MemFlags {
    /// The bits this attribute sets in a header size longword.
    #[must_use]
    pub fn bits(self) -> u32 {
        match self {
            Self::Any => 0,
            Self::Chip => HUNKF_CHIP,
            Self::Fast => HUNKF_FAST,
            Self::Explicit(_) => HUNKF_MEMTYPE,
        }
    }

    /// The attribute the high bits of a header size longword name.
    #[must_use]
    pub fn from_bits(size: u32) -> Self {
        match size & HUNKF_MEMTYPE {
            HUNKF_CHIP => Self::Chip,
            HUNKF_FAST => Self::Fast,
            HUNKF_MEMTYPE => Self::Explicit(0),
            _ => Self::Any,
        }
    }
}

/// What a hunk holds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    /// Executable code ([`HUNK_CODE`]).
    #[default]
    Code,
    /// Initialized data ([`HUNK_DATA`]).
    Data,
    /// Zero-filled space ([`HUNK_BSS`]).
    Bss,
}

impl Kind {
    /// The block number that introduces a hunk of this kind.
    #[must_use]
    pub fn block(self) -> u32 {
        match self {
            Self::Code => HUNK_CODE,
            Self::Data => HUNK_DATA,
            Self::Bss => HUNK_BSS,
        }
    }

    /// The kind block number `block` introduces.
    #[must_use]
    pub fn from_block(block: u32) -> Option<Self> {
        match block {
            HUNK_CODE => Some(Self::Code),
            HUNK_DATA => Some(Self::Data),
            HUNK_BSS => Some(Self::Bss),
            _ => None,
        }
    }
}

/// How relocations are stored.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RelocForm {
    /// [`HUNK_RELOC32`]: longword counts and offsets, which every AmigaOS
    /// release loads. vlink's default, and qld's.
    #[default]
    Long,
    /// [`HUNK_RELOC32SHORT`]: 16-bit counts and offsets, padded to a
    /// longword. Needs AmigaOS 3.0 (`-hunkattr`-style short relocs).
    Short,
}

/// Appends a big-endian longword.
fn put(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_be_bytes());
}

/// Appends a name as a longword count followed by the padded characters.
/// A name longer than `u32::MAX` longwords cannot be encoded, and one that
/// is empty is written as a zero count, which readers take for a
/// terminator; callers must not pass one.
fn put_name(out: &mut Vec<u8>, name: &[u8]) -> Result<()> {
    let longs = name.len().div_ceil(4);
    let count =
        u32::try_from(longs).map_err(|_| Error::Limit("a Hunk symbol name is too long".into()))?;
    put(out, count);
    out.extend_from_slice(name);
    let padded = longs.saturating_mul(4);
    out.resize(
        out.len().saturating_add(padded.saturating_sub(name.len())),
        0,
    );
    Ok(())
}

/// One hunk of a load file, as [`write`] receives it.
#[derive(Clone, Debug, Default)]
pub struct Hunk {
    /// What it holds.
    pub kind: Kind,
    /// Bytes the loader allocates for it; rounded up to a longword when it
    /// is written.
    pub alloc: u64,
    /// Its contents, which may be shorter than `alloc`: the loader zeroes
    /// the rest. Empty for [`Kind::Bss`].
    pub data: Vec<u8>,
    /// Memory attribute.
    pub memory: MemFlags,
    /// 32-bit absolute relocations, as (target hunk, offsets in this
    /// hunk); offsets are sorted and each target appears once.
    pub relocs: Vec<(u32, Vec<u32>)>,
    /// Symbols defined in this hunk, as (name, offset).
    pub symbols: Vec<(Vec<u8>, u32)>,
}

impl Hunk {
    /// The number of longwords the loader allocates.
    ///
    /// # Errors
    ///
    /// [`Error::Limit`] when the hunk is larger than 16 GiB, which no
    /// Hunk size longword can name.
    pub fn alloc_longs(&self) -> Result<u32> {
        u32::try_from(self.alloc.div_ceil(4))
            .map_err(|_| Error::Limit("a Hunk hunk is larger than a size longword can name".into()))
    }
}

/// Encodes a load file: [`HUNK_HEADER`], then every hunk with its
/// relocation and symbol blocks and [`HUNK_END`].
///
/// Trailing longwords of a hunk's contents that are entirely zero are left
/// out, as vlink does: the loader zeroes whatever the block does not fill.
///
/// # Errors
///
/// [`Error::Limit`] when a hunk, a name or an offset does not fit the
/// fields the format has for it.
pub fn write(hunks: &[Hunk], form: RelocForm) -> Result<Vec<u8>> {
    let count = u32::try_from(hunks.len())
        .map_err(|_| Error::Limit("a Hunk load file has too many hunks".into()))?;
    let mut out = Vec::new();
    put(&mut out, HUNK_HEADER);
    // No resident library names.
    put(&mut out, 0);
    put(&mut out, count);
    put(&mut out, 0);
    put(&mut out, count.saturating_sub(1));
    for hunk in hunks {
        let longs = hunk.alloc_longs()?;
        if longs & HUNKF_MEMTYPE != 0 {
            return Err(Error::Limit(
                "a Hunk hunk is larger than the memory attribute bits allow".into(),
            ));
        }
        put(&mut out, longs | hunk.memory.bits());
        if let MemFlags::Explicit(mask) = hunk.memory {
            put(&mut out, mask);
        }
    }
    for hunk in hunks {
        put(&mut out, hunk.kind.block());
        match hunk.kind {
            Kind::Bss => put(&mut out, hunk.alloc_longs()?),
            _ => {
                let kept = content_longs(&hunk.data);
                let longs = u32::try_from(kept).map_err(|_| {
                    Error::Limit("a Hunk hunk is larger than a size longword can name".into())
                })?;
                put(&mut out, longs);
                let bytes = kept.saturating_mul(4).min(hunk.data.len());
                let data = hunk.data.get(..bytes).unwrap_or(&hunk.data);
                out.extend_from_slice(data);
                out.resize(
                    out.len()
                        .saturating_add(kept.saturating_mul(4).saturating_sub(data.len())),
                    0,
                );
            }
        }
        write_relocs(&mut out, &hunk.relocs, form)?;
        if !hunk.symbols.is_empty() {
            put(&mut out, HUNK_SYMBOL);
            for (name, value) in &hunk.symbols {
                put_name(&mut out, name)?;
                put(&mut out, *value);
            }
            put(&mut out, 0);
        }
        put(&mut out, HUNK_END);
    }
    Ok(out)
}

/// How many longwords of `data` the block holds: the contents padded to a
/// longword, without the trailing longwords that are entirely zero. The
/// loader zeroes whatever the block leaves out, and vlink trims the same
/// way, so the two agree byte for byte.
fn content_longs(data: &[u8]) -> usize {
    let mut longs = data.len().div_ceil(4);
    while longs > 0 {
        let start = longs.saturating_sub(1).saturating_mul(4);
        let end = start.saturating_add(4).min(data.len());
        // Bytes past the end are the zero padding of the last longword.
        if !data.get(start..end).is_none_or(is_zero) {
            break;
        }
        longs = longs.saturating_sub(1);
    }
    longs
}

fn is_zero(bytes: &[u8]) -> bool {
    bytes.iter().all(|&b| b == 0)
}

/// Writes the relocation blocks of one hunk, if it has any.
fn write_relocs(out: &mut Vec<u8>, relocs: &[(u32, Vec<u32>)], form: RelocForm) -> Result<()> {
    if relocs.iter().all(|(_, offsets)| offsets.is_empty()) {
        return Ok(());
    }
    match form {
        RelocForm::Long => {
            put(out, HUNK_RELOC32);
            for (target, offsets) in relocs {
                if offsets.is_empty() {
                    continue;
                }
                let count = u32::try_from(offsets.len()).map_err(|_| {
                    Error::Limit("too many Hunk relocations for one target hunk".into())
                })?;
                put(out, count);
                put(out, *target);
                for &offset in offsets {
                    put(out, offset);
                }
            }
            put(out, 0);
        }
        RelocForm::Short => {
            put(out, HUNK_RELOC32SHORT);
            let start = out.len();
            for (target, offsets) in relocs {
                if offsets.is_empty() {
                    continue;
                }
                let count = u16::try_from(offsets.len()).map_err(|_| {
                    Error::Limit("too many Hunk short relocations for one target hunk".into())
                })?;
                let target = u16::try_from(*target)
                    .map_err(|_| Error::Limit("too many hunks for short relocations".into()))?;
                out.extend_from_slice(&count.to_be_bytes());
                out.extend_from_slice(&target.to_be_bytes());
                for &offset in offsets {
                    let offset = u16::try_from(offset).map_err(|_| {
                        Error::Limit("a Hunk short relocation offset is over 64 KiB".into())
                    })?;
                    out.extend_from_slice(&offset.to_be_bytes());
                }
            }
            out.extend_from_slice(&0u16.to_be_bytes());
            // The block is padded to a longword boundary.
            if !out.len().wrapping_sub(start).is_multiple_of(4) {
                out.extend_from_slice(&0u16.to_be_bytes());
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Decoding
// ---------------------------------------------------------------------------

/// A decoded load file.
#[derive(Clone, Debug, Default)]
pub struct LoadFile {
    /// Names of resident libraries the loader must open first.
    pub residents: Vec<Vec<u8>>,
    /// Allocation size, in bytes, and memory attribute of every hunk, as
    /// the header names them.
    pub sizes: Vec<(u64, MemFlags)>,
    /// First hunk of the table, from the header.
    pub first: u32,
    /// Last hunk of the table, from the header.
    pub last: u32,
    /// The hunks themselves.
    pub hunks: Vec<Hunk>,
}

/// Reads big-endian longwords out of a buffer, never panicking and never
/// reading past the end.
struct Reader<'b> {
    data: &'b [u8],
    at: usize,
    /// The file being read, for diagnostics.
    file: &'b Path,
}

impl<'b> Reader<'b> {
    fn long(&mut self) -> Result<u32> {
        let end = self
            .at
            .checked_add(4)
            .ok_or_else(|| self.truncated("a longword"))?;
        let bytes = self
            .data
            .get(self.at..end)
            .ok_or_else(|| self.truncated("a longword"))?;
        self.at = end;
        let mut word = [0u8; 4];
        word.copy_from_slice(bytes);
        Ok(u32::from_be_bytes(word))
    }

    fn word(&mut self) -> Result<u16> {
        let end = self
            .at
            .checked_add(2)
            .ok_or_else(|| self.truncated("a word"))?;
        let bytes = self
            .data
            .get(self.at..end)
            .ok_or_else(|| self.truncated("a word"))?;
        self.at = end;
        let mut half = [0u8; 2];
        half.copy_from_slice(bytes);
        Ok(u16::from_be_bytes(half))
    }

    fn bytes(&mut self, len: usize) -> Result<&'b [u8]> {
        let end = self
            .at
            .checked_add(len)
            .ok_or_else(|| self.truncated("a block's contents"))?;
        let bytes = self
            .data
            .get(self.at..end)
            .ok_or_else(|| self.truncated("a block's contents"))?;
        self.at = end;
        Ok(bytes)
    }

    /// A name: a longword count of longwords, then the padded characters.
    /// `None` at a zero count, which ends a list.
    fn name(&mut self) -> Result<Option<&'b [u8]>> {
        let longs = self.long()?;
        if longs == 0 {
            return Ok(None);
        }
        let len = (longs as usize)
            .checked_mul(4)
            .ok_or_else(|| self.truncated("a name"))?;
        let bytes = self.bytes(len)?;
        let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
        Ok(Some(bytes.get(..end).unwrap_or(bytes)))
    }

    fn done(&self) -> bool {
        self.at >= self.data.len()
    }

    fn truncated(&self, what: &str) -> Error {
        self.bad(format!("{what}, which the file ends in the middle of"))
    }

    fn bad(&self, what: String) -> Error {
        Error::Malformed {
            file: self.file.to_path_buf(),
            member: None,
            offset: self.at as u64,
            what,
        }
    }
}

/// Decodes a Hunk load file.
///
/// The decoder is for tests and diagnostics, not a hot path: it copies the
/// contents it returns. It never panics on malformed input.
///
/// # Errors
///
/// [`Error::Malformed`] when the file is truncated, does not start with
/// [`HUNK_HEADER`], or holds a block the load-file grammar does not allow.
pub fn read(data: &[u8], file: &Path) -> Result<LoadFile> {
    let mut r = Reader { data, at: 0, file };
    if r.long()? != HUNK_HEADER {
        return Err(r.bad("not a Hunk load file: no HUNK_HEADER".into()));
    }
    let mut file = LoadFile::default();
    while let Some(name) = r.name()? {
        file.residents.push(name.to_vec());
    }
    let table = r.long()?;
    file.first = r.long()?;
    file.last = r.long()?;
    for _ in 0..table {
        let size = r.long()?;
        let memory = match MemFlags::from_bits(size) {
            MemFlags::Explicit(_) => MemFlags::Explicit(r.long()?),
            other => other,
        };
        file.sizes
            .push((u64::from(size & !HUNKF_MEMTYPE).saturating_mul(4), memory));
    }
    while !r.done() {
        let block = r.long()?;
        let Some(kind) = Kind::from_block(block) else {
            if block == HUNK_NAME {
                let _ = r.name()?;
                continue;
            }
            return Err(r.bad(format!("unexpected Hunk block {block} where a hunk starts")));
        };
        let longs = r.long()?;
        let len = (longs as usize)
            .checked_mul(4)
            .ok_or_else(|| r.truncated("a hunk"))?;
        let index = file.hunks.len();
        let (alloc, memory) = file
            .sizes
            .get(index)
            .copied()
            .unwrap_or((len as u64, MemFlags::Any));
        let mut hunk = Hunk {
            kind,
            alloc,
            data: Vec::new(),
            memory,
            relocs: Vec::new(),
            symbols: Vec::new(),
        };
        if kind != Kind::Bss {
            hunk.data = r.bytes(len)?.to_vec();
        }
        loop {
            let block = r.long()?;
            match block {
                HUNK_END => break,
                HUNK_RELOC32 => loop {
                    let count = r.long()?;
                    if count == 0 {
                        break;
                    }
                    let target = r.long()?;
                    let mut offsets = Vec::with_capacity(count as usize);
                    for _ in 0..count {
                        offsets.push(r.long()?);
                    }
                    hunk.relocs.push((target, offsets));
                },
                HUNK_RELOC32SHORT | HUNK_DREL32 => {
                    let start = r.at;
                    loop {
                        let count = r.word()?;
                        if count == 0 {
                            break;
                        }
                        let target = u32::from(r.word()?);
                        let mut offsets = Vec::with_capacity(count as usize);
                        for _ in 0..count {
                            offsets.push(u32::from(r.word()?));
                        }
                        hunk.relocs.push((target, offsets));
                    }
                    if !r.at.wrapping_sub(start).is_multiple_of(4) {
                        let _ = r.word()?;
                    }
                }
                HUNK_SYMBOL => {
                    while let Some(name) = r.name()? {
                        let name = name.to_vec();
                        hunk.symbols.push((name, r.long()?));
                    }
                }
                HUNK_DEBUG => {
                    let longs = r.long()?;
                    let len = (longs as usize)
                        .checked_mul(4)
                        .ok_or_else(|| r.truncated("a debug block"))?;
                    let _ = r.bytes(len)?;
                }
                other => {
                    return Err(r.bad(format!("unexpected Hunk block {other} inside a hunk")));
                }
            }
        }
        file.hunks.push(hunk);
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<Hunk> {
        vec![
            Hunk {
                kind: Kind::Code,
                alloc: 12,
                data: vec![0x4e, 0x75, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
                memory: MemFlags::Any,
                relocs: vec![(1, vec![4])],
                symbols: vec![(b"_start".to_vec(), 0)],
            },
            Hunk {
                kind: Kind::Data,
                alloc: 8,
                data: vec![1, 2, 3, 4, 0, 0, 0, 0],
                memory: MemFlags::Chip,
                relocs: Vec::new(),
                symbols: Vec::new(),
            },
            Hunk {
                kind: Kind::Bss,
                alloc: 16,
                data: Vec::new(),
                memory: MemFlags::Any,
                relocs: Vec::new(),
                symbols: vec![(b"buffer".to_vec(), 0)],
            },
        ]
    }

    #[test]
    fn round_trip() {
        let bytes = write(&sample(), RelocForm::Long).expect("write");
        let file = read(&bytes, Path::new("test.hunk")).expect("read");
        assert_eq!(file.sizes.len(), 3);
        assert_eq!(file.first, 0);
        assert_eq!(file.last, 2);
        assert_eq!(file.sizes[0], (12, MemFlags::Any));
        assert_eq!(file.sizes[1], (8, MemFlags::Chip));
        assert_eq!(file.sizes[2], (16, MemFlags::Any));
        // Trailing zero longwords are left out of the block.
        assert_eq!(file.hunks[0].data, vec![0x4e, 0x75, 0, 0]);
        assert_eq!(file.hunks[0].relocs, vec![(1, vec![4])]);
        assert_eq!(file.hunks[0].symbols, vec![(b"_start".to_vec(), 0)]);
        assert_eq!(file.hunks[1].data, vec![1, 2, 3, 4]);
        assert_eq!(file.hunks[2].kind, Kind::Bss);
        assert_eq!(file.hunks[2].symbols, vec![(b"buffer".to_vec(), 0)]);
    }

    #[test]
    fn short_relocs_round_trip() {
        let bytes = write(&sample(), RelocForm::Short).expect("write");
        let file = read(&bytes, Path::new("test.hunk")).expect("read");
        assert_eq!(file.hunks[0].relocs, vec![(1, vec![4])]);
    }

    #[test]
    fn truncation_is_an_error_not_a_panic() {
        let bytes = write(&sample(), RelocForm::Long).expect("write");
        for cut in 0..bytes.len() {
            let _ = read(bytes.get(..cut).expect("in range"), Path::new("test.hunk"));
        }
    }

    #[test]
    fn garbage_is_an_error_not_a_panic() {
        for seed in 0u32..512 {
            let mut bytes = write(&sample(), RelocForm::Long).expect("write");
            // A cheap deterministic mutation of one longword.
            let at = (seed as usize).wrapping_mul(4) % bytes.len();
            for (i, byte) in bytes.iter_mut().skip(at).take(4).enumerate() {
                *byte ^= (seed >> (i * 8)) as u8 | 0x5a;
            }
            let _ = read(&bytes, Path::new("test.hunk"));
        }
    }
}
