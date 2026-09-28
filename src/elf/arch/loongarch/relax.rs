//! LoongArch linker relaxation decisions, for the shrinking framework of
//! [`crate::elf::arch::shrink`].
//!
//! The decisions are lld's, so the output matches it:
//!
//! - `R_LARCH_ALIGN` padding is trimmed so that the code after it is
//!   aligned (always, even with `--no-relax`); a maximum-bytes limit in the
//!   relocation that the padding would exceed drops all of it;
//! - an adjacent `pcalau12i` + `addi.d`/`ld.d` pair marked `R_LARCH_RELAX`
//!   becomes one `pcaddi` within ±2 MiB, for a local address, a GOT entry
//!   whose indirection goes away, and the general-dynamic, local-dynamic
//!   and descriptor entries;
//! - `pcaddu18i` + `jirl` within ±128 MiB becomes `bl` (or `b`);
//! - a local-exec `lu12i.w`/`add.d`/`addi.d` whose offset fits 12 bits
//!   loses the first two instructions;
//! - the `nop`s that an initial-exec or descriptor sequence relaxed to
//!   local-exec or initial-exec leaves go away.
//!
//! Each decision is the one [`super::classify`] and [`super::relax_tls`]
//! make in the writer, asked here through [`Pass::classify`], so that the
//! bytes the writer keeps and the bytes layout removes cannot disagree.

#![deny(clippy::arithmetic_side_effects)]

use crate::arch::loongarch::{self as insn, B, BL, Field, PCADDI, R_RA, R_ZERO};
use crate::elf::arch::shrink::{Edits, Pass, Rewrite, SectionInput};
use crate::elf::arch::{GotKind, Kind};
use crate::elf::read::Relocation;
use crate::error::Result;

use super::{
    R_LARCH_ALIGN, R_LARCH_B26, R_LARCH_CALL36, R_LARCH_GOT_PC_HI20, R_LARCH_PCALA_HI20,
    R_LARCH_PCREL20_S2, R_LARCH_RELAX, R_LARCH_TLS_DESC_LD, R_LARCH_TLS_DESC_PC_HI20,
    R_LARCH_TLS_DESC_PC_LO12, R_LARCH_TLS_DESC_PCREL20_S2, R_LARCH_TLS_GD_PC_HI20,
    R_LARCH_TLS_GD_PCREL20_S2, R_LARCH_TLS_IE_PC_HI20, R_LARCH_TLS_LD_PC_HI20,
    R_LARCH_TLS_LE_ADD_R, R_LARCH_TLS_LE_HI20_R, R_LARCH_TLS_LE_LO12_R, RELAX_HINT,
};

/// Whether the relocation at `index` is followed by an `R_LARCH_RELAX` at
/// the same offset, which allows its instruction to be rewritten. The same
/// test [`crate::elf::arch::Arch::annotate`] makes for the writer.
fn relaxable(relocs: &[Relocation], index: usize) -> bool {
    let Some(rel) = relocs.get(index) else {
        return false;
    };
    relocs
        .get(index.saturating_add(1))
        .is_some_and(|next| next.r_type == R_LARCH_RELAX && next.offset == rel.offset)
}

/// Whether the relocation at `index` and the one two places later are both
/// relaxable and four bytes apart: the `pcalau12i` + `addi.d`/`ld.d` shape
/// the assembler writes (lld's `isPairRelaxable`).
fn pair_relaxable(relocs: &[Relocation], index: usize) -> bool {
    let (Some(hi), Some(lo)) = (relocs.get(index), relocs.get(index.saturating_add(2))) else {
        return false;
    };
    relaxable(relocs, index)
        && relaxable(relocs, index.saturating_add(2))
        && hi.offset.checked_add(4) == Some(lo.offset)
}

/// Whether a `pcaddi` at `displace` bytes from its target reaches it.
fn pcaddi_reaches(displace: i64) -> bool {
    displace & 3 == 0 && insn::fits_signed(displace, 22)
}

