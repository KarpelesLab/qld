//! Linker relaxation with section shrinking.
//!
//! RISC-V code is assembled for the worst case — a call is `auipc` + `jalr`,
//! a thread-local access `lui` + `add` + the access — with `R_RISCV_RELAX`
//! marking the sequences the linker may shorten once addresses are known,
//! and `R_RISCV_ALIGN` marking `nop` padding the linker must trim so that
//! the code after it is aligned *after* shrinking. Shrinking moves code,
//! which moves symbols, which changes what can be relaxed, so layout
//! iterates ([`layout`]):
//!
//! 1. lay out with the current edits (none at first): each relaxed section
//!    is smaller by the bytes its edits delete ([`Relaxation::removed`]);
//! 2. with those addresses, decide every relocation's edit again, section by
//!    section in parallel (lld's `relax`);
//! 3. stop when no section's deletions changed; the layout of step 1 is
//!    then final, with the edits of step 2 (which delete the same bytes).
//!
//! The decisions are lld's, so the output matches it: calls become `jal`
//! (±1 MiB) or, for tail calls in objects with compressed instructions,
//! `c.j` (±2 KiB); a local-exec access whose offset fits 12 bits loses its
//! `lui` and `add` and addresses `tp` directly; a `lui` of an address that
//! fits 12 bits goes away and the access uses `x0`; TLS descriptors in an
//! executable lose the instructions their local-exec or initial-exec form
//! does not need. To guarantee termination, a call's edit may only shrink
//! freely in the first four passes, and only grow back afterwards.
//!
//! Every address in a relaxed section follows the edits through
//! [`SectionRelax::map`]: symbol values and sizes, relocation offsets, and
//! so DWARF and `.eh_frame`, whose label differences (`ADD`/`SUB`/`SET` and
//! `ULEB128` pairs) are computed from the moved labels. The writer applies
//! the edits while copying the section ([`super::apply`]).
//!
//! Nothing here runs for other architectures: [`Relaxation`] stays empty,
//! and its lookups return at the first check.

#![deny(clippy::arithmetic_side_effects)]

use rayon::prelude::*;

use crate::arch::riscv::{self as insn, C_J, JAL, TP, fits_signed, hi20};
use crate::elf::common::Commons;
use crate::elf::export::PREEMPTIBLE;
use crate::elf::layout::{Layout, LayoutInput};
use crate::elf::object::SectionKind;
use crate::elf::read::Relocations;
use crate::elf::read::consts::riscv::*;
use crate::elf::read::consts::{SHF_ALLOC, SHF_EXECINSTR};
use crate::elf::refs::Def;
use crate::elf::reloc::{self, Context};
use crate::elf::values::Addresses;
use crate::error::{Error, Result};
use crate::ids::SectionId;
use crate::symbols::SymbolFlags;

use super::super::{Arch, TlsMode};
use super::EF_RISCV_RVC;

/// Passes after which a call's edit may no longer grow, so that decisions
/// cannot oscillate (lld's limit).
const FREE_PASSES: u32 = 4;
/// How many passes layout may take before giving up on a fixpoint.
pub const MAX_PASSES: u32 = 30;

const NONE: u32 = u32::MAX;

/// How relaxation rewrote the instruction of one relocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rewrite {
    /// `R_RISCV_ALIGN` padding, trimmed by the edit's `remove` bytes.
    Align,
    /// The instruction is deleted.
    Delete,
    /// An `auipc` + `jalr` became this `jal`, whose offset the writer fills.
    Jal(u32),
    /// An `auipc` + `jalr` became this `c.j`, whose offset the writer fills.
    CJump(u16),
    /// A `%lo` access now addresses `x0`: the whole value is its offset.
    X0Rel,
    /// A local-exec access became this complete `tp`-relative instruction.
    Replace(u32),
}

impl Rewrite {
    /// The bytes of the instruction the rewrite keeps, before the deleted
    /// ones.
    #[must_use]
    pub fn kept(self) -> u64 {
        match self {
            Self::Align | Self::Delete | Self::X0Rel => 0,
            Self::Jal(_) | Self::Replace(_) => 4,
            Self::CJump(_) => 2,
        }
    }
}

