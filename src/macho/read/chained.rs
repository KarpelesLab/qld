//! `LC_DYLD_CHAINED_FIXUPS`: the header and the imports table.
//!
//! Only what a static linker needs from a dylib is decoded: the list of
//! symbols the image imports and from which library. The fixup chains
//! themselves (`dyld_chained_starts_in_image`) are exposed as raw offsets.

use super::bytes::{Endian, Source, cstr, subslice, to_u64};
use super::consts::{
    DYLD_CHAINED_IMPORT, DYLD_CHAINED_IMPORT_ADDEND, DYLD_CHAINED_IMPORT_ADDEND64,
    DYLD_CHAINED_SYMBOL_UNCOMPRESSED,
};
use crate::error::Result;

/// `dyld_chained_fixups_header`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChainedFixupsHeader {
    /// `fixups_version` (0).
    pub fixups_version: u32,
    /// Offset of `dyld_chained_starts_in_image` within the blob.
    pub starts_offset: u32,
    /// Offset of the imports table within the blob.
    pub imports_offset: u32,
    /// Offset of the symbol names within the blob.
    pub symbols_offset: u32,
    /// Number of imports.
    pub imports_count: u32,
    /// `DYLD_CHAINED_IMPORT*`.
    pub imports_format: u32,
    /// `DYLD_CHAINED_SYMBOL_*`.
    pub symbols_format: u32,
}

/// One imported symbol.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChainedImport<'a> {
    /// Library ordinal: 1-based dependency index, or a negative special
    /// value (-1 main executable, -2 flat lookup, -3 weak lookup), or 0 for
    /// this image.
    pub lib_ordinal: i32,
    /// Weak import: may be missing at run time.
    pub weak_import: bool,
    /// The symbol name.
    pub name: &'a [u8],
    /// Addend (zero for `DYLD_CHAINED_IMPORT`).
    pub addend: i64,
}

/// The decoded chained fixups blob.
#[derive(Clone, Copy, Debug)]
pub struct ChainedFixups<'a> {
    /// The header.
    pub header: ChainedFixupsHeader,
    data: &'a [u8],
    file_offset: u64,
    endian: Endian,
    source: Source<'a>,
}