/// The edits of one section in pass `pass`.
///
/// # Errors
///
/// [`crate::error::Error::Malformed`] for `R_LARCH_ALIGN` padding too small
/// for its alignment.
#[allow(clippy::too_many_lines)]
pub fn decide<F: crate::elf::read::ElfFormat>(
    pass: &Pass<'_, '_, '_, F>,
    section: &SectionInput<'_, '_, F>,
) -> Result<Edits> {
    let mut edits = Edits::default();
    let relocs = &section.relocs;
    // The `pcaddi` a relaxed pair's second half becomes, by position.
    let mut pair: Option<(u32, u32, u32)> = None;
    for (seq, rel) in (0u32..).zip(relocs) {
        let index = seq as usize;
        let loc = section
            .address
            .wrapping_add(rel.offset)
            .wrapping_sub(edits.delta());
        let mut remove = 0u32;
        let mut rewrite = None;
        if let Some((at, word, r_type)) = pair
            && at == seq
        {
            pair = None;
            edits.push(
                section,
                seq,
                rel.offset,
                0,
                Rewrite::Replace {
                    word,
                    len: 4,
                    r_type,
                },
            );
            continue;
        }
        // The kind the writer gives this relocation, so that the two agree.
        let kind = |r_type: u32| {
            let context = pass.classify(section.file, rel.symbol);
            super::classify(r_type, rel.addend, section.data, rel.offset, context)
                .map_or(Kind::None, |class| class.kind)
        };
        let tpoff = || {
            pass.tp
                .and_then(|tp| section.target(pass, seq, false).map(|s| s.wrapping_sub(tp)))
                .map(|value| value as i64)
        };
        match rel.r_type {
            R_LARCH_ALIGN => {
                let Some((all_bytes, trim)) = align_trim(rel.symbol, rel.addend, loc) else {
                    continue;
                };
                let Some(trim) = trim else {
                    return Err(section.malformed(
                        rel.offset,
                        format!(
                            "insufficient padding bytes for R_LARCH_ALIGN: {all_bytes} bytes available"
                        ),
                    ));
                };
                if trim != 0 {
                    remove = u32::try_from(trim).unwrap_or(u32::MAX);
                    rewrite = Some(Rewrite::Align {
                        addend: u32::try_from(all_bytes).unwrap_or(u32::MAX),
                    });
                }
            }
            R_LARCH_PCALA_HI20 | R_LARCH_GOT_PC_HI20
                if pass.relax
                    && pair_relaxable(relocs, index)
                    && kind(rel.r_type | RELAX_HINT) == Kind::Relax =>
            {
                if let Some((word, at)) = fold_pair(pass, section, seq, loc, None) {
                    remove = 4;
                    rewrite = Some(Rewrite::Delete);
                    pair = Some((at, word, R_LARCH_PCREL20_S2));
                }
            }
            // General- and local-dynamic keep their GOT pair, whose address
            // the `pcaddi` computes; local-dynamic uses the general-dynamic
            // relocation, as lld and mold do.
            R_LARCH_TLS_GD_PC_HI20 | R_LARCH_TLS_LD_PC_HI20
                if pass.relax && pair_relaxable(relocs, index) =>
            {
                let slot = pass.got_address(section.file, rel.symbol, GotKind::TlsGd);
                if let Some((word, at)) = fold_pair(pass, section, seq, loc, slot) {
                    remove = 4;
                    rewrite = Some(Rewrite::Delete);
                    pair = Some((at, word, R_LARCH_TLS_GD_PCREL20_S2));
                }
            }
            R_LARCH_TLS_DESC_PC_HI20 => match kind(rel.r_type) {
                // Rewritten to local-exec or initial-exec: the `pcalau12i`
                // and the `addi.d` that computed the descriptor's address
                // become `nop`s, which go away.
                Kind::DescToLe | Kind::DescToIe if pass.relax && relaxable(relocs, index) => {
                    remove = 4;
                    rewrite = Some(Rewrite::Delete);
                }
                _ if pass.relax && pair_relaxable(relocs, index) => {
                    let slot = pass.got_address(section.file, rel.symbol, GotKind::TlsDesc);
                    if let Some((word, at)) = fold_pair(pass, section, seq, loc, slot) {
                        remove = 4;
                        rewrite = Some(Rewrite::Delete);
                        pair = Some((at, word, R_LARCH_TLS_DESC_PCREL20_S2));
                    }
                }
                _ => {}
            },
            R_LARCH_TLS_DESC_PC_LO12
                if pass.relax
                    && relaxable(relocs, index)
                    && matches!(kind(rel.r_type), Kind::DescToLe | Kind::DescToIe) =>
            {
                remove = 4;
                rewrite = Some(Rewrite::Delete);
            }
            // A descriptor rewritten to local-exec loads the offset with
            // one `ori` when it fits 12 bits, so the `lu12i.w` before it is
            // a `nop`.
            R_LARCH_TLS_DESC_LD
                if pass.relax
                    && relaxable(relocs, index)
                    && kind(rel.r_type) == Kind::DescToLe
                    && tpoff().is_some_and(|value| insn::fits_unsigned(value, 12)) =>
            {
                remove = 4;
                rewrite = Some(Rewrite::Delete);
            }
            // The same for an initial-exec access rewritten to local-exec.
            R_LARCH_TLS_IE_PC_HI20
                if pass.relax
                    && relaxable(relocs, index)
                    && kind(rel.r_type) == Kind::IeToLe
                    && tpoff().is_some_and(|value| insn::fits_unsigned(value, 12)) =>
            {
                remove = 4;
                rewrite = Some(Rewrite::Delete);
            }
            R_LARCH_TLS_LE_HI20_R | R_LARCH_TLS_LE_ADD_R
                if pass.relax
                    && relaxable(relocs, index)
                    && tpoff().is_some_and(|value| insn::fits_signed(value, 12)) =>
            {
                remove = 4;
                rewrite = Some(Rewrite::Delete);
            }
            R_LARCH_TLS_LE_LO12_R
                if pass.relax
                    && relaxable(relocs, index)
                    && tpoff().is_some_and(|value| insn::fits_signed(value, 12)) =>
            {
                let word = insn::read_insn(section.data, index_of(rel.offset))
                    .map(|word| insn::with_rj(word, insn::R_TP));
                if let Some(word) = word {
                    rewrite = Some(Rewrite::Replace {
                        word,
                        len: 4,
                        r_type: R_LARCH_TLS_LE_LO12_R,
                    });
                }
            }
            R_LARCH_CALL36 if pass.relax && relaxable(relocs, index) => {
                let link = rel
                    .offset
                    .checked_add(4)
                    .and_then(|at| insn::read_insn(section.data, index_of(at)))
                    .map(insn::rd);
                let op = match link {
                    Some(R_RA) => BL,
                    Some(R_ZERO) => B,
                    _ => continue,
                };
                if let Some(dest) = section.target(pass, seq, true) {
                    let displace = dest.wrapping_sub(loc) as i64;
                    if Field::B26.encode(0, displace).is_ok() {
                        remove = 4;
                        rewrite = Some(Rewrite::Replace {
                            word: op,
                            len: 4,
                            r_type: R_LARCH_B26,
                        });
                    }
                }
            }
            _ => {}
        }
        if let Some(rewrite) = rewrite {
            edits.push(section, seq, rel.offset, remove, rewrite);
        }
    }
    Ok(edits)
}