/// One relaxed relocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Edit {
    /// Position of the relocation in processing order ([`sorted_order`]).
    pub seq: u32,
    /// Its index in the relocation section.
    pub index: u32,
    /// Its offset in the input section.
    pub offset: u64,
    /// Bytes deleted after the kept part of the instruction.
    pub remove: u32,
    /// Bytes deleted in the section up to and including this edit.
    pub delta: u64,
    /// What becomes of the instruction.
    pub rewrite: Rewrite,
}

/// The edits of one input section.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SectionRelax {
    /// The section.
    pub id: SectionId,
    /// Its edits, in processing order.
    pub edits: Vec<Edit>,
    /// The relocation table was sorted by offset, so processing order is
    /// table order.
    pub sorted: bool,
}

impl SectionRelax {
    /// Bytes the edits delete.
    #[must_use]
    pub fn removed(&self) -> u64 {
        self.edits.last().map_or(0, |e| e.delta)
    }

    /// Where offset `offset` of the input section ends up: minus the bytes
    /// deleted before it. An offset at a relaxed instruction stays at its
    /// start, as lld moves symbols.
    #[must_use]
    pub fn map(&self, offset: u64) -> u64 {
        let before = self.edits.partition_point(|e| e.offset < offset);
        let deleted = before
            .checked_sub(1)
            .and_then(|i| self.edits.get(i))
            .map_or(0, |e| e.delta);
        offset.saturating_sub(deleted)
    }

    /// The edit of the relocation at position `seq`, if relaxed.
    #[must_use]
    pub fn edit(&self, seq: u32) -> Option<&Edit> {
        let at = self.edits.binary_search_by_key(&seq, |e| e.seq).ok()?;
        self.edits.get(at)
    }

    /// The type `--emit-relocs` writes for relocation `index` of the
    /// section's table, as lld writes it: a relaxed call becomes the jump it
    /// is now, a deleted instruction's relocation `R_RISCV_RELAX`.
    #[must_use]
    pub fn emitted_type(&self, index: u32, r_type: u32) -> u32 {
        let edit = if self.sorted {
            self.edit(index)
        } else {
            self.edits.iter().find(|e| e.index == index)
        };
        match edit.map(|e| e.rewrite) {
            Some(Rewrite::Jal(_)) => R_RISCV_JAL,
            Some(Rewrite::CJump(_)) => R_RISCV_RVC_JUMP,
            Some(Rewrite::Delete) => R_RISCV_RELAX,
            _ => r_type,
        }
    }

    /// The deletions, which decide the layout.
    fn shape(&self) -> impl Iterator<Item = (u32, u64)> + '_ {
        self.edits
            .iter()
            .filter(|e| e.remove != 0)
            .map(|e| (e.seq, e.delta))
    }
}

/// The relaxation state of a link: the edits of every relaxed section.
#[derive(Clone, Debug, Default)]
pub struct Relaxation {
    /// Position in `sections` by section ID, or `NONE`; empty when nothing
    /// is relaxed.
    index: Vec<u32>,
    /// The sections with edits, by ID.
    sections: Vec<SectionRelax>,
}

impl Relaxation {
    fn new(total: usize, sections: Vec<SectionRelax>) -> Self {
        if sections.is_empty() {
            return Self::default();
        }
        let mut index = vec![NONE; total];
        for (position, section) in sections.iter().enumerate() {
            if let Some(slot) = index.get_mut(section.id.index()) {
                *slot = u32::try_from(position).unwrap_or(NONE);
            }
        }
        Self { index, sections }
    }

