//! Compressed debug sections in ELF files.
//!
//! # Reading
//!
//! Two encodings exist:
//!
//! - `SHF_COMPRESSED` (the gABI format): the section starts with an
//!   `Elf32_Chdr`/`Elf64_Chdr` giving the algorithm (`ELFCOMPRESS_ZLIB`,
//!   `ELFCOMPRESS_ZSTD`), the uncompressed size and alignment.
//! - Legacy `.zdebug_*` sections (GNU): the contents are `"ZLIB"`, the
//!   uncompressed size as an 8-byte big-endian integer, then a zlib stream.
//!   The output section drops the `z` (`.zdebug_info` → `.debug_info`).
//!
//! [`CompressedSection::detect`] recognizes both, and
//! [`CompressedSection::decompress_into`] expands one section into a buffer
//! of exactly [`size`](CompressedSection::size) bytes, which can be a
//! section's slice of the output file. Sections are independent, so the
//! linker decompresses them one per rayon task, and only for sections that
//! survive garbage collection.
//!
//! # Writing
//!
//! [`compress_section`] produces the contents of an `SHF_COMPRESSED`
//! output section for `--compress-debug-sections`: the compression header
//! followed by the compressed data, compressed in parallel chunks.

use std::borrow::Cow;

use super::compress::Codec;
use super::compress::deflate::{Level, zlib_compress};
use crate::elf::read::consts::{ELFCLASS64, ELFCOMPRESS_ZLIB, ELFCOMPRESS_ZSTD};
use crate::elf::read::{
    CompressionHeader, ElfFormat, Endian, ObjectFile, RawRecord, SectionHeader, Source,
};
use crate::error::{Error, Result};
use crate::target::Endianness;

/// Magic bytes that start a `.zdebug_*` section.
pub const ZDEBUG_MAGIC: &[u8; 4] = b"ZLIB";

/// Size of the `.zdebug_*` header: magic plus 8-byte size.
pub const ZDEBUG_HEADER_SIZE: usize = 12;

/// Prefix of legacy compressed debug section names.
pub const ZDEBUG_PREFIX: &[u8] = b".zdebug";

/// A deflate stream can expand at most about 1032:1, and a Zstandard RLE
/// block 32768:1. Larger claimed sizes are rejected before allocating.
const MAX_ZLIB_RATIO: u64 = 1100;
const MAX_ZSTD_RATIO: u64 = 32_768;

/// A compressed section, ready to decompress.
#[derive(Clone, Copy, Debug)]
pub struct CompressedSection<'a> {
    /// The algorithm.
    pub codec: Codec,
    /// Uncompressed size in bytes.
    pub size: u64,
    /// Alignment of the uncompressed data (`ch_addralign`, or the section
    /// header's alignment for `.zdebug_*`).
    pub align: u64,
    /// The compressed stream.
    pub data: &'a [u8],
    /// File offset of `data`, for error messages.
    pub data_offset: u64,
    /// Whether this is a legacy `.zdebug_*` section.
    pub zdebug: bool,
}

