//! Linker relaxation with section shrinking, for any architecture.
//!
//! RISC-V (and LoongArch) code is assembled for the worst case — a call is
//! `auipc` + `jalr`, a thread-local access `lui` + `add` + the access —
//! with relocations marking the sequences the linker may shorten once
//! addresses are known, and alignment relocations marking `nop` padding the
//! linker must trim so that the code after it is aligned *after* shrinking.
//! Shrinking moves code, which moves symbols, which changes what can be
//! relaxed, so layout iterates ([`layout`]):
//!
//! 1. lay out with the current edits (none at first): each relaxed section
//!    is smaller by the bytes its edits delete ([`Relaxation::removed`]);
//! 2. with those addresses, the architecture decides every relocation's
//!    edit again, section by section in parallel;
//! 3. stop when no section's deletions changed; the layout of step 1 is
//!    then final, with the edits of step 2 (which delete the same bytes).
//!
//! The framework owns everything but the decisions:
//!
//! - the edit plan of each section ([`SectionRelax`]): which relocation's
//!   instruction is deleted, trimmed or replaced ([`Rewrite`]), and how
//!   many bytes go;
//! - address mapping ([`SectionRelax::map`]): symbol values and sizes,
//!   relocation offsets and so DWARF and `.eh_frame` label differences
//!   (`ADD`/`SUB`, `SET`/`SUB_ULEB128`, computed from the moved labels) all
//!   follow the edits, through [`crate::elf::values::Addresses`];
//! - the copy of a section with its edits applied ([`copy`]), which the
//!   writer calls before relocating;
//! - `--emit-relocs` types ([`Relaxation::emitted_type`]).
//!
//! An architecture supplies, per section, the edits of one pass (RISC-V:
//! [`super::riscv::relax::decide`]), and a no-op filler for trimmed
//! alignment. Nothing here runs for other architectures:
//! [`Relaxation`] stays empty, and its lookups return at the first check.

#![deny(clippy::arithmetic_side_effects)]

use std::cell::OnceCell;

use rayon::prelude::*;

use crate::elf::common::Commons;
use crate::elf::export::PREEMPTIBLE;
use crate::elf::inputs::ElfInput;
use crate::elf::layout::{Layout, LayoutInput};
use crate::elf::object::{InputSection, ObjectInput, SectionKind};
use crate::elf::read::consts::{SHF_ALLOC, SHF_EXECINSTR};
use crate::elf::read::{Relocation, Relocations};
use crate::elf::refs::Def;
use crate::elf::reloc::{self, Context};
use crate::elf::synth::Owner;
use crate::elf::values::Addresses;
use crate::error::{Error, Result};
use crate::ids::SectionId;
use crate::symbols::SymbolFlags;

use super::{Arch, TlsMode};

/// How many passes layout may take before giving up on a fixpoint.
pub const MAX_PASSES: u32 = 30;

const NONE: u32 = u32::MAX;

/// Where a relocation's target is, resolved once for all passes.
#[derive(Clone, Copy, Debug)]
enum Place {
    /// Not known before the write (commons, linker-defined and shared
    /// symbols without a PLT entry).
    Unknown,
    /// A fixed address.
    Absolute(u64),
    /// Offset `value` of live regular section `id`, plus `addend`: moves
    /// with the section's relaxation edits.
    Section {
        id: SectionId,
        value: u64,
        addend: i64,
    },
    /// Anywhere else in a section (merged, folded): looked up in full.
    Offset {
        file: usize,
        section: u32,
        value: u64,
        addend: i64,
    },
    /// A PLT entry.
    Plt { owner: Owner, addend: i64 },
    /// An IFUNC stub.
    Iplt { owner: Owner, addend: i64 },
}

/// What relaxation does to the instruction (or padding) of one
/// relocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rewrite {
    /// Alignment padding of `addend` bytes, trimmed by the edit's `remove`
    /// bytes; what stays is refilled with no-ops when the trim splits one.
    Align {
        /// The padding the assembler reserved.
        addend: u32,
    },
    /// The instruction is deleted.
    Delete,
    /// The instruction's first `len` bytes become `word` (2 or 4 bytes);
    /// the edit's `remove` bytes after them go. The writer then applies
    /// relocation type `r_type` there, or nothing when it is 0 (the word is
    /// complete).
    Replace {
        /// The new instruction.
        word: u32,
        /// Its length in bytes.
        len: u8,
        /// The relocation the writer applies to it.
        r_type: u32,
    },
    /// No byte changes; the writer applies relocation type `r_type` (an
    /// architecture-internal number, above 255) instead of the original.
    Retype(u32),
}

