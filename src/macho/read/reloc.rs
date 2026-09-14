//! Relocation entries (`relocation_info` and `scattered_relocation_info`),
//! and the pairing of multi-entry relocations.
//!
//! Mach-O expresses some relocations with two entries:
//!
//! - arm64 `ARM64_RELOC_ADDEND` carries an addend for the following
//!   `BRANCH26`, `PAGE21` or `PAGEOFF12` entry.
//! - arm64 and x86_64 `*_RELOC_SUBTRACTOR` is followed by an `UNSIGNED` entry:
//!   the value is `target - subtrahend + addend`.
//! - i386 and 32-bit Arm `SECTDIFF`, `LOCAL_SECTDIFF`, `HALF` and
//!   `HALF_SECTDIFF` are followed by a `PAIR` entry.
//!
//! [`PairedRelocationIter`] folds these into one [`PairedRelocation`].

use super::bytes::{Endian, Source, to_u64};
use super::consts::{
    ARM_RELOC_HALF, ARM_RELOC_HALF_SECTDIFF, ARM_RELOC_LOCAL_SECTDIFF, ARM_RELOC_PAIR,
    ARM_RELOC_SECTDIFF, ARM64_RELOC_ADDEND, ARM64_RELOC_BRANCH26, ARM64_RELOC_PAGE21,
    ARM64_RELOC_PAGEOFF12, ARM64_RELOC_SUBTRACTOR, ARM64_RELOC_UNSIGNED, CPU_ARCH_ABI64,
    CPU_ARCH_ABI64_32, CPU_TYPE_ARM, CPU_TYPE_ARM64, CPU_TYPE_ARM64_32, CPU_TYPE_X86,
    CPU_TYPE_X86_64, GENERIC_RELOC_LOCAL_SECTDIFF, GENERIC_RELOC_PAIR, GENERIC_RELOC_SECTDIFF,
    R_SCATTERED, X86_64_RELOC_SUBTRACTOR, X86_64_RELOC_UNSIGNED, reloc_name,
};
use crate::error::Result;

/// Size of a relocation entry.
pub const RELOCATION_SIZE: usize = 8;

/// What a relocation refers to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RelocationTarget {
    /// `r_extern` set: symbol table index.
    Symbol(u32),
    /// `r_extern` clear: 1-based section ordinal, or `R_ABS` (0). For
    /// `ARM64_RELOC_ADDEND` this field is the raw 24-bit addend instead.
    Section(u32),
    /// Scattered relocation: the address of the target (`r_value`).
    Scattered(u32),
}

/// A decoded relocation entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Relocation {
    /// Offset of the fixup from the start of the section (`r_address`; 24
    /// bits for scattered entries).
    pub address: u32,
    /// Architecture-specific type.
    pub r_type: u8,
    /// Size of the fixup: 0 = 1 byte, 1 = 2, 2 = 4, 3 = 8.
    pub length: u8,
    /// PC-relative.
    pub pcrel: bool,
    /// The target.
    pub target: RelocationTarget,
}

impl Relocation {
    /// Decodes one entry. Scattered entries exist only on 32-bit
    /// architectures; `cpu_type` decides whether the high address bit means
    /// scattered.
    #[inline]
    #[must_use]
    pub fn decode(record: [u8; RELOCATION_SIZE], endian: Endian, cpu_type: u32) -> Self {
        let [a, b, c, d, e, f, g, h] = record;
        let (word0, word1) = if endian.is_big() {
            (
                u32::from_be_bytes([a, b, c, d]),
                u32::from_be_bytes([e, f, g, h]),
            )
        } else {
            (
                u32::from_le_bytes([a, b, c, d]),
                u32::from_le_bytes([e, f, g, h]),
            )
        };
        let can_scatter = cpu_type & (CPU_ARCH_ABI64 | CPU_ARCH_ABI64_32) == 0;
        if can_scatter && word0 & R_SCATTERED != 0 {
            return Self {
                address: word0 & 0x00ff_ffff,
                r_type: ((word0 >> 24) & 0xf) as u8,
                length: ((word0 >> 28) & 0x3) as u8,
                pcrel: (word0 >> 30) & 1 != 0,
                target: RelocationTarget::Scattered(word1),
            };
        }
        let (symbolnum, pcrel, length, is_extern, r_type) = if endian.is_big() {
            (
                word1 >> 8,
                (word1 >> 7) & 1 != 0,
                ((word1 >> 5) & 3) as u8,
                (word1 >> 4) & 1 != 0,
                (word1 & 0xf) as u8,
            )
        } else {
            (
                word1 & 0x00ff_ffff,
                (word1 >> 24) & 1 != 0,
                ((word1 >> 25) & 3) as u8,
                (word1 >> 27) & 1 != 0,
                (word1 >> 28) as u8,
            )
        };
        Self {
            address: word0,
            r_type,
            length,
            pcrel,
            target: if is_extern {
                RelocationTarget::Symbol(symbolnum)
            } else {
                RelocationTarget::Section(symbolnum)
            },
        }
    }