impl<'a> CompressedSection<'a> {
    /// Builds a description from an ELF compression header and the stream
    /// that follows it, as returned by [`ObjectFile::compressed_data`].
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` for an unknown `ch_type`.
    pub fn from_chdr(
        chdr: &CompressionHeader,
        data: &'a [u8],
        data_offset: u64,
        source: Source<'_>,
    ) -> Result<Self> {
        let codec = match chdr.ch_type {
            ELFCOMPRESS_ZLIB => Codec::Zlib,
            ELFCOMPRESS_ZSTD => Codec::Zstd,
            other => {
                return Err(source.malformed(
                    data_offset,
                    format!("section compression type {other} (unsupported)"),
                ));
            }
        };
        Ok(Self {
            codec,
            size: chdr.ch_size,
            align: chdr.ch_addralign,
            data,
            data_offset,
            zdebug: false,
        })
    }

    /// Parses the contents of a `.zdebug_*` section, which start at file
    /// offset `offset`. Returns `None` if the contents do not start with the
    /// `ZLIB` header (such sections are left as they are, as GNU ld does).
    #[must_use]
    pub fn from_zdebug(contents: &'a [u8], offset: u64, align: u64) -> Option<Self> {
        let (header, data) = contents.split_at_checked(ZDEBUG_HEADER_SIZE)?;
        let (magic, size) = header.split_at(4);
        if magic != ZDEBUG_MAGIC {
            return None;
        }
        Some(Self {
            codec: Codec::Zlib,
            size: u64::from_be_bytes(size.try_into().ok()?),
            align,
            data,
            data_offset: offset.saturating_add(ZDEBUG_HEADER_SIZE as u64),
            zdebug: true,
        })
    }

    /// Recognizes a compressed section of `object`: `SHF_COMPRESSED`, or a
    /// `.zdebug*` name with a `ZLIB` header. Returns `Ok(None)` for other
    /// sections.
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` if the contents or name cannot be read,
    /// the compression header is truncated, or its type is unknown.
    pub fn detect<F: ElfFormat>(
        object: &ObjectFile<'a, F>,
        header: &SectionHeader,
    ) -> Result<Option<Self>> {
        if let Some((chdr, data)) = object.compressed_data(header)? {
            let offset = header
                .sh_offset
                .saturating_add(u64::try_from(F::Chdr::SIZE).unwrap_or(u64::MAX));
            return Self::from_chdr(&chdr, data, offset, object.source()).map(Some);
        }
        if header.is_nobits() || !object.section_name(header)?.starts_with(ZDEBUG_PREFIX) {
            return Ok(None);
        }
        let contents = object.section_data(header)?;
        Ok(Self::from_zdebug(
            contents,
            header.sh_offset,
            header.sh_addralign,
        ))
    }

    /// The uncompressed size as a `usize`, after checking that it is
    /// plausible for the amount of compressed data.
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` if the size exceeds the codec's maximum
    /// expansion of the compressed data or does not fit in memory.
    pub fn checked_size(&self, source: Source<'_>) -> Result<usize> {
        let ratio = match self.codec {
            Codec::Zlib => MAX_ZLIB_RATIO,
            Codec::Zstd => MAX_ZSTD_RATIO,
        };
        let limit = u64::try_from(self.data.len())
            .unwrap_or(u64::MAX)
            .saturating_mul(ratio)
            .saturating_add(1024);
        match usize::try_from(self.size) {
            Ok(size) if self.size <= limit && isize::try_from(size).is_ok() => Ok(size),
            _ => Err(source.malformed(
                self.data_offset,
                format!(
                    "compressed section size {:#x} (too large for {} bytes of data)",
                    self.size,
                    self.data.len()
                ),
            )),
        }
    }

    /// Decompresses into `out`, which must be exactly
    /// [`size`](Self::size) bytes long.
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` if `out` has the wrong size or the data
    /// is corrupt, truncated, fails its checksum or has a different size.
    pub fn decompress_into(&self, out: &mut [u8], source: Source<'_>) -> Result<()> {
        if u64::try_from(out.len()).ok() != Some(self.size) {
            return Err(Error::Internal(format!(
                "decompression buffer of {} bytes for a {}-byte section",
                out.len(),
                self.size
            )));
        }
        self.codec
            .decompress_into(self.data, out)
            .map_err(|e| e.into_error(source.path, source.member, self.data_offset))
    }

    /// Decompresses into a new buffer.
    ///
    /// # Errors
    ///
    /// As [`decompress_into`](Self::decompress_into), plus an error if the
    /// size is implausible (see [`checked_size`](Self::checked_size)) or
    /// the buffer cannot be allocated.
    pub fn decompress(&self, source: Source<'_>) -> Result<Vec<u8>> {
        let size = self.checked_size(source)?;
        let mut out = Vec::new();
        out.try_reserve_exact(size).map_err(|_| {
            source.malformed(
                self.data_offset,
                format!("compressed section size {size:#x} (cannot allocate)"),
            )
        })?;
        out.resize(size, 0);
        self.decompress_into(&mut out, source)?;
        Ok(out)
    }
}

