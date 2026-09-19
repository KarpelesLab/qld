//! The exception index (`.ARM.exidx`).
//!
//! The Arm EHABI describes unwinding with a table of eight-byte entries,
//! sorted by the address of the code they cover: a `PREL31` reference to
//! the function, and either inline unwind instructions, a `PREL31`
//! reference to a `.ARM.extab` entry, or `EXIDX_CANTUNWIND`. Unwinders
//! find it through `PT_ARM_EXIDX` (or `__exidx_start`/`__exidx_end`) and
//! binary-search it, so the linker cannot simply concatenate the input
//! sections: the table has to be in address order, cover every executable
//! section, and end with a sentinel.
//!
//! [`plan`] builds the table before layout, as lld does:
//!
//! - executable input sections are taken in the order layout will place
//!   them; each one contributes its `.ARM.exidx` section (a
//!   `SHF_LINK_ORDER` section pointing at it), or a linker-made
//!   `EXIDX_CANTUNWIND` entry when it has none;
//! - a section whose entries all say the same as the entry before it is
//!   dropped (`--no-merge-exidx-entries` keeps them, and so does a link
//!   whose section order is not the default one, where the planned order
//!   may not be the final one);
//! - a sentinel `EXIDX_CANTUNWIND` entry covers the end of the last
//!   executable section.
//!
//! The whole table takes the place of the first `.ARM.exidx` input
//! section; the others become empty. [`write`] fills it once addresses are
//! known, in address order (which is the planned order unless a linker
//! script or `--symbol-ordering-file` moved something).

#![deny(clippy::arithmetic_side_effects)]

use crate::arch::arm::{EXIDX_CANTUNWIND, Field, read32, write32};
use crate::elf::place::Placement;
use crate::elf::read::consts::{SHF_ALLOC, SHF_EXECINSTR};
use crate::elf::refs::Refs;
use crate::ids::SectionId;

use super::SHT_ARM_EXIDX;

/// Size of one exception index entry.
pub const ENTRY_SIZE: u64 = 8;

/// One piece of the table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Piece {
    /// The entries of one `.ARM.exidx` input section, covering code in
    /// `text`.
    Input {
        /// The `.ARM.exidx` section.
        exidx: SectionId,
        /// The executable section it describes.
        text: SectionId,
        /// Its size in bytes.
        size: u64,
    },
    /// A linker-made `EXIDX_CANTUNWIND` entry for a section without
    /// unwind information.
    CantUnwind {
        /// The executable section it covers.
        text: SectionId,
    },
}

impl Piece {
    /// The executable section the piece describes.
    #[must_use]
    pub fn text(self) -> SectionId {
        match self {
            Self::Input { text, .. } | Self::CantUnwind { text } => text,
        }
    }

    /// Its size in the table.
    #[must_use]
    pub fn size(self) -> u64 {
        match self {
            Self::Input { size, .. } => size,
            Self::CantUnwind { .. } => ENTRY_SIZE,
        }
    }
}

/// The exception index of a link.
#[derive(Clone, Debug, Default)]
pub struct Table {
    /// The `.ARM.exidx` input section whose place holds the table; the
    /// others are laid out empty.
    pub first: SectionId,
    /// The table's pieces, in planned order.
    pub pieces: Vec<Piece>,
    /// The section whose end the sentinel entry covers.
    pub sentinel: SectionId,
    /// The table's size, the pieces plus the sentinel.
    pub size: u64,
}

/// Where layout will place an input section: its output section's rank,
/// the input description that matched it, then its ID.
type OrderKey = ((u16, u32), u16, SectionId);

/// That key for `id`, or `None` when it has no output section.
fn order_key(placement: &Placement<'_>, id: SectionId) -> Option<OrderKey> {
    let output = placement.output_of(id)?;
    let rank = placement.outputs.get(output as usize)?.rank;
    let sub = placement.sub.get(id.index()).copied().unwrap_or(0);
    Some((rank, sub, id))
}

/// Whether the entries of `section` (the contents of a `.ARM.exidx`
/// section) all repeat `previous`, the last unwind word before it, so that
/// the entry before covers them too (lld's `isDuplicateArmExidxSec`).
/// `None` for the synthesized `EXIDX_CANTUNWIND` of a section without
/// unwind information.
fn is_duplicate(previous: u32, section: Option<&[u8]>) -> bool {
    // A reference to `.ARM.extab` describes one function only.
    if previous & 0x8000_0000 == 0 && previous != EXIDX_CANTUNWIND {
        return false;
    }
    let Some(data) = section else {
        return previous == EXIDX_CANTUNWIND;
    };
    let mut at = 4usize;
    while at < data.len() {
        let Some(word) = read32(data, at) else {
            return false;
        };
        if word != previous || (word & 0x8000_0000 == 0 && word != EXIDX_CANTUNWIND) {
            return false;
        }
        at = at.saturating_add(8);
    }
    true
}