fn index_of(offset: u64) -> usize {
    usize::try_from(offset).unwrap_or(usize::MAX)
}

/// The `pcaddi` the second half of the pair starting at `seq` becomes, and
/// its position in the relocation table, when the pair folds: the
/// registers agree and `target` (the symbol, or `slot` when the pair
/// computes a GOT entry's address) is within `pcaddi`'s reach of `loc`.
fn fold_pair<F: crate::elf::read::ElfFormat>(
    pass: &Pass<'_, '_, '_, F>,
    section: &SectionInput<'_, '_, F>,
    seq: u32,
    loc: u64,
    slot: Option<u64>,
) -> Option<(u32, u32)> {
    let at = seq.checked_add(2)?;
    section.relocs.get(at as usize)?;
    let dest = match slot {
        Some(slot) => slot,
        None => section.target(pass, seq, false)?,
    };
    let displace = dest.wrapping_sub(loc) as i64;
    if !pcaddi_reaches(displace) {
        return None;
    }
    let offset = section.relocs.get(seq as usize)?.offset;
    let register = super::pair_register(section.data, offset)?;
    Some((insn::ri20(PCADDI, register, 0), at))
}

/// The padding an `R_LARCH_ALIGN` at `loc` reserved, and the bytes of it
/// relaxation removes (lld's rule): everything past the alignment
/// boundary, and all of it when reaching the boundary would emit more than
/// the relocation's maximum. `None` when the relocation says nothing;
/// `Some((all, None))` when the padding is too small for its alignment.
fn align_trim(symbol: u32, addend: i64, loc: u64) -> Option<(u64, Option<u64>)> {
    let addend = u64::try_from(addend).ok()?;
    // Without a symbol the addend is the padding itself (`2^n - 4`); with
    // one it holds the alignment's exponent and, above the low byte, the
    // most padding bytes the alignment may cost.
    let encoded = if symbol == 0 {
        if addend == 0 {
            return None;
        }
        u64::from(addend.ilog2()).checked_add(1)?
    } else {
        addend
    };
    let exponent = encoded & 0xff;
    if !(3..64).contains(&exponent) {
        return None;
    }
    let align = 1u64.checked_shl(u32::try_from(exponent).ok()?)?;
    let all_bytes = align.checked_sub(4)?;
    let max_bytes = encoded >> 8;
    let offset = loc & align.wrapping_sub(1);
    let needed = if offset == 0 {
        0
    } else {
        align.wrapping_sub(offset)
    };
    if max_bytes != 0 && needed > max_bytes {
        return Some((all_bytes, Some(all_bytes)));
    }
    Some((all_bytes, all_bytes.checked_sub(needed)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alignment_padding_is_trimmed_to_the_boundary() {
        // 16-byte alignment, 12 bytes of padding, reached at a multiple of
        // 16: all the padding goes.
        assert_eq!(align_trim(1, 4, 0x1000), Some((12, Some(12))));
        // Four bytes past it: 12 bytes are needed, none go.
        assert_eq!(align_trim(1, 4, 0x1004), Some((12, Some(0))));
        // Eight bytes past it: 8 are needed, 4 go.
        assert_eq!(align_trim(1, 4, 0x1008), Some((12, Some(4))));
        // A maximum of 4 bytes: 12 would be needed, so all of it goes.
        assert_eq!(align_trim(1, 4 | (4 << 8), 0x1004), Some((12, Some(12))));
        // The old form gives the padding itself.
        assert_eq!(align_trim(0, 12, 0x1008), Some((12, Some(4))));
        assert_eq!(align_trim(0, 28, 0x1000), Some((28, Some(28))));
        assert_eq!(align_trim(0, 0, 0x1000), None);
    }

    #[test]
    fn pcaddi_reach_is_twenty_two_signed_bits() {
        assert!(pcaddi_reaches(0));
        assert!(pcaddi_reaches((1 << 21) - 4));
        assert!(!pcaddi_reaches(1 << 21));
        assert!(pcaddi_reaches(-(1 << 21)));
        assert!(!pcaddi_reaches(2));
    }
}

/// What a partial link (`-r`) must know about an input section to keep its
/// alignment through a later relaxing link.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SectionAlign {
    /// The section carries `R_LARCH_RELAX` marks, so the sections after it
    /// can move and need their alignment written down.
    pub relaxes: bool,
    /// An `R_LARCH_ALIGN` at its start already guarantees the alignment.
    pub covered: bool,
}

/// [`SectionAlign`] of a section whose relocations are `relocs` and whose
/// alignment is `align`. Following lld, a weaker `R_LARCH_ALIGN` at offset
/// 0 (from an older assembler) does not count as covering the alignment.
#[must_use]
pub fn section_align<F: crate::elf::read::ElfFormat>(
    relocs: &crate::elf::read::RelaSlice<'_, F>,
    align: u64,
) -> SectionAlign {
    let mut out = SectionAlign::default();
    for rel in relocs.iter() {
        if rel.r_type == R_LARCH_RELAX {
            out.relaxes = true;
        } else if rel.r_type == R_LARCH_ALIGN
            && rel.offset == 0
            && u64::try_from(rel.addend).is_ok_and(|addend| addend >= align.saturating_sub(4))
        {
            out.covered = true;
        }
    }
    out
}
