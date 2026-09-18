//! RISC-V linker relaxation decisions, for the shrinking framework of
//! [`crate::elf::arch::shrink`].
//!
//! The decisions are lld's, so the output matches it:
//!
//! - `R_RISCV_ALIGN` padding is trimmed so that the code after it is
//!   aligned (always, even with `--no-relax`);
//! - a call (`auipc` + `jalr`, `CALL`/`CALL_PLT` with `R_RISCV_RELAX`)
//!   becomes `jal` within ±1 MiB, or `c.j` within ±2 KiB for a tail call
//!   in an object with compressed instructions;
//! - a local-exec access whose offset fits 12 bits loses its `lui` and
//!   `add` and addresses `tp` directly;
//! - a `lui` of an absolute address that fits 12 bits goes away and the
//!   access uses `x0`; with `--relax-gp` (off by default, as in lld; GNU
//!   ld relaxes by default), one within 2 KiB of `__global_pointer$` uses
//!   `gp`;
//! - in an executable, a TLS descriptor sequence loses the instructions its
//!   local-exec or initial-exec form does not need.
//!
//! To guarantee termination, a call's edit may only shrink freely in the
//! first four passes, and only grow back afterwards.

#![deny(clippy::arithmetic_side_effects)]

use crate::arch::riscv::{self as insn, C_J, JAL, TP, fits_signed, hi20};
use crate::elf::layout::{Layout, LayoutInput};
use crate::elf::read::consts::riscv::*;
use crate::elf::refs::Def;
use crate::elf::sections::NONE;
use crate::error::Result;
use crate::symbols::SymbolName;

use super::super::TlsMode;
use super::super::shrink::{Edits, Pass, Rewrite, SectionInput};
use super::EF_RISCV_RVC;

/// Passes after which a call's edit may no longer grow, so that decisions
/// cannot oscillate (lld's limit).
const FREE_PASSES: u32 = 4;

/// The internal relocation type of a `%lo` access relaxed to `x0` (lld's
/// `INTERNAL_R_RISCV_X0REL_*`; the original type tells I from S).
pub const X0REL: u32 = 0x100;
/// The internal relocation type of a `%lo` access relaxed to `gp`.
pub const GPREL: u32 = 0x101;

/// The name of the global pointer symbol.
pub const GLOBAL_POINTER: &[u8] = b"__global_pointer$";

/// The value of `__global_pointer$` in `layout`, when something defines it
/// (lld relaxes against it only then).
#[must_use]
pub fn global_pointer(input: &LayoutInput<'_, '_>, layout: &Layout<'_>) -> Option<u64> {
    let refs = &input.refs;
    let id = refs.symbols.lookup(&SymbolName::new(GLOBAL_POINTER))?;
    match refs.global_target(id, true).def {
        Def::Section {
            file,
            section,
            value,
        } => {
            let sid = refs.sections.id(file, section)?;
            (layout.section_shndx.get(sid.index()).copied().unwrap_or(0) != 0).then(|| {
                layout
                    .section_addr
                    .get(sid.index())
                    .copied()
                    .unwrap_or(0)
                    .wrapping_add(layout.relax.map(sid, value))
            })
        }
        Def::Absolute(value) => Some(value),
        // Linker-defined: by a script, or by qld (0x800 past `.sdata`).
        Def::Linker(_) => match layout
            .script_symbols
            .iter()
            .find(|s| s.name.as_slice() == GLOBAL_POINTER && s.defined)
        {
            Some(symbol) => Some(symbol.value),
            None => Some(
                input
                    .placement
                    .outputs
                    .iter()
                    .position(|o| o.name == b".sdata")
                    .and_then(|i| layout.output_places.get(i))
                    .filter(|place| place.2 != NONE)
                    .map_or(layout.base, |place| place.0)
                    .wrapping_add(0x800),
            ),
        },
        _ => None,
    }
}

