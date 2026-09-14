//! Universal (fat) binaries: a big-endian `fat_header` followed by
//! `fat_arch` or `fat_arch_64` entries, each locating one slice.
//!
//! A slice is a complete Mach-O object, archive or dylib (or a `.tbd`-less
//! stub); [`FatFile::select`] picks the one for the architecture being
//! linked.

use super::arch::Arch;
use super::bytes::{Endian, Source, subslice, to_u64};
use super::consts::{FAT_MAGIC, FAT_MAGIC_64};
use crate::error::Result;

/// Size of `fat_header`.
const HEADER_SIZE: usize = 8;
/// Size of `fat_arch`.
const ARCH_SIZE: usize = 20;
/// Size of `fat_arch_64`.
const ARCH64_SIZE: usize = 32;

/// Java class files share `0xCAFEBABE` with [`FAT_MAGIC`]. Their next field
/// is a version pair whose major number is at least 45, while no universal
/// binary has that many slices. (`src/input/identify.rs` uses the same rule.)
const JAVA_MIN_MAJOR: u32 = 45;

/// One slice of a universal binary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FatSlice<'a> {
    /// Architecture of the slice (subtype capability bits masked off).
    pub arch: Arch,
    /// The raw `cpusubtype`, with capability bits.
    pub raw_cpu_subtype: u32,
    /// Offset of the slice in the file.
    pub offset: u64,
    /// Size of the slice.
    pub size: u64,
    /// Alignment of the slice, as a power of two.
    pub align: u32,
    /// The slice contents.
    pub data: &'a [u8],
}

/// A parsed universal binary.
#[derive(Clone, Debug)]
pub struct FatFile<'a> {
    slices: Vec<FatSlice<'a>>,
    is64: bool,
    source: Source<'a>,
}

impl<'a> FatFile<'a> {
    /// Whether `data` starts with a universal binary header (and is not a
    /// Java class file).
    #[must_use]
    pub fn is_fat(data: &[u8]) -> bool {
        let magic = Endian::BIG.u32(data, 0);
        matches!(magic, Some(FAT_MAGIC | FAT_MAGIC_64))
            && Endian::BIG
                .u32(data, 4)
                .is_some_and(|count| count < JAVA_MIN_MAJOR)
    }