    /// Size of the fixup in bytes.
    #[must_use]
    pub fn size(&self) -> u8 {
        1u8 << (self.length & 3)
    }

    /// Whether `r_extern` is set.
    #[must_use]
    pub fn is_extern(&self) -> bool {
        matches!(self.target, RelocationTarget::Symbol(_))
    }

    /// Whether this is a scattered entry.
    #[must_use]
    pub fn is_scattered(&self) -> bool {
        matches!(self.target, RelocationTarget::Scattered(_))
    }

    /// The symbol index, for external relocations.
    #[must_use]
    pub fn symbol(&self) -> Option<u32> {
        match self.target {
            RelocationTarget::Symbol(index) => Some(index),
            _ => None,
        }
    }

    /// The type name for `cpu_type`, such as `"ARM64_RELOC_PAGE21"`.
    #[must_use]
    pub fn type_name(&self, cpu_type: u32) -> Option<&'static str> {
        reloc_name(cpu_type, self.r_type)
    }
}

/// A slice of relocation entries.
#[derive(Clone, Copy, Debug)]
pub struct RelocationTable<'a> {
    data: &'a [u8],
    file_offset: u64,
    endian: Endian,
    cpu_type: u32,
}

impl<'a> RelocationTable<'a> {
    /// Wraps relocation bytes found at `file_offset`.
    #[must_use]
    pub fn new(data: &'a [u8], file_offset: u64, endian: Endian, cpu_type: u32) -> Self {
        Self {
            data,
            file_offset,
            endian,
            cpu_type,
        }
    }

    /// Number of entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.data.len() / RELOCATION_SIZE
    }

    /// Whether there are no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// File offset of the table.
    #[must_use]
    pub fn file_offset(&self) -> u64 {
        self.file_offset
    }

    /// Decodes entry `index`.
    #[must_use]
    pub fn get(&self, index: usize) -> Option<Relocation> {
        let start = index.checked_mul(RELOCATION_SIZE)?;
        let record = super::bytes::array::<RELOCATION_SIZE>(self.data, start)?;
        Some(Relocation::decode(record, self.endian, self.cpu_type))
    }

    /// Iterates over the raw entries, unpaired.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = Relocation> + use<'a> {
        let (endian, cpu_type) = (self.endian, self.cpu_type);
        self.data
            .as_chunks::<RELOCATION_SIZE>()
            .0
            .iter()
            .map(move |record| Relocation::decode(*record, endian, cpu_type))
    }

    /// Iterates over the entries with pairs folded together.
    #[must_use]
    pub fn paired(&self, source: Source<'a>) -> PairedRelocationIter<'a> {
        PairedRelocationIter {
            table: *self,
            index: 0,
            source,
        }
    }
}

/// A relocation with the entries that modify it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PairedRelocation {
    /// Index of the main entry in the section's relocation table.
    pub index: usize,
    /// Index of the first entry of the group (the `ADDEND` or `SUBTRACTOR`
    /// entry, when there is one; otherwise `index`).
    pub first_index: usize,
    /// The main entry.
    pub relocation: Relocation,
    /// The explicit addend from a preceding `ARM64_RELOC_ADDEND`.
    pub addend: Option<i32>,
    /// The preceding `SUBTRACTOR` entry: its target is subtracted.
    pub subtractor: Option<Relocation>,
    /// The following `PAIR` entry (i386 and 32-bit Arm).
    pub pair: Option<Relocation>,
}

