//! `.eh_frame` record splitting.
//!
//! An `.eh_frame` section is a sequence of CIE and FDE records. This module
//! finds the record boundaries, the CIE each FDE points to, and which
//! relocations belong to which record — in particular the relocation of an
//! FDE's `pc_begin` field, which ties the FDE to the function it describes.
//! Call frame instructions are not interpreted.

use core::marker::PhantomData;
use core::ops::Range;

use super::format::{ElfFormat, Endian, read_u32, read_u64};
use super::reloc::Relocations;
use super::source::{Source, to_u64};
use crate::error::Result;

/// What an `.eh_frame` record is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EhFrameRecordKind {
    /// A common information entry.
    Cie,
    /// A frame description entry.
    Fde {
        /// Section offset of the CIE this FDE uses.
        cie_offset: usize,
    },
}

/// One CIE or FDE.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EhFrameRecord<'a> {
    /// Offset of the record within the section.
    pub offset: usize,
    /// The whole record, including its length field.
    pub data: &'a [u8],
    /// Size of the length field: 4, or 12 for the 64-bit DWARF form.
    pub length_size: usize,
    /// CIE or FDE.
    pub kind: EhFrameRecordKind,
}

impl<'a> EhFrameRecord<'a> {
    /// Whether this record is a CIE.
    #[must_use]
    pub fn is_cie(&self) -> bool {
        self.kind == EhFrameRecordKind::Cie
    }

    /// Section offset of the FDE `pc_begin` field (meaningful for FDEs): the
    /// location the function relocation applies to.
    #[must_use]
    pub fn pc_begin_offset(&self) -> usize {
        self.offset
            .saturating_add(self.length_size)
            .saturating_add(4)
    }

    /// The CIE augmentation string (for CIEs), without its terminator.
    ///
    /// Returns `None` for FDEs and for CIEs too short to hold one.
    #[must_use]
    pub fn augmentation(&self) -> Option<&'a [u8]> {
        if !self.is_cie() {
            return None;
        }
        // length, CIE id (4), version (1), then the string.
        let start = self.length_size.checked_add(5)?;
        let tail = self.data.get(start..)?;
        let len = super::strtab::find_nul(tail)?;
        tail.get(..len)
    }
}

/// Iterator over the records of an `.eh_frame` section.
///
/// Iteration ends at the end of the data or at a zero terminator. The first
/// malformed record is reported as an error and ends iteration.
#[derive(Debug)]
pub struct EhFrameIter<'a, E: Endian> {
    data: &'a [u8],
    pos: usize,
    file_offset: u64,
    source: Source<'a>,
    _endian: PhantomData<E>,
}

impl<E: Endian> Clone for EhFrameIter<'_, E> {
    fn clone(&self) -> Self {
        Self { ..*self }
    }
}

impl<'a, E: Endian> EhFrameIter<'a, E> {
    /// Iterates over the records in `data`, found at `file_offset`.
    #[must_use]
    pub fn new(data: &'a [u8], file_offset: u64, source: Source<'a>) -> Self {
        Self {
            data,
            pos: 0,
            file_offset,
            source,
            _endian: PhantomData,
        }
    }

    /// Parses the record at `pos`. `Ok(None)` is a terminator.
    fn parse_one(
        &self,
        pos: usize,
    ) -> core::result::Result<Option<EhFrameRecord<'a>>, &'static str> {
        let length = read_u32::<E>(self.data, pos).ok_or(".eh_frame record length (truncated)")?;
        if length == 0 {
            return Ok(None);
        }
        let (length_size, length) = if length == u32::MAX {
            let at = pos.checked_add(4).ok_or(".eh_frame record length")?;
            let ext = read_u64::<E>(self.data, at).ok_or(".eh_frame record length (truncated)")?;
            (12, ext)
        } else {
            (4, u64::from(length))
        };
        let length = usize::try_from(length).map_err(|_| ".eh_frame record length")?;
        if length < 4 {
            return Err(".eh_frame record length (too short)");
        }
        let end = pos
            .checked_add(length_size)
            .and_then(|p| p.checked_add(length))
            .ok_or(".eh_frame record length")?;
        let data = self
            .data
            .get(pos..end)
            .ok_or(".eh_frame record (extends past end of section)")?;
        let id_pos = pos.checked_add(length_size).ok_or(".eh_frame record")?;
        let id = read_u32::<E>(self.data, id_pos).ok_or(".eh_frame record")?;
        let kind = if id == 0 {
            EhFrameRecordKind::Cie
        } else {
            let id = usize::try_from(id).map_err(|_| ".eh_frame CIE pointer")?;
            let cie_offset = id_pos
                .checked_sub(id)
                .ok_or(".eh_frame CIE pointer (out of range)")?;
            EhFrameRecordKind::Fde { cie_offset }
        };
        Ok(Some(EhFrameRecord {
            offset: pos,
            data,
            length_size,
            kind,
        }))
    }
}

impl<'a, E: Endian> Iterator for EhFrameIter<'a, E> {
    type Item = Result<EhFrameRecord<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.pos >= self.data.len() {
            return None;
        }
        let pos = self.pos;
        match self.parse_one(pos) {
            Ok(Some(record)) => {
                self.pos = pos.saturating_add(record.data.len());
                Some(Ok(record))
            }
            Ok(None) => {
                self.pos = self.data.len();
                None
            }
            Err(what) => {
                self.pos = self.data.len();
                Some(Err(self.source.malformed(
                    self.file_offset.saturating_add(to_u64(pos)),
                    what,
                )))
            }
        }
    }
}

