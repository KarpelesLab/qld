//! ARM64 relocations and range-extension thunks.
//!
//! COFF ARM64 relocations keep their addend in the instruction, as lld and
//! `link.exe` read it: the `adrp`/`adr` immediate is a byte offset added
//! to the symbol, the `add`/`ldr` immediate is added to the low 12 bits of
//! the (scaled) offset, and a branch immediate is added to the destination.
//! Instruction fields are packed with [`crate::arch::aarch64`], which the
//! ELF backend shares.
//!
//! # Thunks
//!
//! `b`/`bl` (`BRANCH26`) reach ±128 MiB, `b.cond`/`cbz` (`BRANCH19`)
//! ±1 MiB and `tbz` (`BRANCH14`) ±32 KiB. A branch that cannot reach its
//! destination goes through a thunk, `adrp x16; add x16, x16, :lo12:; br
//! x16`, which reaches ±4 GiB. Thunks are grouped in a block placed right
//! after the input section whose branches need them, so a thunk is as near
//! the branch as it can be. The link driver discovers them while
//! relocating: a branch out of range with no thunk yet is recorded in
//! [`Applied::thunk_requests`], the driver adds it to the [`Thunks`] plan
//! and lays the image out again, until no branch asks for a new thunk.
//! Blocks only grow, so this settles; the ordering of every block is by
//! destination, so the image does not depend on the order the requests
//! arrived in.

#![deny(clippy::arithmetic_side_effects)]

use std::collections::BTreeMap;

use crate::arch::aarch64::{self, Field as Insn, read_insn, write_insn};

use super::read::consts::arm64::{
    IMAGE_REL_ARM64_ABSOLUTE, IMAGE_REL_ARM64_ADDR32, IMAGE_REL_ARM64_ADDR32NB,
    IMAGE_REL_ARM64_ADDR64, IMAGE_REL_ARM64_BRANCH14, IMAGE_REL_ARM64_BRANCH19,
    IMAGE_REL_ARM64_BRANCH26, IMAGE_REL_ARM64_PAGEBASE_REL21, IMAGE_REL_ARM64_PAGEOFFSET_12A,
    IMAGE_REL_ARM64_PAGEOFFSET_12L, IMAGE_REL_ARM64_REL21, IMAGE_REL_ARM64_REL32,
    IMAGE_REL_ARM64_SECREL, IMAGE_REL_ARM64_SECREL_HIGH12A, IMAGE_REL_ARM64_SECREL_LOW12A,
    IMAGE_REL_ARM64_SECREL_LOW12L, IMAGE_REL_ARM64_SECTION,
};
use super::reloc::{Applied, Field, past_the_end};

/// Size of one thunk: three instructions.
pub const THUNK_SIZE: u32 = 12;

/// The thunks of an image: for each input section `(file, section)` whose
/// branches need them, the destination RVAs of its block, sorted.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Thunks {
    /// Destinations by input section.
    pub blocks: BTreeMap<(u32, u32), Vec<u32>>,
}

impl Thunks {
    /// Whether there are no thunks.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// Size of the block after input section `section` of `file`.
    #[must_use]
    pub fn block_size(&self, file: u32, section: u32) -> u32 {
        self.blocks.get(&(file, section)).map_or(0, |targets| {
            u32::try_from(targets.len())
                .unwrap_or(u32::MAX)
                .saturating_mul(THUNK_SIZE)
        })
    }

    /// Adds the requested thunks. Returns whether any was new.
    pub fn add(&mut self, requests: &[(u32, u32, u32)]) -> bool {
        let mut changed = false;
        for &(file, section, target) in requests {
            let block = self.blocks.entry((file, section)).or_default();
            if let Err(at) = block.binary_search(&target) {
                block.insert(at, target);
                changed = true;
            }
        }
        changed
    }

    /// The index of the thunk for `target` in the block of `(file,
    /// section)`.
    #[must_use]
    pub fn index_of(&self, file: u32, section: u32, target: u32) -> Option<u32> {
        let block = self.blocks.get(&(file, section))?;
        let at = block.binary_search(&target).ok()?;
        u32::try_from(at).ok()
    }
}

/// Writes the thunks of one block, placed at `rva`, into `out`.
///
/// # Errors
///
/// Returns a message when a destination is more than 4 GiB away, which a
/// PE image cannot be.
pub fn render_block(out: &mut [u8], rva: u32, targets: &[u32]) -> Result<(), String> {
    let mut offset = 0u32;
    for &target in targets {
        let address = rva.wrapping_add(offset);
        aarch64::write_thunk(
            out,
            u64::from(offset),
            u64::from(address),
            u64::from(target),
        )
        .map_err(|_| format!("range-extension thunk to {target:#x} out of range"))?;
        offset = offset.saturating_add(THUNK_SIZE);
    }
    Ok(())
}