/// The contents of a section, decompressed if it is compressed.
///
/// # Errors
///
/// See [`CompressedSection::detect`] and [`CompressedSection::decompress`].
pub fn section_contents<'a, F: ElfFormat>(
    object: &ObjectFile<'a, F>,
    header: &SectionHeader,
) -> Result<Cow<'a, [u8]>> {
    match CompressedSection::detect(object, header)? {
        Some(compressed) => compressed.decompress(object.source()).map(Cow::Owned),
        None => object.section_data(header).map(Cow::Borrowed),
    }
}

/// The name a section has once decompressed: `.zdebug_*` becomes
/// `.debug_*`; other names are unchanged.
#[must_use]
pub fn decompressed_name(name: &[u8]) -> Cow<'_, [u8]> {
    match name.strip_prefix(ZDEBUG_PREFIX) {
        Some(rest) => {
            let mut owned = Vec::with_capacity(name.len().saturating_sub(1));
            owned.extend_from_slice(b".debug");
            owned.extend_from_slice(rest);
            Cow::Owned(owned)
        }
        None => Cow::Borrowed(name),
    }
}

/// Output compression for debug sections (`--compress-debug-sections`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OutputCompression {
    /// zlib at the given level (lld uses level 1, or 6 with `-O2`).
    Zlib(Level),
    /// Zstandard.
    Zstd,
}

impl OutputCompression {
    /// Parses a `--compress-debug-sections` value: `zlib`, `zlib-gabi`,
    /// or `zstd`. `none` (and anything else) returns `None`.
    #[must_use]
    pub fn from_option(value: &str, level: Level) -> Option<Self> {
        match value {
            "zlib" | "zlib-gabi" => Some(Self::Zlib(level)),
            "zstd" => Some(Self::Zstd),
            _ => None,
        }
    }
}

/// Builds the contents of an `SHF_COMPRESSED` section holding `data`: an
/// `Elf_Chdr` for format `F`, then the compressed stream. `align` is the
/// uncompressed section's alignment.
///
/// The output is deterministic and independent of the thread count.
///
/// # Errors
///
/// Returns `Error::Unimplemented` for Zstandard, which qld cannot write yet.
pub fn compress_section<F: ElfFormat>(
    data: &[u8],
    compression: OutputCompression,
    align: u64,
) -> Result<Vec<u8>> {
    let (ch_type, stream) = match compression {
        OutputCompression::Zlib(level) => (ELFCOMPRESS_ZLIB, zlib_compress(data, level)),
        OutputCompression::Zstd => {
            return Err(Error::Unimplemented(
                "--compress-debug-sections=zstd output (roadmap M5)".into(),
            ));
        }
    };
    let size = u64::try_from(data.len()).unwrap_or(u64::MAX);
    let mut out = encode_chdr::<F>(ch_type, size, align);
    out.reserve_exact(stream.len());
    out.extend_from_slice(&stream);
    Ok(out)
}