/// An `.eh_frame` record with the relocations that apply to it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EhFrameEntry<'a> {
    /// The record.
    pub record: EhFrameRecord<'a>,
    /// Indices of the relocations whose offsets fall inside the record.
    pub relocations: Range<usize>,
    /// For FDEs, the index of the relocation applied to `pc_begin`: the one
    /// that names the function (or its section) the FDE describes.
    pub pc_begin_relocation: Option<usize>,
}

/// Splits an `.eh_frame` section into records and assigns relocations to
/// them.
///
/// `relocations` must be the relocation section targeting this `.eh_frame`,
/// sorted by offset (as assemblers emit them).
///
/// # Errors
///
/// Returns `Error::Malformed` for malformed records, unsorted relocations,
/// or relocations outside every record.
pub fn split_eh_frame<'a, F: ElfFormat>(
    data: &'a [u8],
    relocations: Relocations<'a, F>,
    file_offset: u64,
    source: Source<'a>,
) -> Result<Vec<EhFrameEntry<'a>>> {
    match relocations {
        Relocations::Rel(r) => {
            split_with::<F::Endian>(data, r.iter().map(|r| r.offset), file_offset, source)
        }
        Relocations::Rela(r) => {
            split_with::<F::Endian>(data, r.iter().map(|r| r.offset), file_offset, source)
        }
    }
}

/// [`split_eh_frame`] over any sequence of relocation offsets.
///
/// # Errors
///
/// See [`split_eh_frame`].
pub fn split_with<'a, E: Endian>(
    data: &'a [u8],
    offsets: impl Iterator<Item = u64>,
    file_offset: u64,
    source: Source<'a>,
) -> Result<Vec<EhFrameEntry<'a>>> {
    let mut offsets = offsets.enumerate().peekable();
    let mut entries = Vec::new();
    let mut last = 0u64;
    let reloc_error = |offset: u64, what: &str| {
        source.malformed(file_offset.saturating_add(offset), what.to_owned())
    };
    let mut next_reloc = 0usize;
    for record in EhFrameIter::<E>::new(data, file_offset, source) {
        let record = record?;
        let start = to_u64(record.offset);
        let end = start.saturating_add(to_u64(record.data.len()));
        let first = next_reloc;
        let mut pc_begin_relocation = None;
        let pc_begin = to_u64(record.pc_begin_offset());
        while let Some(&(index, offset)) = offsets.peek() {
            if offset >= end {
                break;
            }
            if offset < last {
                return Err(reloc_error(offset, ".eh_frame relocations (not sorted)"));
            }
            if offset < start {
                return Err(reloc_error(
                    offset,
                    ".eh_frame relocation (outside any record)",
                ));
            }
            if !record.is_cie() && offset == pc_begin && pc_begin_relocation.is_none() {
                pc_begin_relocation = Some(index);
            }
            last = offset;
            next_reloc = index.saturating_add(1);
            offsets.next();
        }
        entries.push(EhFrameEntry {
            record,
            relocations: first..next_reloc,
            pc_begin_relocation,
        });
    }
    if let Some(&(_, offset)) = offsets.peek() {
        return Err(reloc_error(
            offset,
            ".eh_frame relocation (outside any record)",
        ));
    }
    Ok(entries)
}

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)]
mod tests {
    use super::*;
    use crate::elf::read::format::Little;
    use std::path::Path;

    fn record(id: u32, body: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&((body.len() + 4) as u32).to_le_bytes());
        v.extend_from_slice(&id.to_le_bytes());
        v.extend_from_slice(body);
        v
    }

    #[test]
    fn splits_records() {
        let src = Source::new(Path::new("t.o"));
        let mut data = record(0, b"\x01zR\0\x01\x78\x10\x01\x1b\0\0\0");
        let fde_at = data.len();
        data.extend(record((fde_at + 4) as u32, &[0; 12]));
        let fde2_at = data.len();
        data.extend(record((fde2_at + 4) as u32, &[0; 12]));
        data.extend_from_slice(&[0; 4]);

        let records: Vec<_> = EhFrameIter::<Little>::new(&data, 0, src)
            .collect::<Result<_>>()
            .unwrap();
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].augmentation(), Some(&b"zR"[..]));
        assert_eq!(records[1].kind, EhFrameRecordKind::Fde { cie_offset: 0 });
        assert_eq!(records[2].offset, fde2_at);

        let offsets = [(fde_at + 8) as u64, (fde2_at + 8) as u64];
        let entries = split_with::<Little>(&data, offsets.into_iter(), 0, src).unwrap();
        assert_eq!(entries[0].relocations, 0..0);
        assert_eq!(entries[1].relocations, 0..1);
        assert_eq!(entries[1].pc_begin_relocation, Some(0));
        assert_eq!(entries[2].pc_begin_relocation, Some(1));

        let unsorted = [(fde2_at + 8) as u64, (fde_at + 8) as u64];
        assert!(split_with::<Little>(&data, unsorted.into_iter(), 0, src).is_err());
        let outside = [(data.len() + 8) as u64];
        assert!(split_with::<Little>(&data, outside.into_iter(), 0, src).is_err());

        // A CIE pointer before the start of the section.
        let bad = record(1000, &[0; 8]);
        assert!(
            EhFrameIter::<Little>::new(&bad, 0, src)
                .next()
                .unwrap()
                .is_err()
        );
    }
}