/// Applies ARM64 relocation `r_type`, or returns `None` for a type qld
/// does not handle (`TOKEN`, used only by .NET).
pub(super) fn apply(
    field: &Field<'_, '_, '_>,
    data: &mut [u8],
    r_type: u16,
    out: &mut Applied,
) -> Option<Result<(), String>> {
    let at = field.site.at();
    let pc = i64::from(field.site.rva());
    let symbol = field.rva() as i64;
    Some(match r_type {
        IMAGE_REL_ARM64_ABSOLUTE => Ok(()),
        IMAGE_REL_ARM64_ADDR32 => field.addr32(data, out),
        IMAGE_REL_ARM64_ADDR32NB => field.addr32nb(data),
        IMAGE_REL_ARM64_ADDR64 => field.addr64(data, out),
        IMAGE_REL_ARM64_SECREL => field.secrel(data),
        IMAGE_REL_ARM64_SECTION => field.section_index(data),
        IMAGE_REL_ARM64_REL32 => field.rel32(data, 0),
        IMAGE_REL_ARM64_BRANCH26 => branch(field, data, Insn::Branch26, out),
        IMAGE_REL_ARM64_BRANCH19 => branch(field, data, Insn::Branch19, out),
        IMAGE_REL_ARM64_BRANCH14 => branch(field, data, Insn::Branch14, out),
        IMAGE_REL_ARM64_PAGEBASE_REL21 => address(data, at, symbol, pc, true),
        IMAGE_REL_ARM64_REL21 => address(data, at, symbol, pc, false),
        IMAGE_REL_ARM64_PAGEOFFSET_12A => add_low12(data, at, symbol as u64),
        IMAGE_REL_ARM64_PAGEOFFSET_12L => load_low12(data, at, symbol as u64),
        IMAGE_REL_ARM64_SECREL_LOW12A => add_low12(data, at, u64::from(field.section_offset())),
        IMAGE_REL_ARM64_SECREL_HIGH12A => add_low12(
            data,
            at,
            u64::from(field.section_offset())
                .checked_shr(12)
                .unwrap_or(0),
        ),
        IMAGE_REL_ARM64_SECREL_LOW12L => load_low12(data, at, u64::from(field.section_offset())),
        _ => return None,
    })
}

/// The instruction at `at`, or the error for a field past the end.
fn instruction(data: &[u8], at: usize) -> Result<u32, String> {
    read_insn(data, at).ok_or_else(past_the_end)
}

fn store(data: &mut [u8], at: usize, insn: u32) -> Result<(), String> {
    write_insn(data, at, insn).ok_or_else(past_the_end)
}

/// The signed value of the `bits`-bit field at `lsb` of `insn`.
fn signed_field(insn: u32, lsb: u32, bits: u32) -> i64 {
    let raw = insn.checked_shr(lsb).unwrap_or(0);
    let unused = 32u32.saturating_sub(bits);
    i64::from(
        (raw.checked_shl(unused).unwrap_or(0) as i32)
            .checked_shr(unused)
            .unwrap_or(0),
    )
}

/// `BRANCH26`, `BRANCH19` and `BRANCH14`: a branch to the symbol plus the
/// addend already in the immediate, through a thunk when it is too far.
fn branch(
    field: &Field<'_, '_, '_>,
    data: &mut [u8],
    kind: Insn,
    out: &mut Applied,
) -> Result<(), String> {
    let site = field.site;
    let at = site.at();
    let insn = instruction(data, at)?;
    let (lsb, bits) = match kind {
        Insn::Branch26 => (0, 26),
        Insn::Branch19 => (5, 19),
        _ => (5, 14),
    };
    let addend = signed_field(insn, lsb, bits).wrapping_mul(4);
    let target = (field.rva() as i64).wrapping_add(addend);
    let pc = i64::from(site.rva());
    if let Ok(patched) = kind.encode(insn, target.wrapping_sub(pc)) {
        return store(data, at, patched);
    }
    // Out of reach: branch to the thunk, or ask for one.
    let target = target as u32;
    let layout = field.addresses.layout;
    let Some(index) = layout.thunks.index_of(site.file, site.section, target) else {
        out.thunk_requests.push((site.file, site.section, target));
        return Ok(());
    };
    let thunk = layout
        .thunk_rvas
        .get(&(site.file, site.section))
        .copied()
        .ok_or_else(|| "range-extension thunk block was not placed".to_string())?
        .wrapping_add(index.saturating_mul(THUNK_SIZE));
    let patched = kind
        .encode(insn, i64::from(thunk).wrapping_sub(pc))
        .map_err(|_| format!("branch relocation out of range even through a thunk: {target:#x}"))?;
    store(data, at, patched)
}

