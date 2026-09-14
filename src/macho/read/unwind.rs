//! `__LD,__compact_unwind` entries.
//!
//! Each record describes one function (or a range of it):
//!
//! | Field | 64-bit | 32-bit |
//! | --- | --- | --- |
//! | function address | 8 | 4 |
//! | function length | 4 | 4 |
//! | encoding | 4 | 4 |
//! | personality | 8 | 4 |
//! | LSDA | 8 | 4 |
//!
//! The address, personality and LSDA fields are normally relocated; the
//! relocation is returned with each field, so the linker can find the
//! function's atom, the personality symbol and the LSDA atom.

use super::bytes::to_u64;
use super::consts::{
    CPU_TYPE_ARM64, CPU_TYPE_X86_64, UNWIND_ARM64_MODE_DWARF, UNWIND_HAS_LSDA, UNWIND_MODE_MASK,
    UNWIND_PERSONALITY_MASK, UNWIND_X86_64_MODE_DWARF,
};
use super::object::ObjectFile;
use super::reloc::PairedRelocation;
use crate::error::Result;

/// A relocatable field of a compact unwind record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnwindField {
    /// The value stored in the section (an address in the object, or zero).
    pub value: u64,
    /// The relocation applied to the field, if any.
    pub relocation: Option<PairedRelocation>,
}

/// One `__compact_unwind` record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompactUnwindEntry {
    /// Offset of the record within the section.
    pub offset: u64,
    /// The function address.
    pub function: UnwindField,
    /// Length of the function (or range) in bytes.
    pub length: u32,
    /// The compact unwind encoding.
    pub encoding: u32,
    /// The personality function (usually a relocation to an undefined
    /// symbol such as `___gxx_personality_v0`).
    pub personality: UnwindField,
    /// The language-specific data area.
    pub lsda: UnwindField,
}

impl CompactUnwindEntry {
    /// Whether the encoding says to use the DWARF CFI in `__eh_frame`
    /// instead, for CPU type `cpu_type`.
    #[must_use]
    pub fn needs_dwarf(&self, cpu_type: u32) -> bool {
        let mode = self.encoding & UNWIND_MODE_MASK;
        match cpu_type {
            CPU_TYPE_ARM64 => mode == UNWIND_ARM64_MODE_DWARF,
            CPU_TYPE_X86_64 => mode == UNWIND_X86_64_MODE_DWARF,
            _ => false,
        }
    }

    /// Whether `UNWIND_HAS_LSDA` is set.
    #[must_use]
    pub fn has_lsda(&self) -> bool {
        self.encoding & UNWIND_HAS_LSDA != 0
    }

    /// The personality index bits of the encoding (0 when unset).
    #[must_use]
    pub fn personality_index(&self) -> u32 {
        (self.encoding & UNWIND_PERSONALITY_MASK) >> 28
    }
}

/// Decodes the `__LD,__compact_unwind` section of `object`, if it has one.
///
/// # Errors
///
/// Returns `Error::Malformed` if the section size is not a multiple of the
/// record size, or a relocation is malformed or does not start at a
/// relocatable field.
pub fn compact_unwind_entries(object: &ObjectFile<'_>) -> Result<Vec<CompactUnwindEntry>> {
    let Some((index, section)) = object.find_section(b"__LD", b"__compact_unwind") else {
        return Ok(Vec::new());
    };
    let data = object.section_data(index)?;
    let is64 = object.file().is64();
    let endian = object.file().endian();
    let (record, word, personality_at, lsda_at) = if is64 {
        (32usize, 8usize, 16usize, 24usize)
    } else {
        (20, 4, 12, 16)
    };
    let source = object.source();
    if data.len().checked_rem(record) != Some(0) {
        return Err(source.malformed(
            u64::from(section.offset),
            format!(
                "__compact_unwind (size {:#x} is not a multiple of {record})",
                data.len()
            ),
        ));
    }
    let mut entries = Vec::with_capacity(data.len().checked_div(record).unwrap_or(0));
    for (i, chunk) in data.chunks_exact(record).enumerate() {
        let read_word = |at: usize| endian.word(chunk, at, is64).unwrap_or(0);
        let field = |at: usize| UnwindField {
            value: read_word(at),
            relocation: None,
        };
        entries.push(CompactUnwindEntry {
            offset: to_u64(i.saturating_mul(record)),
            function: field(0),
            length: endian.u32(chunk, word).unwrap_or(0),
            encoding: endian.u32(chunk, word.saturating_add(4)).unwrap_or(0),
            personality: field(personality_at),
            lsda: field(lsda_at),
        });
    }
    let table = object.relocations(index)?;
    for relocation in table.paired(source) {
        let relocation = relocation?;
        let address = usize::try_from(relocation.relocation.address).unwrap_or(usize::MAX);
        let (entry, within) = (
            address.checked_div(record).unwrap_or(usize::MAX),
            address.checked_rem(record).unwrap_or(usize::MAX),
        );
        let slot = entries.get_mut(entry).and_then(|e| match within {
            0 => Some(&mut e.function),
            w if w == personality_at => Some(&mut e.personality),
            w if w == lsda_at => Some(&mut e.lsda),
            _ => None,
        });
        match slot {
            Some(field) if field.relocation.is_none() => field.relocation = Some(relocation),
            _ => {
                return Err(source.malformed(
                    table
                        .file_offset()
                        .saturating_add(to_u64(relocation.index).saturating_mul(8)),
                    format!(
                        "__compact_unwind relocation (address {address:#x} is not a relocatable field)"
                    ),
                ));
            }
        }
    }
    Ok(entries)
}