impl<'a> ChainedFixups<'a> {
    /// Parses the header of the blob `data`, found at `file_offset`.
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` if the header is truncated, the imports
    /// table does not fit, or the format is unknown. Compressed symbol names
    /// are reported as unsupported (no known linker writes them).
    pub fn parse(
        data: &'a [u8],
        file_offset: u64,
        endian: Endian,
        source: Source<'a>,
    ) -> Result<Self> {
        let fail = |what: &str| source.malformed(file_offset, format!("chained fixups ({what})"));
        let field = |at: usize| endian.u32(data, at).ok_or_else(|| fail("truncated header"));
        let header = ChainedFixupsHeader {
            fixups_version: field(0)?,
            starts_offset: field(4)?,
            imports_offset: field(8)?,
            symbols_offset: field(12)?,
            imports_count: field(16)?,
            imports_format: field(20)?,
            symbols_format: field(24)?,
        };
        let size = match header.imports_format {
            DYLD_CHAINED_IMPORT => 4u64,
            DYLD_CHAINED_IMPORT_ADDEND => 8,
            DYLD_CHAINED_IMPORT_ADDEND64 => 16,
            _ => return Err(fail("unknown imports format")),
        };
        if header.symbols_format != DYLD_CHAINED_SYMBOL_UNCOMPRESSED {
            return Err(fail("compressed symbol names are not supported"));
        }
        if subslice(
            data,
            u64::from(header.imports_offset),
            u64::from(header.imports_count).saturating_mul(size),
        )
        .is_none()
        {
            return Err(fail("imports table extends past the blob"));
        }
        if usize::try_from(header.symbols_offset)
            .ok()
            .is_none_or(|at| at > data.len())
        {
            return Err(fail("symbols offset out of range"));
        }
        Ok(Self {
            header,
            data,
            file_offset,
            endian,
            source,
        })
    }

    /// Number of imports.
    #[must_use]
    pub fn len(&self) -> usize {
        self.header.imports_count as usize
    }

    /// Whether there are no imports.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.header.imports_count == 0
    }

    /// Decodes import `index`.
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` if `index` is out of range or the name
    /// offset is invalid.
    pub fn import(&self, index: usize) -> Result<ChainedImport<'a>> {
        let fail = |what: &str| {
            self.source.malformed(
                self.file_offset
                    .saturating_add(u64::from(self.header.imports_offset)),
                format!("chained import {index} ({what})"),
            )
        };
        if index >= self.len() {
            return Err(fail("out of range"));
        }
        let e = self.endian;
        let base = usize::try_from(self.header.imports_offset).unwrap_or(usize::MAX);
        let (lib_ordinal, weak_import, name_offset, addend) = match self.header.imports_format {
            DYLD_CHAINED_IMPORT | DYLD_CHAINED_IMPORT_ADDEND => {
                let size = if self.header.imports_format == DYLD_CHAINED_IMPORT {
                    4
                } else {
                    8
                };
                let at = index
                    .checked_mul(size)
                    .and_then(|o| o.checked_add(base))
                    .ok_or_else(|| fail("offset"))?;
                let raw = e.u32(self.data, at).ok_or_else(|| fail("truncated"))?;
                let addend = if size == 8 {
                    at.checked_add(4)
                        .and_then(|a| e.u32(self.data, a))
                        .map_or(0, |v| i64::from(v as i32))
                } else {
                    0
                };
                // 8-bit ordinals above 0xF0 are negative special values.
                let ordinal = (raw & 0xff) as u8;
                let lib_ordinal = if ordinal > 0xf0 {
                    i32::from(ordinal as i8)
                } else {
                    i32::from(ordinal)
                };
                (lib_ordinal, (raw >> 8) & 1 != 0, raw >> 9, addend)
            }
            _ => {
                let at = index
                    .checked_mul(16)
                    .and_then(|o| o.checked_add(base))
                    .ok_or_else(|| fail("offset"))?;
                let raw = e.u64(self.data, at).ok_or_else(|| fail("truncated"))?;
                let addend = at
                    .checked_add(8)
                    .and_then(|a| e.u64(self.data, a))
                    .map_or(0, |v| v as i64);
                let ordinal = (raw & 0xffff) as u16;
                let lib_ordinal = if ordinal > 0xfff0 {
                    i32::from(ordinal as i16)
                } else {
                    i32::from(ordinal)
                };
                (
                    lib_ordinal,
                    (raw >> 16) & 1 != 0,
                    (raw >> 32) as u32,
                    addend,
                )
            }
        };
        let name = usize::try_from(self.header.symbols_offset)
            .ok()
            .and_then(|s| s.checked_add(usize::try_from(name_offset).ok()?))
            .and_then(|at| self.data.get(at..))
            .and_then(cstr)
            .ok_or_else(|| fail("name offset"))?;
        Ok(ChainedImport {
            lib_ordinal,
            weak_import,
            name,
            addend,
        })
    }

    /// Iterates over the imports.
    pub fn imports(&self) -> impl Iterator<Item = Result<ChainedImport<'a>>> + '_ {
        (0..self.len()).map(|index| self.import(index))
    }

    /// File offset of the blob.
    #[must_use]
    pub fn file_offset(&self) -> u64 {
        self.file_offset
    }

    /// Size of the blob.
    #[must_use]
    pub fn size(&self) -> u64 {
        to_u64(self.data.len())
    }
}

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn imports() {
        let src = Source::new(Path::new("lib.dylib"));
        let mut blob = Vec::new();
        for v in [0u32, 28, 28, 36, 2, DYLD_CHAINED_IMPORT, 0] {
            blob.extend_from_slice(&v.to_le_bytes());
        }
        // _a from lib 1, weak _b from flat lookup (0xfe).
        blob.extend_from_slice(&1u32.to_le_bytes());
        blob.extend_from_slice(&(0xfeu32 | (1 << 8) | (4 << 9)).to_le_bytes());
        blob.extend_from_slice(b"_a\0\0_b\0");
        let fixups = ChainedFixups::parse(&blob, 0, Endian::LITTLE, src).unwrap();
        let imports: Vec<_> = fixups.imports().collect::<Result<_>>().unwrap();
        assert_eq!(imports.len(), 2);
        assert_eq!((imports[0].lib_ordinal, imports[0].name), (1, &b"_a"[..]));
        assert_eq!(imports[1].lib_ordinal, -2);
        assert!(imports[1].weak_import);
        assert_eq!(imports[1].name, b"_b");
        assert!(fixups.import(2).is_err());
        assert!(ChainedFixups::parse(&blob[..20], 0, Endian::LITTLE, src).is_err());
    }
}