    /// Whether no section is relaxed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sections.is_empty()
    }

    /// The edits of section `id`, if it has any.
    #[must_use]
    pub fn section(&self, id: SectionId) -> Option<&SectionRelax> {
        if self.index.is_empty() {
            return None;
        }
        let position = *self.index.get(id.index())?;
        self.sections.get(position as usize)
    }

    /// Where offset `offset` of section `id` ends up.
    #[inline]
    #[must_use]
    pub fn map(&self, id: SectionId, offset: u64) -> u64 {
        match self.section(id) {
            Some(section) => section.map(offset),
            None => offset,
        }
    }

    /// The type `--emit-relocs` writes for relocation `index` of section
    /// `id` ([`SectionRelax::emitted_type`]).
    #[must_use]
    pub fn emitted_type(&self, id: SectionId, index: usize, r_type: u32) -> u32 {
        match (self.section(id), u32::try_from(index)) {
            (Some(section), Ok(index)) => section.emitted_type(index, r_type),
            _ => r_type,
        }
    }

    /// Bytes relaxation deletes from section `id`.
    #[inline]
    #[must_use]
    pub fn removed(&self, id: SectionId) -> u64 {
        self.section(id).map_or(0, SectionRelax::removed)
    }

    /// The size of a symbol at `value` with size `size` in section `id`.
    #[must_use]
    pub fn symbol_size(&self, id: SectionId, value: u64, size: u64) -> u64 {
        match self.section(id) {
            Some(section) => section
                .map(value.saturating_add(size))
                .saturating_sub(section.map(value)),
            None => size,
        }
    }

    fn same_shape(&self, other: &Self) -> bool {
        self.sections.len() == other.sections.len()
            && self
                .sections
                .iter()
                .zip(&other.sections)
                .all(|(a, b)| a.id == b.id && a.shape().eq(b.shape()))
    }
}

/// The processing order of a relocation table: by offset, stably. `None`
/// when the table is already in that order, as assemblers write it.
#[must_use]
pub fn sorted_order(offsets: &[u64]) -> Option<Vec<u32>> {
    if offsets.is_sorted() {
        return None;
    }
    let mut order: Vec<u32> = (0..u32::try_from(offsets.len()).unwrap_or(u32::MAX)).collect();
    order.sort_by_key(|&i| offsets.get(i as usize).copied().unwrap_or(u64::MAX));
    Some(order)
}

/// A section relaxation looks at.
#[derive(Clone, Copy, Debug)]
struct Candidate {
    id: SectionId,
    file: usize,
    section: u32,
}

/// The live code sections with relocations, in section ID order.
fn candidates(input: &LayoutInput<'_, '_>) -> Vec<Candidate> {
    let refs = &input.refs;
    let per_file: Vec<Vec<Candidate>> = refs
        .files
        .par_iter()
        .enumerate()
        .map(|(file, input_file)| {
            let Some(object) = &input_file.object else {
                return Vec::new();
            };
            let mut found = Vec::new();
            for (index, section) in object.sections.iter().enumerate() {
                let Ok(index) = u32::try_from(index) else {
                    break;
                };
                if section.relocs == 0
                    || section.header.sh_flags & (SHF_ALLOC | SHF_EXECINSTR)
                        != (SHF_ALLOC | SHF_EXECINSTR)
                    || !matches!(section.kind, SectionKind::Regular)
                    || section.is_nobits()
                    || !refs.sections.is_live_in(file, index)
                {
                    continue;
                }
                if let Some(id) = refs.sections.id(file, index) {
                    found.push(Candidate {
                        id,
                        file,
                        section: index,
                    });
                }
            }
            found
        })
        .collect();
    per_file.into_iter().flatten().collect()
}

/// Lays out with relaxation: repeats `inner` until the edits stop changing.
///
/// # Errors
///
/// Errors from `inner`, [`Error::Malformed`] for `R_RISCV_ALIGN` padding
/// too small for its alignment, and [`Error::Internal`] when no fixpoint is
/// reached.
pub fn layout<'a>(
    input: &LayoutInput<'_, 'a>,
    inner: &dyn Fn(&LayoutInput<'_, 'a>) -> Result<Layout<'a>>,
) -> Result<Layout<'a>> {
    let candidates = candidates(input);
    let mut state = Relaxation::default();
    if candidates.is_empty() {
        let round = LayoutInput {
            relax: Some(&state),
            ..*input
        };
        return inner(&round);
    }
    for pass in 0..MAX_PASSES {
        let round = LayoutInput {
            relax: Some(&state),
            ..*input
        };
        let mut layout = inner(&round)?;
        layout.relax = state;
        let next = relax_pass(input, &layout, &candidates, pass)?;
        if next.same_shape(&layout.relax) {
            layout.relax = next;
            return Ok(layout);
        }
        state = next;
    }
    Err(Error::Internal("RISC-V relaxation did not converge".into()))
}

/// What one pass needs.
struct Pass<'p, 'x, 'a> {
    addresses: Addresses<'x, 'a>,
    context: Context,
    relax: bool,
    tp: Option<u64>,
    previous: &'p Relaxation,
    pass: u32,
}