/// Encodes an `Elf32_Chdr` or `Elf64_Chdr` in `F`'s byte order.
#[must_use]
pub fn encode_chdr<F: ElfFormat>(ch_type: u32, size: u64, align: u64) -> Vec<u8> {
    let big = <F::Endian as Endian>::ENDIANNESS == Endianness::Big;
    let u32_bytes = |v: u32| {
        if big {
            v.to_be_bytes()
        } else {
            v.to_le_bytes()
        }
    };
    let u64_bytes = |v: u64| {
        if big {
            v.to_be_bytes()
        } else {
            v.to_le_bytes()
        }
    };
    let mut out = Vec::with_capacity(24);
    out.extend_from_slice(&u32_bytes(ch_type));
    if F::CLASS == ELFCLASS64 {
        out.extend_from_slice(&[0; 4]); // ch_reserved
        out.extend_from_slice(&u64_bytes(size));
        out.extend_from_slice(&u64_bytes(align));
    } else {
        // Sizes beyond 4 GiB cannot occur in a 32-bit output.
        out.extend_from_slice(&u32_bytes(u32::try_from(size).unwrap_or(u32::MAX)));
        out.extend_from_slice(&u32_bytes(u32::try_from(align).unwrap_or(u32::MAX)));
    }
    out
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::elf::read::{Elf32Be, Elf64Le};

    fn source() -> Source<'static> {
        Source::new(Path::new("test.o"))
    }

    #[test]
    fn chdr_roundtrip() {
        let bytes = encode_chdr::<Elf64Le>(ELFCOMPRESS_ZLIB, 0x12_3456_789a, 8);
        assert_eq!(bytes.len(), 24);
        let (raw, _) = <[u8; 24]>::slice_from(&bytes);
        let chdr = Elf64Le::decode_chdr(&raw[0]);
        assert_eq!(chdr.ch_type, ELFCOMPRESS_ZLIB);
        assert_eq!(chdr.ch_size, 0x12_3456_789a);
        assert_eq!(chdr.ch_addralign, 8);

        let bytes = encode_chdr::<Elf32Be>(ELFCOMPRESS_ZSTD, 77, 4);
        assert_eq!(bytes.len(), 12);
        let (raw, _) = <[u8; 12]>::slice_from(&bytes);
        let chdr = Elf32Be::decode_chdr(&raw[0]);
        assert_eq!(
            (chdr.ch_type, chdr.ch_size, chdr.ch_addralign),
            (ELFCOMPRESS_ZSTD, 77, 4)
        );
    }

    #[test]
    fn compress_then_decompress() {
        let data: Vec<u8> = (0..300_000u32)
            .map(|i| (i % 251) as u8 ^ (i >> 9) as u8)
            .collect();
        let contents =
            compress_section::<Elf64Le>(&data, OutputCompression::Zlib(Level::FASTEST), 1).unwrap();
        let (raw, _) = <[u8; 24]>::slice_from(&contents);
        let chdr = Elf64Le::decode_chdr(&raw[0]);
        let section = CompressedSection::from_chdr(&chdr, &contents[24..], 24, source()).unwrap();
        assert_eq!(section.decompress(source()).unwrap(), data);
        assert!(compress_section::<Elf64Le>(&data, OutputCompression::Zstd, 1).is_err());
    }

    #[test]
    fn zdebug_header() {
        let data = b"some debug info".repeat(40);
        let z = zlib_compress(&data, Level::DEFAULT);
        let mut contents = ZDEBUG_MAGIC.to_vec();
        contents.extend_from_slice(&(data.len() as u64).to_be_bytes());
        contents.extend_from_slice(&z);
        let section = CompressedSection::from_zdebug(&contents, 0x100, 1).unwrap();
        assert!(section.zdebug);
        assert_eq!(section.data_offset, 0x10c);
        assert_eq!(section.decompress(source()).unwrap(), data);
        assert!(CompressedSection::from_zdebug(b"ZLIX\0\0\0\0\0\0\0\x01x", 0, 1).is_none());
        assert!(CompressedSection::from_zdebug(b"ZLIB", 0, 1).is_none());
        assert_eq!(&*decompressed_name(b".zdebug_info"), b".debug_info");
        assert_eq!(&*decompressed_name(b".debug_info"), b".debug_info");
    }

    #[test]
    fn rejects_implausible_sizes_and_bad_types() {
        let chdr = CompressionHeader {
            ch_type: ELFCOMPRESS_ZLIB,
            ch_size: u64::MAX,
            ch_addralign: 1,
        };
        let section = CompressedSection::from_chdr(&chdr, &[0x78, 0x9c], 0, source()).unwrap();
        assert!(section.decompress(source()).is_err());
        let chdr = CompressionHeader {
            ch_type: 99,
            ..chdr
        };
        assert!(CompressedSection::from_chdr(&chdr, &[], 0, source()).is_err());
        // Wrong-size buffer.
        let chdr = CompressionHeader {
            ch_type: ELFCOMPRESS_ZLIB,
            ch_size: 10,
            ch_addralign: 1,
        };
        let section = CompressedSection::from_chdr(&chdr, &[], 0, source()).unwrap();
        assert!(section.decompress_into(&mut [0; 9], source()).is_err());
    }
}