/// Iterator over [`PairedRelocation`]s. A broken pair is reported as an
/// error and ends iteration.
#[derive(Clone, Debug)]
pub struct PairedRelocationIter<'a> {
    table: RelocationTable<'a>,
    index: usize,
    source: Source<'a>,
}

impl PairedRelocationIter<'_> {
    #[cold]
    fn fail(&mut self, index: usize, what: &str) -> Option<Result<PairedRelocation>> {
        let offset = self
            .table
            .file_offset
            .saturating_add(to_u64(index).saturating_mul(to_u64(RELOCATION_SIZE)));
        let name = self
            .table
            .get(index)
            .and_then(|r| r.type_name(self.table.cpu_type))
            .unwrap_or("relocation");
        self.index = self.table.len();
        Some(Err(self.source.malformed(
            offset,
            format!("{name} (relocation {index}: {what})"),
        )))
    }
}

/// Sign-extends a 24-bit field.
fn sign_extend_24(value: u32) -> i32 {
    ((value << 8) as i32) >> 8
}

impl Iterator for PairedRelocationIter<'_> {
    type Item = Result<PairedRelocation>;

    fn next(&mut self) -> Option<Self::Item> {
        let first_index = self.index;
        let first = self.table.get(first_index)?;
        let cpu_type = self.table.cpu_type;
        let mut index = first_index;
        let mut relocation = first;
        let mut addend = None;
        let mut subtractor = None;
        let mut pair = None;

        let is_arm64 = matches!(cpu_type, CPU_TYPE_ARM64 | CPU_TYPE_ARM64_32);
        let (subtractor_type, unsigned_type) = match cpu_type {
            CPU_TYPE_X86_64 => (Some(X86_64_RELOC_SUBTRACTOR), X86_64_RELOC_UNSIGNED),
            _ if is_arm64 => (Some(ARM64_RELOC_SUBTRACTOR), ARM64_RELOC_UNSIGNED),
            _ => (None, 0),
        };

        if is_arm64 && first.r_type == ARM64_RELOC_ADDEND {
            let RelocationTarget::Section(raw) = first.target else {
                return self.fail(first_index, "external addend");
            };
            addend = Some(sign_extend_24(raw));
            index = first_index.saturating_add(1);
            let Some(next) = self.table.get(index) else {
                return self.fail(first_index, "not followed by another relocation");
            };
            if !matches!(
                next.r_type,
                ARM64_RELOC_BRANCH26 | ARM64_RELOC_PAGE21 | ARM64_RELOC_PAGEOFF12
            ) {
                return self.fail(first_index, "not followed by BRANCH26, PAGE21 or PAGEOFF12");
            }
            if next.address != first.address {
                return self.fail(first_index, "address differs from the next relocation");
            }
            relocation = next;
        } else if subtractor_type == Some(first.r_type) {
            index = first_index.saturating_add(1);
            let Some(next) = self.table.get(index) else {
                return self.fail(first_index, "not followed by another relocation");
            };
            if next.r_type != unsigned_type {
                return self.fail(first_index, "not followed by UNSIGNED");
            }
            if next.address != first.address {
                return self.fail(first_index, "address differs from the next relocation");
            }
            subtractor = Some(first);
            relocation = next;
        } else {
            let (needs_pair, pair_type) = match cpu_type {
                CPU_TYPE_X86 => (
                    matches!(
                        first.r_type,
                        GENERIC_RELOC_SECTDIFF | GENERIC_RELOC_LOCAL_SECTDIFF
                    ) && first.is_scattered(),
                    GENERIC_RELOC_PAIR,
                ),
                CPU_TYPE_ARM => (
                    matches!(
                        first.r_type,
                        ARM_RELOC_SECTDIFF
                            | ARM_RELOC_LOCAL_SECTDIFF
                            | ARM_RELOC_HALF
                            | ARM_RELOC_HALF_SECTDIFF
                    ),
                    ARM_RELOC_PAIR,
                ),
                _ => (false, 0),
            };
            if needs_pair {
                let at = first_index.saturating_add(1);
                match self.table.get(at) {
                    Some(next) if next.r_type == pair_type => {
                        pair = Some(next);
                        self.index = at;
                    }
                    _ => return self.fail(first_index, "not followed by PAIR"),
                }
            }
        }
        self.index = self.index.max(index).saturating_add(1);
        Some(Ok(PairedRelocation {
            index,
            first_index,
            relocation,
            addend,
            subtractor,
            pair,
        }))
    }
}

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)]
mod tests {
    use super::*;
    use std::path::Path;