/// Decides every candidate's edits against `layout`, whose `relax` holds
/// the edits it was laid out with.
fn relax_pass<'a>(
    input: &LayoutInput<'_, 'a>,
    layout: &Layout<'a>,
    candidates: &[Candidate],
    pass: u32,
) -> Result<Relaxation> {
    let commons = Commons::default();
    let arch = input.synth.arch;
    let context = Pass {
        addresses: Addresses {
            refs: input.refs,
            layout,
            merged: input.merged,
            eh_frames: input.eh_frames,
            synth: input.synth,
            commons: &commons,
            globals: Vec::new(),
        },
        context: Context {
            mode: input.mode,
            relax: input.options.relax,
            copy_relocs: input.options.copy_relocs,
            arch,
        },
        relax: input.options.relax,
        tp: layout.tls.map(|tls| tls.tp(arch)),
        previous: &layout.relax,
        pass,
    };
    let results: Vec<Result<Option<SectionRelax>>> = candidates
        .par_iter()
        .map(|candidate| relax_section(&context, *candidate))
        .collect();
    let mut sections = Vec::new();
    for result in results {
        if let Some(section) = result? {
            sections.push(section);
        }
    }
    Ok(Relaxation::new(input.refs.sections.len(), sections))
}

fn malformed(file: &crate::elf::inputs::ElfInput<'_>, offset: u64, what: String) -> Error {
    Error::Malformed {
        file: file.path(),
        member: file.member(),
        offset,
        what,
    }
}

impl Pass<'_, '_, '_> {
    /// `S + A` of relocation target `symbol` of `file`, through its PLT
    /// entry or IFUNC stub when `branch` says the relocation is a call.
    /// `None` when the address is not known yet (commons, linker-defined
    /// and shared symbols without a PLT entry): such relocations are not
    /// relaxed.
    fn target(&self, file: usize, symbol: u32, addend: i64, branch: bool) -> Option<u64> {
        let addresses = &self.addresses;
        let refs = &addresses.refs;
        let target = refs.target(file, symbol as usize)?;
        let owner = Addresses::owner(&target, file, symbol);
        if branch {
            if target.is_ifunc()
                && let Some(stub) = addresses.iplt_address(owner)
            {
                return Some(stub.wrapping_add_signed(addend));
            }
            let flags = target
                .global
                .map_or(SymbolFlags::EMPTY, |id| refs.symbols.flags(id));
            if flags.contains(SymbolFlags::NEEDS_PLT | PREEMPTIBLE)
                && let Some(plt) = addresses.plt_address(owner)
            {
                return Some(plt.wrapping_add_signed(addend));
            }
        }
        match target.def {
            Def::Section {
                file,
                section,
                value,
            } => {
                let merge = refs.sections.kind_in(file, section) == Some(SectionKind::Merge);
                if merge && target.is_section_symbol() {
                    return addresses.section_offset_address(
                        file,
                        section,
                        value.checked_add_signed(addend)?,
                    );
                }
                Some(
                    addresses
                        .section_offset_address(file, section, value)?
                        .wrapping_add_signed(addend),
                )
            }
            Def::Absolute(value) => Some(value.wrapping_add_signed(addend)),
            Def::Undefined { weak: true } => Some(addend as u64),
            _ => None,
        }
    }

    /// How a TLS access to `symbol` of `file` is linked.
    fn tls_mode(&self, file: usize, symbol: u32) -> TlsMode {
        let refs = &self.addresses.refs;
        let Some(target) = refs.target(file, symbol as usize) else {
            return TlsMode::Dynamic;
        };
        let flags = target
            .global
            .map_or(SymbolFlags::EMPTY, |id| refs.symbols.flags(id));
        reloc::classify_context(&self.context, &target, flags).tls
    }

    /// The edits of one section a call's edit may reach at most (lld: 6
    /// during the free passes, then what the previous pass removed).
    fn call_limit(&self, id: SectionId, seq: u32) -> u32 {
        if self.pass < FREE_PASSES {
            return 6;
        }
        self.previous
            .section(id)
            .and_then(|s| s.edit(seq))
            .map_or(0, |e| e.remove)
    }
}