impl Rewrite {
    /// The bytes the rewrite keeps at the relocation's offset before the
    /// deleted ones.
    #[must_use]
    pub fn kept(self) -> u64 {
        match self {
            Self::Replace { len, .. } => u64::from(len),
            Self::Align { .. } | Self::Delete | Self::Retype(_) => 0,
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

    /// The edit of relocation `index` of the section's table.
    #[must_use]
    pub fn edit_of_index(&self, index: u32) -> Option<&Edit> {
        if self.sorted {
            self.edit(index)
        } else {
            self.edits.iter().find(|e| e.index == index)
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
    /// The type `--emit-relocs` gives a deleted instruction's relocation.
    deleted_type: u32,
}

impl Relaxation {
    fn new(total: usize, sections: Vec<SectionRelax>, deleted_type: u32) -> Self {
        if sections.is_empty() {
            return Self::default();
        }
        let mut index = vec![NONE; total];
        for (position, section) in sections.iter().enumerate() {
            if let Some(slot) = index.get_mut(section.id.index()) {
                *slot = u32::try_from(position).unwrap_or(NONE);
            }
        }
        Self {
            index,
            sections,
            deleted_type,
        }
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
    /// `id`, as lld writes it: a replaced instruction's new relocation, and
    /// the architecture's marker for a deleted one.
    #[must_use]
    pub fn emitted_type(&self, id: SectionId, index: usize, r_type: u32) -> u32 {
        let edit = u32::try_from(index)
            .ok()
            .and_then(|index| self.section(id)?.edit_of_index(index));
        match edit.map(|e| e.rewrite) {
            Some(Rewrite::Replace { r_type: new, .. }) if new != 0 => new,
            Some(Rewrite::Delete) => self.deleted_type,
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

/// The relocations of a section in processing order, and whether the table
/// was already sorted.
#[must_use]
pub fn ordered(relocs: Vec<Relocation>) -> (Vec<Relocation>, bool) {
    let offsets: Vec<u64> = relocs.iter().map(|r| r.offset).collect();
    match sorted_order(&offsets) {
        None => (relocs, true),
        Some(order) => (
            order
                .iter()
                .filter_map(|&i| relocs.get(i as usize).copied())
                .collect(),
            false,
        ),
    }
}

/// Copies `data` into `out` with the edits of `relax` applied (lld's
/// `finalizeRelax`): deleted bytes are dropped, replaced instructions
/// written, and trimmed alignment padding refilled with `fill` (the
/// architecture's no-ops) when the trim does not remove whole 4-byte
/// no-ops.
pub fn copy(data: &[u8], relax: &SectionRelax, out: &mut [u8], fill: fn(&mut [u8])) {
    let mut from = 0usize;
    let mut to = 0usize;
    let copy = |out: &mut [u8], from: usize, until: usize, to: &mut usize| {
        let len = until.saturating_sub(from);
        if let (Some(src), Some(dest)) = (
            data.get(from..until),
            out.get_mut(*to..to.saturating_add(len)),
        ) {
            dest.copy_from_slice(src);
        }
        *to = to.saturating_add(len);
    };
    for edit in &relax.edits {
        let Ok(at) = usize::try_from(edit.offset) else {
            break;
        };
        if at < from {
            continue;
        }
        copy(out, from, at, &mut to);
        let remove = edit.remove as usize;
        let mut kept = 0usize;
        match edit.rewrite {
            Rewrite::Align { addend } => {
                let addend = addend as usize;
                if !remove.is_multiple_of(4) || !addend.is_multiple_of(4) {
                    kept = addend.saturating_sub(remove);
                    if let Some(padding) = out.get_mut(to..to.saturating_add(kept)) {
                        fill(padding);
                    }
                }
            }
            Rewrite::Replace { word, len, .. } => {
                kept = usize::from(len);
                let bytes = word.to_le_bytes();
                if let (Some(dest), Some(src)) =
                    (out.get_mut(to..to.saturating_add(kept)), bytes.get(..kept))
                {
                    dest.copy_from_slice(src);
                }
            }
            Rewrite::Delete | Rewrite::Retype(_) => {}
        }
        to = to.saturating_add(kept);
        from = at.saturating_add(kept).saturating_add(remove);
    }
    copy(out, from, data.len(), &mut to);
}

/// A section relaxation looks at.
#[derive(Clone, Copy, Debug)]
struct Candidate {
    id: SectionId,
    file: usize,
    section: u32,
}

/// The live code sections with relocations, in section ID order.
fn candidates<F: crate::elf::read::ElfFormat>(input: &LayoutInput<'_, '_, F>) -> Vec<Candidate> {
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
/// Errors from `inner` and from the architecture's decisions (malformed
/// alignment padding), and [`Error::Internal`] when no fixpoint is reached.
pub fn layout<'a, F: crate::elf::read::ElfFormat>(
    input: &LayoutInput<'_, 'a, F>,
    inner: &dyn Fn(&LayoutInput<'_, 'a, F>) -> Result<Layout<'a>>,
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
    // Relocations are read, and their targets resolved, once; each pass
    // only recomputes addresses.
    let prepared: Vec<Result<Option<SectionInput<'_, 'a, F>>>> = candidates
        .par_iter()
        .map(|candidate| prepare(input, *candidate))
        .collect();
    let mut sections = Vec::with_capacity(prepared.len());
    for section in prepared {
        sections.extend(section?);
    }
    for pass in 0..MAX_PASSES {
        let round = LayoutInput {
            relax: Some(&state),
            ..*input
        };
        let mut layout = inner(&round)?;
        layout.relax = state;
        let next = relax_pass(input, &layout, &mut sections, pass)?;
        if next.same_shape(&layout.relax) {
            layout.relax = next;
            return Ok(layout);
        }
        state = next;
    }
    Err(Error::Internal("linker relaxation did not converge".into()))
}

/// What the architecture's decisions can look up in one pass.
pub struct Pass<'p, 'x, 'a, F: crate::elf::read::ElfFormat = crate::elf::read::Elf64Le> {
    /// Addresses with the layout of this pass (symbol values follow the
    /// edits it was laid out with; global symbols are looked up by
    /// definition, so [`Addresses::globals`] is empty).
    pub addresses: Addresses<'x, 'a, F>,
    /// Relocation decision context.
    pub context: Context,
    /// `--relax`: relaxation marks may be acted on. Alignment is honored
    /// either way.
    pub relax: bool,
    /// The thread pointer, when there is a TLS segment.
    pub tp: Option<u64>,
    /// A global pointer register's value the architecture may relax
    /// accesses against (RISC-V `--relax-gp`: `__global_pointer$`).
    pub gp: Option<u64>,
    /// The edits this pass's layout was computed with.
    pub previous: &'p Relaxation,
    /// The pass number, from 0.
    pub pass: u32,
}

impl<F: crate::elf::read::ElfFormat> Pass<'_, '_, '_, F> {
    /// Where relocation target `symbol` of `file` (plus `addend`) is, in a
    /// form that stays valid across passes: through its PLT entry or IFUNC
    /// stub when `branch` says the relocation is a call.
    fn resolve(&self, file: usize, symbol: u32, addend: i64, branch: bool) -> Place {
        let addresses = &self.addresses;
        let refs = &addresses.refs;
        let Some(target) = refs.target(file, symbol as usize) else {
            return Place::Unknown;
        };
        let owner = Addresses::<F>::owner(&target, file, symbol);
        if branch {
            if target.is_ifunc() && addresses.iplt_address(owner).is_some() {
                return Place::Iplt { owner, addend };
            }
            let flags = target
                .global
                .map_or(SymbolFlags::EMPTY, |id| refs.symbols.flags(id));
            if flags.contains(SymbolFlags::NEEDS_PLT | PREEMPTIBLE)
                && addresses.plt_address(owner).is_some()
            {
                return Place::Plt { owner, addend };
            }
        }
        match target.def {
            Def::Section {
                file,
                section,
                value,
            } => {
                let kind = refs.sections.kind_in(file, section);
                let section_symbol = target.is_section_symbol();
                if kind == Some(SectionKind::Merge) && section_symbol {
                    return match value.checked_add_signed(addend) {
                        Some(offset) => Place::Offset {
                            file,
                            section,
                            value: offset,
                            addend: 0,
                        },
                        None => Place::Unknown,
                    };
                }
                if kind == Some(SectionKind::Regular)
                    && let Some(id) = refs.sections.id(file, section)
                    && refs.sections.is_live(id)
                {
                    // A section symbol's offset into relaxed code moves
                    // with the code, as a label does.
                    if section_symbol && let Some(offset) = value.checked_add_signed(addend) {
                        return Place::Section {
                            id,
                            value: offset,
                            addend: 0,
                        };
                    }
                    return Place::Section { id, value, addend };
                }
                Place::Offset {
                    file,
                    section,
                    value,
                    addend,
                }
            }
            Def::Absolute(value) => Place::Absolute(value.wrapping_add_signed(addend)),
            Def::Undefined { weak: true } => Place::Absolute(addend as u64),
            _ => Place::Unknown,
        }
    }

    /// The address of `place` in this pass's layout; `None` when it is not
    /// known yet (commons, linker-defined and shared symbols without a PLT
    /// entry): such relocations are not relaxed.
    fn address(&self, place: Place) -> Option<u64> {
        let addresses = &self.addresses;
        match place {
            Place::Unknown => None,
            Place::Absolute(value) => Some(value),
            Place::Section { id, value, addend } => {
                let layout = addresses.layout;
                if layout.section_shndx.get(id.index()).copied().unwrap_or(0) == 0 {
                    return None;
                }
                Some(
                    layout
                        .section_addr
                        .get(id.index())?
                        .wrapping_add(self.previous.map(id, value))
                        .wrapping_add_signed(addend),
                )
            }
            Place::Offset {
                file,
                section,
                value,
                addend,
            } => Some(
                addresses
                    .section_offset_address(file, section, value)?
                    .wrapping_add_signed(addend),
            ),
            Place::Plt { owner, addend } => {
                Some(addresses.plt_address(owner)?.wrapping_add_signed(addend))
            }
            Place::Iplt { owner, addend } => {
                Some(addresses.iplt_address(owner)?.wrapping_add_signed(addend))
            }
        }
    }

    /// How a TLS access to `symbol` of `file` is linked.
    #[must_use]
    pub fn tls_mode(&self, file: usize, symbol: u32) -> TlsMode {
        let refs = &self.addresses.refs;
        let Some(target) = refs.target(file, symbol as usize) else {
            return TlsMode::Dynamic;
        };
        let flags = target
            .global
            .map_or(SymbolFlags::EMPTY, |id| refs.symbols.flags(id));
        reloc::classify_context(&self.context, &target, flags).tls
    }

    /// The edit the previous pass made for relocation `seq` of section `id`.
    #[must_use]
    pub fn previous_edit(&self, id: SectionId, seq: u32) -> Option<&Edit> {
        self.previous.section(id)?.edit(seq)
    }
}

/// One code section, prepared for the architecture's decisions.
pub struct SectionInput<'s, 'a, F: crate::elf::read::ElfFormat = crate::elf::read::Elf64Le> {
    /// The section.
    pub id: SectionId,
    /// Its file, by index.
    pub file: usize,
    /// The file.
    pub input: &'s ElfInput<'a, F>,
    /// The parsed object.
    pub object: &'s ObjectInput<'a, F>,
    /// The section header.
    pub section: &'s InputSection<'a>,
    /// Its contents.
    pub data: &'a [u8],
    /// Its relocations in processing order.
    pub relocs: Vec<Relocation>,
    /// Their index in the table, by position, when the table was not
    /// sorted.
    order: Option<Vec<u32>>,
    /// Its address in this pass's layout.
    pub address: u64,
    /// The resolved target of each relocation, filled when first asked.
    targets: Vec<OnceCell<Place>>,
}

impl<F: crate::elf::read::ElfFormat> SectionInput<'_, '_, F> {
    /// `S + A` of the relocation at position `seq` in this pass, through
    /// its PLT entry or IFUNC stub when `branch` says it is a call. `None`
    /// when the address is not known (such relocations are not relaxed).
    #[must_use]
    pub fn target(&self, pass: &Pass<'_, '_, '_, F>, seq: u32, branch: bool) -> Option<u64> {
        let rel = self.relocs.get(seq as usize)?;
        let place = *self
            .targets
            .get(seq as usize)?
            .get_or_init(|| pass.resolve(self.file, rel.symbol, rel.addend, branch));
        pass.address(place)
    }

    /// The table index of the relocation at position `seq`.
    #[must_use]
    pub fn index_of(&self, seq: u32) -> u32 {
        self.order
            .as_ref()
            .and_then(|o| o.get(seq as usize).copied())
            .unwrap_or(seq)
    }

    /// An error about malformed input at `offset` of the section.
    #[must_use]
    pub fn malformed(&self, offset: u64, what: String) -> Error {
        Error::Malformed {
            file: self.input.path(),
            member: self.input.member(),
            offset: self.section.header.sh_offset.saturating_add(offset),
            what,
        }
    }
}

/// The edits of a section as the architecture decides them, in processing
/// order.
#[derive(Debug, Default)]
pub struct Edits {
    edits: Vec<Edit>,
    delta: u64,
}

impl Edits {
    /// Bytes deleted so far in this pass: the relocation at `offset` is at
    /// `section address + offset - delta` once they are gone.
    #[must_use]
    pub fn delta(&self) -> u64 {
        self.delta
    }

    /// Records the edit of the relocation at position `seq`.
    pub fn push<F: crate::elf::read::ElfFormat>(
        &mut self,
        section: &SectionInput<'_, '_, F>,
        seq: u32,
        offset: u64,
        remove: u32,
        rewrite: Rewrite,
    ) {
        self.delta = self.delta.saturating_add(u64::from(remove));
        self.edits.push(Edit {
            seq,
            index: section.index_of(seq),
            offset,
            remove,
            delta: self.delta,
            rewrite,
        });
    }
}

/// Decides every candidate's edits against `layout`, whose `relax` holds
/// the edits it was laid out with.
fn relax_pass<'a, F: crate::elf::read::ElfFormat>(
    input: &LayoutInput<'_, 'a, F>,
    layout: &Layout<'a>,
    sections: &mut [SectionInput<'_, 'a, F>],
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
            weak_zero: Context::weak_zero(arch, input.mode),
        },
        relax: input.options.relax,
        tp: layout.tls.map(|tls| tls.tp(arch)),
        gp: match arch {
            Arch::RiscV64 if input.options.relax_gp => {
                super::riscv::relax::global_pointer(input, layout)
            }
            _ => None,
        },
        previous: &layout.relax,
        pass,
    };
    let results: Vec<Result<Option<SectionRelax>>> = sections
        .par_iter_mut()
        .map(|section| relax_section(&context, section))
        .collect();
    let mut sections = Vec::new();
    for result in results {
        if let Some(section) = result? {
            sections.push(section);
        }
    }
    Ok(Relaxation::new(
        input.refs.sections.len(),
        sections,
        deleted_type(arch),
    ))
}

/// Reads candidate section `candidate` for the passes: its relocations
/// in processing order and its contents.
fn prepare<'s, 'a, F: crate::elf::read::ElfFormat>(
    input: &'s LayoutInput<'_, 'a, F>,
    candidate: Candidate,
) -> Result<Option<SectionInput<'s, 'a, F>>> {
    let Some(file) = input.refs.files.get(candidate.file) else {
        return Ok(None);
    };
    let Some(object) = &file.object else {
        return Ok(None);
    };
    let Some(section) = object.section(candidate.section) else {
        return Ok(None);
    };
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
    let relocs: Vec<Relocation> = relas.iter().collect();
    let offsets: Vec<u64> = relocs.iter().map(|r| r.offset).collect();
    let order = sorted_order(&offsets);
    let relocs: Vec<Relocation> = match &order {
        Some(order) => order
            .iter()
            .filter_map(|&i| relocs.get(i as usize).copied())
            .collect(),
        None => relocs,
    };
    let targets = std::iter::repeat_with(OnceCell::new)
        .take(relocs.len())
        .collect();
    Ok(Some(SectionInput {
        id: candidate.id,
        file: candidate.file,
        input: file,
        object,
        section,
        data: object.section_data(section)?,
        relocs,
        order,
        address: 0,
        targets,
    }))
}

fn relax_section<F: crate::elf::read::ElfFormat>(
    pass: &Pass<'_, '_, '_, F>,
    section: &mut SectionInput<'_, '_, F>,
) -> Result<Option<SectionRelax>> {
    let layout = pass.addresses.layout;
    let index = section.id.index();
    if layout.section_shndx.get(index).copied().unwrap_or(0) == 0 {
        return Ok(None);
    }
    section.address = layout.section_addr.get(index).copied().unwrap_or(0);
    let edits = decide(pass, section)?;
    if edits.edits.is_empty() {
        return Ok(None);
    }
    Ok(Some(SectionRelax {
        id: section.id,
        edits: edits.edits,
        sorted: section.order.is_none(),
    }))
}

/// Whether `arch` relaxes with section shrinking, so layout goes through
/// [`layout`].
#[must_use]
pub fn applies(arch: Arch) -> bool {
    arch == Arch::RiscV64
}

/// The architecture's edits of one section.
fn decide<F: crate::elf::read::ElfFormat>(
    pass: &Pass<'_, '_, '_, F>,
    section: &SectionInput<'_, '_, F>,
) -> Result<Edits> {
    match pass.context.arch {
        Arch::RiscV64 => super::riscv::relax::decide(pass, section),
        _ => Ok(Edits::default()),
    }
}

/// The type `--emit-relocs` gives the relocation of a deleted instruction.
fn deleted_type(arch: Arch) -> u32 {
    match arch {
        Arch::RiscV64 => crate::elf::read::consts::riscv::R_RISCV_RELAX,
        _ => 0,
    }
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

    fn jump(len: u8) -> Rewrite {
        Rewrite::Replace {
            word: 0x6f,
            len,
            r_type: 17,
        }
    }

    #[test]
    fn offsets_move_by_the_bytes_deleted_before_them() {
        let section = SectionRelax {
            id: SectionId::new(0),
            sorted: true,
            edits: vec![
                edit(0, 0x10, 4, 4, jump(4)),
                edit(3, 0x20, 0, 4, Rewrite::Retype(0x100)),
                edit(5, 0x30, 6, 10, jump(2)),
            ],
        };
        assert_eq!(section.map(0x8), 0x8);
        assert_eq!(section.map(0x10), 0x10, "a relaxed call keeps its start");
        assert_eq!(section.map(0x18), 0x14);
        assert_eq!(section.map(0x30), 0x2c);
        assert_eq!(section.map(0x38), 0x2e);
        assert_eq!(section.removed(), 10);
        assert_eq!(
            section.edit(3).map(|e| e.rewrite),
            Some(Rewrite::Retype(0x100))
        );
        assert_eq!(section.edit(4), None);
        let state = Relaxation::new(4, vec![section], 51);
        assert_eq!(state.map(SectionId::new(0), 0x38), 0x2e);
        assert_eq!(state.map(SectionId::new(1), 0x38), 0x38);
        assert_eq!(state.symbol_size(SectionId::new(0), 0x10, 0x28), 0x1e);
        assert_eq!(state.emitted_type(SectionId::new(0), 0, 19), 17);
        assert_eq!(state.emitted_type(SectionId::new(0), 1, 19), 19);
        assert_eq!(Relaxation::default().removed(SectionId::new(0)), 0);
    }

    #[test]
    fn order_is_by_offset() {
        assert_eq!(sorted_order(&[0, 4, 4, 8]), None);
        assert_eq!(sorted_order(&[8, 0, 4]), Some(vec![1, 2, 0]));
    }

    #[test]
    fn copy_drops_and_rewrites_bytes() {
        // A call (8 bytes) at 0, then 6 bytes of alignment padding at 8
        // (4-byte and 2-byte no-ops), then an instruction at 14.
        let mut data = Vec::new();
        data.extend_from_slice(&0x0000_0097u32.to_le_bytes());
        data.extend_from_slice(&0x0000_80e7u32.to_le_bytes());
        data.extend_from_slice(&[0x13, 0, 0, 0, 1, 0]);
        data.extend_from_slice(&0x1234_5678u32.to_le_bytes());
        let relax = SectionRelax {
            id: SectionId::new(0),
            sorted: true,
            edits: vec![
                edit(0, 0, 4, 4, jump(4)),
                // At 4 once the call shrank, 8-byte alignment needs 4 bytes
                // of padding: 2 of the 6 go, and what stays is refilled.
                edit(2, 8, 2, 6, Rewrite::Align { addend: 6 }),
            ],
        };
        let mut out = vec![0xaa; data.len() - 6];
        copy(&data, &relax, &mut out, |p| p.fill(0xee));
        let mut expect = Vec::new();
        expect.extend_from_slice(&0x6fu32.to_le_bytes());
        expect.extend_from_slice(&[0xee; 4]);
        expect.extend_from_slice(&0x1234_5678u32.to_le_bytes());
        assert_eq!(out, expect);
    }
}
