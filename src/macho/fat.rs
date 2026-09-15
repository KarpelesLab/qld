//! Writing universal (fat) binaries.
//!
//! The big-endian `fat_header` is followed by one `fat_arch` per slice
//! (`fat_arch_64` when an offset or size needs more than 32 bits). Slices
//! start at page boundaries: 2^14 for arm64, 2^12 for x86_64, like `lipo`.

#![deny(clippy::arithmetic_side_effects)]

use crate::error::{Error, Result};
use crate::macho::read::Arch;
use crate::macho::read::consts::{CPU_TYPE_ARM64, FAT_MAGIC, FAT_MAGIC_64};

use super::buf::{align_up, to_u64, to_usize};

/// One slice to assemble.
#[derive(Clone, Debug)]
pub struct Slice {
    /// Its architecture.
    pub arch: Arch,
    /// The raw `cpusubtype` of the slice's header.
    pub cpu_subtype: u32,
    /// The Mach-O image.
    pub data: Vec<u8>,
}

/// The alignment (log2) of a slice of `arch`.
#[must_use]
pub fn slice_align(arch: Arch) -> u32 {
    if arch.cpu_type == CPU_TYPE_ARM64 {
        14
    } else {
        12
    }
}

/// Assembles a universal binary. Slices are ordered by CPU type, as `lipo`
/// orders them.
///
/// # Errors
///
/// [`Error::Limit`] when the file would exceed the 64-bit format.
pub fn assemble(mut slices: Vec<Slice>) -> Result<Vec<u8>> {
    slices.sort_by_key(|s| (s.arch.cpu_type, s.arch.cpu_subtype));
    let count = u32::try_from(slices.len()).map_err(|_| Error::Limit("too many slices".into()))?;
    // Decide the header form from a 32-bit layout attempt.
    let layout = |entry: u64| -> Vec<u64> {
        let mut offset = 8u64.saturating_add(entry.saturating_mul(to_u64(slices.len())));
        slices
            .iter()
            .map(|slice| {
                let align = 1u64.checked_shl(slice_align(slice.arch)).unwrap_or(1);
                let start = align_up(offset, align);
                offset = start.saturating_add(to_u64(slice.data.len()));
                start
            })
            .collect()
    };
    let offsets32 = layout(20);
    let needs64 = slices
        .iter()
        .zip(&offsets32)
        .any(|(s, &o)| u32::try_from(o).is_err() || u32::try_from(s.data.len()).is_err());
    let offsets = if needs64 { layout(32) } else { offsets32 };
    let end = slices
        .iter()
        .zip(&offsets)
        .map(|(s, &o)| o.saturating_add(to_u64(s.data.len())))
        .max()
        .unwrap_or(8);
    let mut out = vec![0u8; to_usize(end)];
    let mut header = Vec::new();
    header.extend_from_slice(&(if needs64 { FAT_MAGIC_64 } else { FAT_MAGIC }).to_be_bytes());
    header.extend_from_slice(&count.to_be_bytes());
    for (slice, &offset) in slices.iter().zip(&offsets) {
        header.extend_from_slice(&slice.arch.cpu_type.to_be_bytes());
        header.extend_from_slice(&slice.cpu_subtype.to_be_bytes());
        if needs64 {
            header.extend_from_slice(&offset.to_be_bytes());
            header.extend_from_slice(&to_u64(slice.data.len()).to_be_bytes());
            header.extend_from_slice(&slice_align(slice.arch).to_be_bytes());
            header.extend_from_slice(&0u32.to_be_bytes());
        } else {
            header.extend_from_slice(&u32::try_from(offset).unwrap_or(0).to_be_bytes());
            header.extend_from_slice(&u32::try_from(slice.data.len()).unwrap_or(0).to_be_bytes());
            header.extend_from_slice(&slice_align(slice.arch).to_be_bytes());
        }
    }
    out.get_mut(..header.len())
        .ok_or_else(|| Error::Internal("fat header does not fit".into()))?
        .copy_from_slice(&header);
    for (slice, &offset) in slices.iter().zip(&offsets) {
        let start = to_usize(offset);
        out.get_mut(start..start.saturating_add(slice.data.len()))
            .ok_or_else(|| Error::Internal("fat slice does not fit".into()))?
            .copy_from_slice(&slice.data);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::macho::read::{FatFile, Source};
    use std::path::Path;

    #[test]
    fn slices_round_trip_through_the_reader() {
        let slices = vec![
            Slice {
                arch: Arch::X86_64,
                cpu_subtype: 3,
                data: vec![1; 5000],
            },
            Slice {
                arch: Arch::ARM64,
                cpu_subtype: 0,
                data: vec![2; 100],
            },
        ];
        let fat = assemble(slices).unwrap();
        let parsed = FatFile::parse(&fat, Source::new(Path::new("f"))).unwrap();
        assert_eq!(parsed.slices().len(), 2);
        let x86 = parsed.select(Arch::X86_64).unwrap();
        assert_eq!(x86.offset, 0x1000);
        assert_eq!(x86.data, &[1; 5000][..]);
        let arm = parsed.select(Arch::ARM64).unwrap();
        assert_eq!(arm.offset % 0x4000, 0);
        assert_eq!(arm.data, &[2; 100][..]);
    }
}