    /// Parses the header and the slice table.
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` if the magic is wrong, the file looks like a
    /// Java class file, the table is truncated, or a slice lies outside the
    /// file.
    pub fn parse(data: &'a [u8], source: Source<'a>) -> Result<Self> {
        let magic = Endian::BIG
            .u32(data, 0)
            .ok_or_else(|| source.malformed(0, "universal header (truncated)"))?;
        let is64 = match magic {
            FAT_MAGIC => false,
            FAT_MAGIC_64 => true,
            _ => return Err(source.malformed(0, "universal header magic")),
        };
        let count = Endian::BIG
            .u32(data, 4)
            .ok_or_else(|| source.malformed(4, "universal header (truncated)"))?;
        if count >= JAVA_MIN_MAJOR {
            return Err(source.malformed(
                4,
                format!("universal header (nfat_arch {count}: a Java class file?)"),
            ));
        }
        let entry_size = if is64 { ARCH64_SIZE } else { ARCH_SIZE };
        let mut slices = Vec::with_capacity(count as usize);
        for i in 0..count as usize {
            let at = i
                .checked_mul(entry_size)
                .and_then(|n| n.checked_add(HEADER_SIZE))
                .ok_or_else(|| source.malformed(0, "universal header"))?;
            let entry = data
                .get(at..)
                .and_then(|d| d.get(..entry_size))
                .ok_or_else(|| source.malformed(to_u64(at), format!("fat_arch {i} (truncated)")))?;
            let be32 = |off| Endian::BIG.u32(entry, off).unwrap_or(0);
            let be64 = |off| Endian::BIG.u64(entry, off).unwrap_or(0);
            let cpu_type = be32(0);
            let raw_cpu_subtype = be32(4);
            let (offset, size, align) = if is64 {
                (be64(8), be64(16), be32(24))
            } else {
                (u64::from(be32(8)), u64::from(be32(12)), be32(16))
            };
            let slice_data = subslice(data, offset, size).ok_or_else(|| {
                source.malformed(
                    to_u64(at),
                    format!(
                        "fat_arch {i} (slice at {offset:#x} size {size:#x} is outside the file)"
                    ),
                )
            })?;
            slices.push(FatSlice {
                arch: Arch::new(cpu_type, raw_cpu_subtype),
                raw_cpu_subtype,
                offset,
                size,
                align,
                data: slice_data,
            });
        }
        Ok(Self {
            slices,
            is64,
            source,
        })
    }

    /// Whether the header is `FAT_MAGIC_64`.
    #[must_use]
    pub fn is64(&self) -> bool {
        self.is64
    }

    /// The slices, in file order.
    #[must_use]
    pub fn slices(&self) -> &[FatSlice<'a>] {
        &self.slices
    }

    /// The slice whose `cputype` and masked `cpusubtype` equal `arch`.
    #[must_use]
    pub fn find(&self, arch: Arch) -> Option<&FatSlice<'a>> {
        self.slices.iter().find(|slice| slice.arch == arch)
    }

    /// Selects the slice for `arch`.
    ///
    /// An exact match wins. Failing that, `x86_64` accepts a lone slice of
    /// another x86_64 subtype (`x86_64h`). arm64 and arm64e are different
    /// ABIs and never substitute for each other.
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` naming the requested architecture and
    /// listing the available ones when nothing matches.
    pub fn select(&self, arch: Arch) -> Result<&FatSlice<'a>> {
        if let Some(slice) = self.find(arch) {
            return Ok(slice);
        }
        if arch == Arch::X86_64 {
            let mut same = self
                .slices
                .iter()
                .filter(|slice| slice.arch.cpu_type == arch.cpu_type);
            if let (Some(slice), None) = (same.next(), same.next()) {
                return Ok(slice);
            }
        }
        let available = self
            .slices
            .iter()
            .map(|slice| slice.arch.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        Err(self.source.malformed(
            0,
            format!(
                "universal file (no slice for architecture {arch}; available: {})",
                if available.is_empty() {
                    "none"
                } else {
                    &available
                }
            ),
        ))
    }

    /// A [`Source`] for errors in `slice`, whose offsets are relative to the
    /// slice.
    #[must_use]
    pub fn slice_source(&self, slice: &FatSlice<'_>) -> Source<'a> {
        self.source.at(slice.offset)
    }
}

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)]
mod tests {
    use super::*;
    use crate::macho::read::consts::{CPU_TYPE_ARM64, CPU_TYPE_X86_64};
    use std::path::Path;

    fn fat(entries: &[(u32, u32, u32, u32)], total: usize) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&FAT_MAGIC.to_be_bytes());
        v.extend_from_slice(&(entries.len() as u32).to_be_bytes());
        for &(t, s, off, size) in entries {
            for x in [t, s, off, size, 12] {
                v.extend_from_slice(&x.to_be_bytes());
            }
        }
        v.resize(total, 0xaa);
        v
    }

    #[test]
    fn select_slices() {
        let src = Source::new(Path::new("fat"));
        let data = fat(
            &[
                (CPU_TYPE_X86_64, 8, 0x100, 0x10),
                (CPU_TYPE_ARM64, 0x8000_0002, 0x200, 0x20),
            ],
            0x300,
        );
        assert!(FatFile::is_fat(&data));
        let file = FatFile::parse(&data, src).unwrap();
        assert_eq!(file.slices().len(), 2);
        assert_eq!(file.select(Arch::ARM64E).unwrap().offset, 0x200);
        assert_eq!(file.select(Arch::X86_64).unwrap().arch, Arch::X86_64H);
        let error = file.select(Arch::ARM64).unwrap_err().to_string();
        assert!(
            error.contains("no slice for architecture arm64; available: x86_64h, arm64e"),
            "{error}"
        );

        // Slice outside the file.
        let bad = fat(&[(CPU_TYPE_ARM64, 0, 0x100, 0x1000)], 0x200);
        assert!(FatFile::parse(&bad, src).is_err());
        // Java class file: minor 0, major 52.
        let mut class = FAT_MAGIC.to_be_bytes().to_vec();
        class.extend_from_slice(&[0, 0, 0, 52]);
        assert!(!FatFile::is_fat(&class));
        assert!(FatFile::parse(&class, src).is_err());
    }
}