/// The last unwind word of `data`, which decides whether the next section
/// repeats it.
fn last_unwind(data: Option<&[u8]>) -> u32 {
    let Some(data) = data else {
        return EXIDX_CANTUNWIND;
    };
    data.len()
        .checked_sub(4)
        .and_then(|at| read32(data, at))
        .unwrap_or(EXIDX_CANTUNWIND)
}

/// Builds the exception index. `merge` drops sections whose entries repeat
/// the one before (`--merge-exidx-entries`, the default, and only when the
/// planned order is the final one). `None` when no input has one.
#[must_use]
pub fn plan<F: crate::elf::read::ElfFormat>(
    refs: &Refs<'_, '_, F>,
    placement: &Placement<'_>,
    merge: bool,
) -> Option<Table> {
    // Every live `.ARM.exidx` section, by the executable section it
    // describes, and every executable section, in layout order.
    let mut exidx: Vec<(SectionId, SectionId, u64)> = Vec::new();
    let mut executable: Vec<(OrderKey, SectionId)> = Vec::new();
    for (file_index, file) in refs.files.iter().enumerate() {
        let Some(object) = &file.object else {
            continue;
        };
        for (index, section) in object.sections.iter().enumerate() {
            let index = u32::try_from(index).unwrap_or(u32::MAX);
            let Some(id) = refs.sections.id(file_index, index) else {
                continue;
            };
            if !refs.sections.is_live(id) || placement.output_of(id).is_none() {
                continue;
            }
            let header = &section.header;
            if header.sh_type == SHT_ARM_EXIDX {
                let text = refs.sections.id(file_index, header.sh_link);
                if let Some(text) = text.filter(|&t| refs.sections.is_live(t)) {
                    exidx.push((text, id, header.sh_size));
                }
                continue;
            }
            if header.sh_flags & (SHF_ALLOC | SHF_EXECINSTR) == (SHF_ALLOC | SHF_EXECINSTR)
                && header.sh_size > 0
                && section.kind != crate::elf::object::SectionKind::Ignored
                && let Some(key) = order_key(placement, id)
            {
                executable.push((key, id));
            }
        }
    }
    if exidx.is_empty() {
        return None;
    }
    exidx.sort_unstable();
    executable.sort_unstable();
    let first = exidx
        .iter()
        .filter_map(|&(_, id, _)| Some((order_key(placement, id)?, id)))
        .min()?
        .1;
    let sentinel = executable.last()?.1;
    let data = |id: SectionId| -> Option<&[u8]> {
        let (file, index) = refs.sections.locate(id)?;
        let object = refs.files.get(file)?.object.as_ref()?;
        let section = object.section(index)?;
        object.elf.section_data(&section.header).ok()
    };
    let mut pieces: Vec<Piece> = Vec::with_capacity(executable.len());
    let mut size = 0u64;
    let mut previous: Option<u32> = None;
    for (_, text) in executable {
        let at = exidx.partition_point(|&(t, ..)| t < text);
        let found = exidx.get(at).copied().filter(|&(t, ..)| t == text);
        let piece = match found {
            Some((_, id, section_size)) if section_size > 0 => Piece::Input {
                exidx: id,
                text,
                size: section_size,
            },
            _ => Piece::CantUnwind { text },
        };
        let contents = match piece {
            Piece::Input { exidx, .. } => data(exidx),
            Piece::CantUnwind { .. } => None,
        };
        if merge
            && let Some(previous) = previous
            && is_duplicate(previous, contents)
        {
            continue;
        }
        previous = Some(last_unwind(contents));
        size = size.saturating_add(piece.size());
        pieces.push(piece);
    }
    Some(Table {
        first,
        pieces,
        sentinel,
        // The sentinel entry.
        size: size.saturating_add(ENTRY_SIZE),
    })
}

/// Writes a `PREL31` reference to `target` at `at` in `out`, whose
/// address is `place`.
fn put_prel31(out: &mut [u8], at: usize, place: u64, target: u64) -> Option<()> {
    let insn = read32(out, at)?;
    let value = i64::from(target.wrapping_sub(place) as u32 as i32);
    let encoded = Field::Prel31.encode(insn, value).ok()?;
    write32(out, at, encoded)
}