#[allow(clippy::too_many_lines)]
fn relax_section(pass: &Pass<'_, '_, '_>, candidate: Candidate) -> Result<Option<SectionRelax>> {
    let addresses = &pass.addresses;
    let layout = addresses.layout;
    let refs = &addresses.refs;
    let Some(input_file) = refs.files.get(candidate.file) else {
        return Ok(None);
    };
    let Some(object) = &input_file.object else {
        return Ok(None);
    };
    let Some(section) = object.section(candidate.section) else {
        return Ok(None);
    };
    if layout
        .section_shndx
        .get(candidate.id.index())
        .copied()
        .unwrap_or(0)
        == 0
    {
        return Ok(None);
    }
    let sec_addr = layout
        .section_addr
        .get(candidate.id.index())
        .copied()
        .unwrap_or(0);
    let data = object.section_data(section)?;
    let Some(relocations) = object
        .section(section.relocs)
        .map(|r| object.elf.relocation_section(section.relocs, &r.header))
        .transpose()?
        .flatten()
    else {
        return Ok(None);
    };
    let Relocations::Rela(relas) = relocations.relocations else {
        return Ok(None);
    };
    let rvc = object.elf.elf().header().e_flags & EF_RISCV_RVC != 0;
    let offsets: Vec<u64> = relas.iter().map(|r| r.offset).collect();
    let order = sorted_order(&offsets);
    let count = u32::try_from(relas.len()).unwrap_or(u32::MAX);
    let index_at = |seq: u32| -> usize {
        order
            .as_ref()
            .and_then(|o| o.get(seq as usize).copied())
            .unwrap_or(seq) as usize
    };
    let mut edits = Vec::new();
    let mut delta = 0u64;
    // The state of the TLS descriptor sequence being relaxed.
    let mut desc_relax = false;
    let mut desc_exec = false;
    let mut desc_short = false;
    for seq in 0..count {
        let Some(rel) = relas.get(index_at(seq)) else {
            continue;
        };
        let relaxable = pass.relax
            && seq
                .checked_add(1)
                .filter(|&n| n < count)
                .and_then(|n| relas.get(index_at(n)))
                .is_some_and(|n| n.r_type == R_RISCV_RELAX);
        let loc = sec_addr.wrapping_add(rel.offset).wrapping_sub(delta);
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
                    return Err(malformed(
                        input_file,
                        section.header.sh_offset.saturating_add(rel.offset),
                        format!(
                            "insufficient padding bytes for R_RISCV_ALIGN: {addend} bytes available for requested alignment of {align} bytes"
                        ),
                    ));
                };
                if trim != 0 {
                    remove = u32::try_from(trim).unwrap_or(u32::MAX);
                    rewrite = Some(Rewrite::Align);
                }
            }
            R_RISCV_CALL | R_RISCV_CALL_PLT if relaxable => {
                let limit = pass.call_limit(candidate.id, seq);
                let rd = rel
                    .offset
                    .checked_add(4)
                    .and_then(|at| usize::try_from(at).ok())
                    .and_then(|at| insn::read32(data, at))
                    .map(insn::rd);
                if let (Some(dest), Some(rd)) = (
                    pass.target(candidate.file, rel.symbol, rel.addend, true),
                    rd,
                ) {
                    let displace = dest.wrapping_sub(loc) as i64;
                    if limit >= 6 && rvc && fits_signed(displace, 12) && rd == insn::X0 {
                        remove = 6;
                        rewrite = Some(Rewrite::CJump(C_J));
                    } else if limit >= 4 && fits_signed(displace, 21) {
                        remove = 4;
                        rewrite = Some(Rewrite::Jal(JAL | (rd << 7)));
                    }
                }
            }
            R_RISCV_TPREL_HI20 | R_RISCV_TPREL_ADD | R_RISCV_TPREL_LO12_I
            | R_RISCV_TPREL_LO12_S
                if relaxable =>
            {
                let value = pass.tp.and_then(|tp| {
                    pass.target(candidate.file, rel.symbol, rel.addend, false)
                        .map(|s| s.wrapping_sub(tp))
                });
                if let Some(value) = value
                    && hi20(value) == 0
                {
                    let word = usize::try_from(rel.offset)
                        .ok()
                        .and_then(|at| insn::read32(data, at));
                    match rel.r_type {
                        R_RISCV_TPREL_HI20 | R_RISCV_TPREL_ADD => {
                            remove = 4;
                            rewrite = Some(Rewrite::Delete);
                        }
                        R_RISCV_TPREL_LO12_I => {
                            rewrite = word.map(|w| {
                                Rewrite::Replace(insn::set_lo12_i(
                                    insn::with_rs1(w, TP),
                                    value as u32,
                                ))
                            });
                        }
                        _ => {
                            rewrite = word.map(|w| {
                                Rewrite::Replace(insn::set_lo12_s(
                                    insn::with_rs1(w, TP),
                                    value as u32,
                                ))
                            });
                        }
                    }
                }
            }
            R_RISCV_HI20 | R_RISCV_LO12_I | R_RISCV_LO12_S if relaxable => {
                if let Some(value) = pass.target(candidate.file, rel.symbol, rel.addend, false)
                    && fits_signed(value as i64, 12)
                {
                    if rel.r_type == R_RISCV_HI20 {
                        remove = 4;
                        rewrite = Some(Rewrite::Delete);
                    } else {
                        rewrite = Some(Rewrite::X0Rel);
                    }
                }
            }
            R_RISCV_TLSDESC_HI20 => {
                let mode = pass.tls_mode(candidate.file, rel.symbol);
                desc_relax = relaxable;
                desc_exec = mode != TlsMode::Dynamic;
                desc_short = relaxable
                    && mode == TlsMode::LocalExec
                    && pass.tp.is_some_and(|tp| {
                        pass.target(candidate.file, rel.symbol, rel.addend, false)
                            .is_some_and(|s| hi20(s.wrapping_sub(tp)) == 0)
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
            delta = delta.saturating_add(u64::from(remove));
            edits.push(Edit {
                seq,
                index: u32::try_from(index_at(seq)).unwrap_or(u32::MAX),
                offset: rel.offset,
                remove,
                delta,
                rewrite,
            });
        }
    }
    if edits.is_empty() {
        return Ok(None);
    }
    Ok(Some(SectionRelax {
        id: candidate.id,
        edits,
        sorted: order.is_none(),
    }))
}