/// `PAGEBASE_REL21` (`adrp`, `page` set) and `REL21` (`adr`): the
/// immediate already holds a byte addend.
fn address(data: &mut [u8], at: usize, symbol: i64, pc: i64, page: bool) -> Result<(), String> {
    let insn = instruction(data, at)?;
    let low = i64::from(insn.checked_shr(29).unwrap_or(0) & 3);
    let high = signed_field(insn, 5, 19);
    let addend = high.wrapping_mul(4) | low;
    let target = symbol.wrapping_add(addend);
    let patched = if page {
        let delta =
            (aarch64::page(target as u64) as i64).wrapping_sub(aarch64::page(pc as u64) as i64);
        Insn::Adrp21.encode(insn, delta)
    } else {
        Insn::Adr21.encode(insn, target.wrapping_sub(pc))
    }
    .map_err(|_| {
        format!(
            "{} relocation out of range",
            if page { "PAGEBASE_REL21" } else { "REL21" }
        )
    })?;
    store(data, at, patched)
}

/// `PAGEOFFSET_12A` and the `SECREL_*12A` pair: adds `value & 0xfff` to
/// the 12-bit `add` immediate.
fn add_low12(data: &mut [u8], at: usize, value: u64) -> Result<(), String> {
    let insn = instruction(data, at)?;
    let existing = u64::from(insn.checked_shr(10).unwrap_or(0) & 0xfff);
    let sum = (value & 0xfff).wrapping_add(existing) & 0xfff;
    let patched = (insn & !(0xfff << 10)) | ((sum as u32) << 10);
    store(data, at, patched)
}

/// `PAGEOFFSET_12L` and `SECREL_LOW12L`: the low 12 bits of `value`,
/// scaled by the access size, added to the load/store immediate.
fn load_low12(data: &mut [u8], at: usize, value: u64) -> Result<(), String> {
    let insn = instruction(data, at)?;
    let mut scale = insn.checked_shr(30).unwrap_or(0);
    // A 128-bit SIMD access (`ldr q`) has size 0 with bits 26 and 23 set.
    if insn & 0x0480_0000 == 0x0480_0000 {
        scale = scale.saturating_add(4);
    }
    let low = value & 0xfff;
    let mask = 1u64.checked_shl(scale).unwrap_or(1).wrapping_sub(1);
    if low & mask != 0 {
        return Err(format!(
            "misaligned load/store offset {low:#x} for a {}-byte access",
            1u32.checked_shl(scale).unwrap_or(0)
        ));
    }
    let existing = u64::from(insn.checked_shr(10).unwrap_or(0) & 0xfff);
    let limit = 0xfffu64.checked_shr(scale).unwrap_or(0);
    let sum = low.checked_shr(scale).unwrap_or(0).wrapping_add(existing) & limit;
    let patched = (insn & !(0xfff << 10)) | ((sum as u32) << 10);
    store(data, at, patched)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thunk_blocks_are_sorted_and_deduplicated() {
        let mut thunks = Thunks::default();
        assert!(thunks.add(&[(1, 2, 0x9000), (1, 2, 0x5000), (1, 2, 0x9000)]));
        assert!(!thunks.add(&[(1, 2, 0x5000)]));
        assert_eq!(thunks.blocks[&(1, 2)], [0x5000, 0x9000]);
        assert_eq!(thunks.block_size(1, 2), 24);
        assert_eq!(thunks.block_size(1, 3), 0);
        assert_eq!(thunks.index_of(1, 2, 0x9000), Some(1));
    }

    #[test]
    fn page_offsets_add_to_the_immediate() {
        // add x0, x0, #8 plus the low bits of 0x1234.
        let mut data = 0x9100_2000u32.to_le_bytes().to_vec();
        add_low12(&mut data, 0, 0x1234).unwrap();
        let insn = u32::from_le_bytes(data[..4].try_into().unwrap());
        assert_eq!((insn >> 10) & 0xfff, 0x234 + 8);
        // ldr x1, [x0] with an 8-byte-aligned offset: scaled by 8.
        let mut data = 0xf940_0001u32.to_le_bytes().to_vec();
        load_low12(&mut data, 0, 0x1238).unwrap();
        let insn = u32::from_le_bytes(data[..4].try_into().unwrap());
        assert_eq!((insn >> 10) & 0xfff, 0x238 / 8);
        // A misaligned doubleword offset is an error.
        let mut data = 0xf940_0001u32.to_le_bytes().to_vec();
        assert!(load_low12(&mut data, 0, 0x1234).is_err());
    }

    #[test]
    fn adrp_reads_its_byte_addend() {
        // adrp x0, with a byte addend of 0x10 in the immediate.
        let mut data = (0x9000_0000u32 | (0x10 >> 2) << 5).to_le_bytes().to_vec();
        address(&mut data, 0, 0x2ff8, 0x1000, true).unwrap();
        let insn = u32::from_le_bytes(data[..4].try_into().unwrap());
        let pages = ((insn >> 29) & 3) | (((insn >> 5) & 0x7ffff) << 2);
        // 0x2ff8 + 0x10 is on page 0x3000, two pages past 0x1000.
        assert_eq!(pages, 2);
    }
}