    fn entry(
        address: u32,
        symbolnum: u32,
        pcrel: bool,
        length: u32,
        ext: bool,
        ty: u32,
    ) -> [u8; 8] {
        let word1 = symbolnum
            | (u32::from(pcrel) << 24)
            | (length << 25)
            | (u32::from(ext) << 27)
            | (ty << 28);
        let mut out = [0; 8];
        out[..4].copy_from_slice(&address.to_le_bytes());
        out[4..].copy_from_slice(&word1.to_le_bytes());
        out
    }

    #[test]
    fn pairs_arm64() {
        let src = Source::new(Path::new("t.o"));
        let mut data = Vec::new();
        data.extend(entry(0x10, 0xff_fff0, false, 2, false, 10)); // ADDEND -16
        data.extend(entry(0x10, 3, true, 2, true, 3)); // PAGE21
        data.extend(entry(0x20, 1, false, 3, true, 1)); // SUBTRACTOR
        data.extend(entry(0x20, 2, false, 3, true, 0)); // UNSIGNED
        data.extend(entry(0x30, 4, true, 2, true, 2)); // BRANCH26
        let table = RelocationTable::new(&data, 0x100, Endian::LITTLE, CPU_TYPE_ARM64);
        assert_eq!(table.len(), 5);
        let paired: Vec<_> = table.paired(src).collect::<Result<_>>().unwrap();
        assert_eq!(paired.len(), 3);
        assert_eq!(paired[0].addend, Some(-16));
        assert_eq!(paired[0].relocation.r_type, ARM64_RELOC_PAGE21);
        assert_eq!((paired[0].first_index, paired[0].index), (0, 1));
        assert_eq!(paired[1].subtractor.unwrap().symbol(), Some(1));
        assert_eq!(paired[1].relocation.symbol(), Some(2));
        assert_eq!(
            paired[2].relocation.type_name(CPU_TYPE_ARM64),
            Some("ARM64_RELOC_BRANCH26")
        );
        assert_eq!(paired[2].relocation.size(), 4);

        // SUBTRACTOR at the end.
        let table = RelocationTable::new(&data[16..24], 0, Endian::LITTLE, CPU_TYPE_ARM64);
        assert!(table.paired(src).next().unwrap().is_err());
    }

    #[test]
    fn scattered_i386() {
        let src = Source::new(Path::new("t.o"));
        let mut data = Vec::new();
        // SECTDIFF, length 2, scattered, address 0x40, value 0x1000.
        let word0 = R_SCATTERED | (2 << 28) | (u32::from(GENERIC_RELOC_SECTDIFF) << 24) | 0x40;
        data.extend_from_slice(&word0.to_le_bytes());
        data.extend_from_slice(&0x1000u32.to_le_bytes());
        let word0 = R_SCATTERED | (2 << 28) | (u32::from(GENERIC_RELOC_PAIR) << 24);
        data.extend_from_slice(&word0.to_le_bytes());
        data.extend_from_slice(&0x2000u32.to_le_bytes());
        let table = RelocationTable::new(&data, 0, Endian::LITTLE, CPU_TYPE_X86);
        let paired: Vec<_> = table.paired(src).collect::<Result<_>>().unwrap();
        assert_eq!(paired.len(), 1);
        assert_eq!(
            paired[0].relocation.target,
            RelocationTarget::Scattered(0x1000)
        );
        assert_eq!(paired[0].relocation.address, 0x40);
        assert_eq!(
            paired[0].pair.unwrap().target,
            RelocationTarget::Scattered(0x2000)
        );
        // On x86_64 the same bit is part of the address.
        let table = RelocationTable::new(&data, 0, Endian::LITTLE, CPU_TYPE_X86_64);
        assert!(!table.get(0).unwrap().is_scattered());
    }
}