/// Writes the exception index at address `base` into `out`.
///
/// `address` gives the address of an input section (`None` when it is not
/// in the output), and `relocate` applies the relocations of a
/// `.ARM.exidx` input section that has been copied to `at` in `out`.
pub fn write<'d>(
    table: &Table,
    base: u64,
    out: &mut [u8],
    address: &dyn Fn(SectionId) -> Option<u64>,
    size_of: &dyn Fn(SectionId) -> u64,
    contents: &dyn Fn(SectionId) -> Option<&'d [u8]>,
    relocate: &mut dyn FnMut(SectionId, u64, &mut [u8]),
) {
    // In address order: the planned order, unless the layout moved
    // sections apart from it.
    let mut pieces: Vec<(u64, usize, Piece)> = table
        .pieces
        .iter()
        .enumerate()
        .map(|(index, &piece)| (address(piece.text()).unwrap_or(0), index, piece))
        .collect();
    pieces.sort_unstable_by_key(|&(at, index, _)| (at, index));
    let mut offset = 0u64;
    for (text_address, _, piece) in pieces {
        let at = usize::try_from(offset).unwrap_or(usize::MAX);
        let place = base.wrapping_add(offset);
        match piece {
            Piece::Input { exidx, size, .. } => {
                let end = at.saturating_add(usize::try_from(size).unwrap_or(0));
                if let (Some(data), Some(dest)) = (contents(exidx), out.get_mut(at..end)) {
                    let len = dest.len().min(data.len());
                    if let (Some(dest), Some(data)) = (dest.get_mut(..len), data.get(..len)) {
                        dest.copy_from_slice(data);
                    }
                    relocate(exidx, place, dest);
                }
                offset = offset.saturating_add(size);
            }
            Piece::CantUnwind { .. } => {
                let _ = write32(out, at, 0);
                let _ = put_prel31(out, at, place, text_address);
                let _ = write32(out, at.saturating_add(4), EXIDX_CANTUNWIND);
                offset = offset.saturating_add(ENTRY_SIZE);
            }
        }
    }
    // The sentinel covers the end of the last executable section, so that
    // an unwinder searching past it stops.
    let at = usize::try_from(offset).unwrap_or(usize::MAX);
    let place = base.wrapping_add(offset);
    let end = address(table.sentinel)
        .unwrap_or(0)
        .wrapping_add(size_of(table.sentinel));
    let _ = write32(out, at, 0);
    let _ = put_prel31(out, at, place, end);
    let _ = write32(out, at.saturating_add(4), EXIDX_CANTUNWIND);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(unwind: u32) -> Vec<u8> {
        let mut bytes = vec![0u8; 8];
        write32(&mut bytes, 4, unwind);
        bytes
    }

    #[test]
    fn duplicate_sections_repeat_the_entry_before() {
        let cantunwind = entry(EXIDX_CANTUNWIND);
        let inline = entry(0x8001_9b40);
        let extab = entry(0x0000_0010);
        assert!(is_duplicate(EXIDX_CANTUNWIND, Some(&cantunwind)));
        assert!(is_duplicate(EXIDX_CANTUNWIND, None));
        assert!(!is_duplicate(0x8001_9b40, Some(&cantunwind)));
        assert!(is_duplicate(0x8001_9b40, Some(&inline)));
        // A reference into `.ARM.extab` covers one function only.
        assert!(!is_duplicate(0x10, Some(&extab)));
        assert!(!is_duplicate(0x8001_9b40, Some(&extab)));
        // Two entries, the second different.
        let mut two = inline.clone();
        two.extend_from_slice(&cantunwind);
        assert!(!is_duplicate(0x8001_9b40, Some(&two)));
        assert_eq!(last_unwind(Some(&two)), EXIDX_CANTUNWIND);
        assert_eq!(last_unwind(None), EXIDX_CANTUNWIND);
    }

    #[test]
    fn writes_entries_in_address_order() {
        let table = Table {
            first: SectionId::new(0),
            pieces: vec![
                Piece::CantUnwind {
                    text: SectionId::new(1),
                },
                Piece::CantUnwind {
                    text: SectionId::new(2),
                },
            ],
            sentinel: SectionId::new(2),
            size: 24,
        };
        let mut out = vec![0u8; 24];
        // Section 2 is placed before section 1.
        let address = |id: SectionId| {
            Some(if id == SectionId::new(1) {
                0x2000
            } else {
                0x1000
            })
        };
        write(
            &table,
            0x1_0000,
            &mut out,
            &address,
            &|_| 0x40,
            &|_| None,
            &mut |_, _, _| {},
        );
        let target = |at: usize| {
            let word = read32(&out, at).unwrap();
            0x1_0000u64
                .wrapping_add(at as u64)
                .wrapping_add(Field::Prel31.decode(word) as u64)
        };
        assert_eq!(target(0), 0x1000);
        assert_eq!(read32(&out, 4), Some(EXIDX_CANTUNWIND));
        assert_eq!(target(8), 0x2000);
        // The sentinel covers the end of the last section.
        assert_eq!(target(16), 0x1040);
        assert_eq!(read32(&out, 20), Some(EXIDX_CANTUNWIND));
    }
}