/// Whether `arch` relaxes with section shrinking, so layout goes through
/// [`layout`].
#[must_use]
pub fn applies(arch: Arch) -> bool {
    arch == Arch::RiscV64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(seq: u32, offset: u64, remove: u32, delta: u64, rewrite: Rewrite) -> Edit {
        Edit {
            seq,
            index: seq,
            offset,
            remove,
            delta,
            rewrite,
        }
    }

    #[test]
    fn offsets_move_by_the_bytes_deleted_before_them() {
        let section = SectionRelax {
            id: SectionId::new(0),
            sorted: true,
            edits: vec![
                edit(0, 0x10, 4, 4, Rewrite::Jal(JAL)),
                edit(3, 0x20, 0, 4, Rewrite::X0Rel),
                edit(5, 0x30, 6, 10, Rewrite::CJump(C_J)),
            ],
        };
        assert_eq!(section.map(0x8), 0x8);
        assert_eq!(section.map(0x10), 0x10, "a relaxed call keeps its start");
        assert_eq!(section.map(0x18), 0x14);
        assert_eq!(section.map(0x30), 0x2c);
        assert_eq!(section.map(0x38), 0x2e);
        assert_eq!(section.removed(), 10);
        assert_eq!(section.edit(3).map(|e| e.rewrite), Some(Rewrite::X0Rel));
        assert_eq!(section.edit(4), None);
        let state = Relaxation::new(4, vec![section]);
        assert_eq!(state.map(SectionId::new(0), 0x38), 0x2e);
        assert_eq!(state.map(SectionId::new(1), 0x38), 0x38);
        assert_eq!(state.symbol_size(SectionId::new(0), 0x10, 0x28), 0x1e);
        assert_eq!(Relaxation::default().removed(SectionId::new(0)), 0);
    }

    #[test]
    fn order_is_by_offset() {
        assert_eq!(sorted_order(&[0, 4, 4, 8]), None);
        assert_eq!(sorted_order(&[8, 0, 4]), Some(vec![1, 2, 0]));
    }
}