/// The edits of one section in pass `pass`.
///
/// # Errors
///
/// [`crate::error::Error::Malformed`] for `R_RISCV_ALIGN` padding too small
/// for its alignment.
#[allow(clippy::too_many_lines)]
pub fn decide(pass: &Pass<'_, '_, '_>, section: &SectionInput<'_, '_>) -> Result<Edits> {
    let mut edits = Edits::default();
    let rvc = section.object.elf.elf().header().e_flags & EF_RISCV_RVC != 0;
    let relocs = &section.relocs;
    // The state of the TLS descriptor sequence being relaxed.
    let mut desc_relax = false;
    let mut desc_exec = false;
    let mut desc_short = false;
    for (seq, rel) in (0u32..).zip(relocs) {
        let relaxable = pass.relax
            && relocs
                .get((seq as usize).saturating_add(1))
                .is_some_and(|n| n.r_type == R_RISCV_RELAX);
        let loc = section
            .address
            .wrapping_add(rel.offset)
            .wrapping_sub(edits.delta());
        let target = |branch: bool| pass.target(section.file, rel.symbol, rel.addend, branch);
        let word = || {
            usize::try_from(rel.offset)
                .ok()
                .and_then(|at| insn::read32(section.data, at))
        };
        let mut remove = 0u32;
        let mut rewrite = None;
        match rel.r_type {
            R_RISCV_ALIGN => {
                let Ok(addend) = u64::try_from(rel.addend) else {
                    continue;
                };
                let align = addend
                    .checked_add(2)
                    .and_then(u64::checked_next_power_of_two)
                    .unwrap_or(u64::MAX);
                let next = loc.wrapping_add(addend);
                let aligned = loc
                    .checked_add(align.wrapping_sub(1))
                    .map_or(loc, |v| v & !align.wrapping_sub(1));
                let Some(trim) = next.checked_sub(aligned) else {
                    return Err(section.malformed(
                        rel.offset,
                        format!(
                            "insufficient padding bytes for R_RISCV_ALIGN: {addend} bytes available for requested alignment of {align} bytes"
                        ),
                    ));
                };
                if trim != 0 {
                    remove = u32::try_from(trim).unwrap_or(u32::MAX);
                    rewrite = Some(Rewrite::Align {
                        addend: u32::try_from(addend).unwrap_or(u32::MAX),
                    });
                }
            }
            R_RISCV_CALL | R_RISCV_CALL_PLT if relaxable => {
                // lld: 6 during the free passes, then at most what the
                // previous pass removed.
                let limit = if pass.pass < FREE_PASSES {
                    6
                } else {
                    pass.previous_edit(section.id, seq).map_or(0, |e| e.remove)
                };
                let rd = rel
                    .offset
                    .checked_add(4)
                    .and_then(|at| usize::try_from(at).ok())
                    .and_then(|at| insn::read32(section.data, at))
                    .map(insn::rd);
                if let (Some(dest), Some(rd)) = (target(true), rd) {
                    let displace = dest.wrapping_sub(loc) as i64;
                    if limit >= 6 && rvc && fits_signed(displace, 12) && rd == insn::X0 {
                        remove = 6;
                        rewrite = Some(Rewrite::Replace {
                            word: u32::from(C_J),
                            len: 2,
                            r_type: R_RISCV_RVC_JUMP,
                        });
                    } else if limit >= 4 && fits_signed(displace, 21) {
                        remove = 4;
                        rewrite = Some(Rewrite::Replace {
                            word: JAL | (rd << 7),
                            len: 4,
                            r_type: R_RISCV_JAL,
                        });
                    }
                }
            }
            R_RISCV_TPREL_HI20 | R_RISCV_TPREL_ADD | R_RISCV_TPREL_LO12_I
            | R_RISCV_TPREL_LO12_S
                if relaxable =>
            {
                let value = pass
                    .tp
                    .and_then(|tp| target(false).map(|s| s.wrapping_sub(tp)));
                if let Some(value) = value
                    && hi20(value) == 0
                {
                    let tp_relative = |insn: u32| {
                        let insn = insn::with_rs1(insn, TP);
                        if rel.r_type == R_RISCV_TPREL_LO12_I {
                            insn::set_lo12_i(insn, value as u32)
                        } else {
                            insn::set_lo12_s(insn, value as u32)
                        }
                    };
                    match rel.r_type {
                        R_RISCV_TPREL_HI20 | R_RISCV_TPREL_ADD => {
                            remove = 4;
                            rewrite = Some(Rewrite::Delete);
                        }
                        _ => {
                            rewrite = word().map(|w| Rewrite::Replace {
                                word: tp_relative(w),
                                len: 4,
                                r_type: 0,
                            });
                        }
                    }
                }
            }
            R_RISCV_HI20 | R_RISCV_LO12_I | R_RISCV_LO12_S if relaxable => {
                let value = target(false);
                let internal = match value {
                    Some(value) if fits_signed(value as i64, 12) => Some(X0REL),
                    Some(value)
                        if pass
                            .gp
                            .is_some_and(|gp| fits_signed(value.wrapping_sub(gp) as i64, 12)) =>
                    {
                        Some(GPREL)
                    }
                    _ => None,
                };
                if let Some(internal) = internal {
                    if rel.r_type == R_RISCV_HI20 {
                        remove = 4;
                        rewrite = Some(Rewrite::Delete);
                    } else {
                        rewrite = Some(Rewrite::Retype(internal));
                    }
                }
            }
            R_RISCV_TLSDESC_HI20 => {
                let mode = pass.tls_mode(section.file, rel.symbol);
                desc_relax = relaxable;
                desc_exec = mode != TlsMode::Dynamic;
                desc_short = relaxable
                    && mode == TlsMode::LocalExec
                    && pass.tp.is_some_and(|tp| {
                        target(false).is_some_and(|s| hi20(s.wrapping_sub(tp)) == 0)
                    });
                if desc_relax && desc_exec {
                    remove = 4;
                    rewrite = Some(Rewrite::Delete);
                }
            }
            R_RISCV_TLSDESC_LOAD_LO12 if desc_relax && desc_exec => {
                remove = 4;
                rewrite = Some(Rewrite::Delete);
            }
            R_RISCV_TLSDESC_ADD_LO12 if desc_short => {
                remove = 4;
                rewrite = Some(Rewrite::Delete);
            }
            _ => {}
        }
        if let Some(rewrite) = rewrite {
            edits.push(section, seq, rel.offset, remove, rewrite);
        }
    }
    Ok(edits)
}

/// Fills trimmed alignment padding with no-ops: `nop`s, then a `c.nop`
/// for a 2-byte remainder.
pub fn fill_nops(out: &mut [u8]) {
    let (words, rest) = out.as_chunks_mut::<4>();
    for word in words {
        *word = insn::NOP.to_le_bytes();
    }
    if let Some(half) = rest.first_chunk_mut::<2>() {
        *half = insn::C_NOP.to_le_bytes();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn padding_is_nops_then_a_compressed_nop() {
        let mut out = [0u8; 6];
        fill_nops(&mut out);
        assert_eq!(out, [0x13, 0, 0, 0, 0x01, 0]);
    }
}
