//! Symbol values after layout.
//!
//! [`Addresses`] answers "where did offset `x` of input section `s` end up?"
//! for every kind of input section (regular, merged, `.eh_frame`), and holds
//! the address of every global symbol, computed once in parallel.

#![deny(clippy::arithmetic_side_effects)]

use rayon::prelude::*;

use crate::args::LinkOptions;
use crate::ids::{SectionId, SymbolId};

use super::common::Commons;
use super::defined::{LinkerSymbols, Value, defsym_expr};
use super::ehframe::EhFrames;
use super::inputs::DefsymExpr;
use super::layout::Layout;
use super::merge::Merged;
use super::object::SectionKind;
use super::place::Placement;
use super::refs::{Def, Refs, Target};
use super::rules::Synthetic;
use super::synth::{GotKind, Owner, Synth};

/// Everything needed to compute addresses.
pub struct Addresses<'x, 'a> {
    /// Relocation target resolution.
    pub refs: Refs<'x, 'a>,
    /// The layout.
    pub layout: &'x Layout<'a>,
    /// Merged sections.
    pub merged: &'x Merged<'x, 'a>,
    /// `.eh_frame` sections.
    pub eh_frames: &'x EhFrames<'a>,
    /// Synthetic sections.
    pub synth: &'x Synth,
    /// Common symbols.
    pub commons: &'x Commons,
    /// The address of every global symbol, by ID.
    pub globals: Vec<u64>,
}

impl<'x, 'a> Addresses<'x, 'a> {
    /// Computes global symbol addresses.
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        refs: Refs<'x, 'a>,
        layout: &'x Layout<'a>,
        merged: &'x Merged<'x, 'a>,
        eh_frames: &'x EhFrames<'a>,
        synth: &'x Synth,
        commons: &'x Commons,
        placement: &Placement<'_>,
        linker: &LinkerSymbols,
        options: &LinkOptions,
    ) -> Self {
        let mut this = Self {
            refs,
            layout,
            merged,
            eh_frames,
            synth,
            commons,
            globals: Vec::new(),
        };
        let values: Vec<u64> = (0..refs.symbols.len())
            .into_par_iter()
            .map(|index| {
                let target = refs.global_target(SymbolId::new(index), true);
                match target.def {
                    Def::Section {
                        file,
                        section,
                        value,
                    } => this
                        .section_offset_address(file, section, value)
                        .unwrap_or(0),
                    Def::Absolute(value) => value,
                    Def::Common(id) => this.common_address(id),
                    Def::Shared(id) => this.shared_address(id),
                    Def::Linker(_) | Def::Undefined { .. } => 0,
                }
            })
            .collect();
        this.globals = values;

        // Linker-defined symbols, defsyms last (they may refer to others).
        let mut defsyms = Vec::new();
        for &(id, value) in &linker.entries {
            let address = match value {
                Value::Defsym(index) => {
                    defsyms.push((id, index));
                    continue;
                }
                other => this.linker_value(other, placement),
            };
            if let Some(slot) = this.globals.get_mut(id.index()) {
                *slot = address;
            }
        }
        for (id, index) in defsyms {
            let address = match defsym_expr(options, index) {
                Some(DefsymExpr::Absolute(value)) => value,
                Some(DefsymExpr::Symbol(name, offset)) => refs
                    .symbols
                    .lookup(&crate::symbols::SymbolName::new(name.as_bytes()))
                    .and_then(|target| this.globals.get(target.index()).copied())
                    .map_or(0, |v| v.wrapping_add_signed(offset)),
                None => 0,
            };
            if let Some(slot) = this.globals.get_mut(id.index()) {
                *slot = address;
            }
        }
        this
    }

    /// The address a symbol defined by a shared library has in this
    /// output: its copy relocation, or its canonical PLT entry, or 0.
    fn shared_address(&self, id: SymbolId) -> u64 {
        if let Some(copy) = self.synth.copy_of(id) {
            let kind = if copy.relro {
                Synthetic::DynRelro
            } else {
                Synthetic::DynBss
            };
            return self
                .layout
                .synthetic(kind)
                .map_or(0, |(addr, ..)| addr.wrapping_add(copy.offset));
        }
        if self
            .refs
            .symbols
            .flags(id)
            .contains(crate::symbols::SymbolFlags::NEEDS_CANONICAL_PLT)
        {
            return self.plt_address(Owner::Global(id)).unwrap_or(0);
        }
        0
    }

    fn common_address(&self, id: SymbolId) -> u64 {
        let base = self
            .layout
            .synthetic(Synthetic::Common)
            .map_or(0, |(addr, ..)| addr);
        base.wrapping_add(self.commons.offset(id).unwrap_or(0))
    }

    fn linker_value(&self, value: Value, placement: &Placement<'_>) -> u64 {
        let layout = self.layout;
        let named = |name: &str| {
            placement
                .outputs
                .iter()
                .position(|o| o.name == name.as_bytes())
                .and_then(|i| layout.output_places.get(i))
                .copied()
                .unwrap_or((0, 0, 0))
        };
        match value {
            Value::EhdrStart | Value::ExecutableStart => layout.base,
            Value::Etext => layout.etext,
            Value::Edata => layout.edata,
            Value::BssStart => layout.bss_start,
            Value::End => layout.end,
            Value::SectionStart(name) => named(name).0,
            Value::SectionEnd(name) => named(name).1,
            Value::OutputStart(output) => {
                layout.output_places.get(output as usize).map_or(0, |p| p.0)
            }
            Value::OutputEnd(output) => {
                layout.output_places.get(output as usize).map_or(0, |p| p.1)
            }
            Value::GotBase => layout
                .synthetic(Synthetic::GotPlt)
                .or_else(|| layout.synthetic(Synthetic::Got))
                .map_or(named(".got.plt").0, |(addr, ..)| addr),
            Value::RelaIpltStart => layout
                .synthetic(Synthetic::RelaPlt)
                .map_or(named(".rela.plt").0, |(addr, ..)| addr),
            Value::RelaIpltEnd => layout
                .synthetic(Synthetic::RelaPlt)
                .map_or(named(".rela.plt").0, |(addr, _, size)| {
                    addr.wrapping_add(size)
                }),
            Value::Dynamic => layout
                .synthetic(Synthetic::Dynamic)
                .map_or(0, |(addr, ..)| addr),
            Value::Defsym(_) => 0,
        }
    }

    /// The address of offset `offset` of section `section` of `file`, or
    /// `None` if the section is not in the output.
    #[must_use]
    pub fn section_offset_address(&self, file: usize, section: u32, offset: u64) -> Option<u64> {
        let refs = &self.refs;
        let id = refs.sections.id(file, section)?;
        if !refs.sections.is_live(id) {
            // Folded by ICF: the kept section has the same contents.
            let kept = refs.sections.resolve(id)?;
            return Some(self.section_address(kept)?.wrapping_add(offset));
        }
        let kind = refs
            .files
            .get(file)?
            .object
            .as_ref()?
            .section(section)?
            .kind;
        match kind {
            SectionKind::Merge => {
                if let Some(group) = self.merged.group_of(id) {
                    let (base, _) = *self.layout.merge_place.get(group as usize)?;
                    let size = refs
                        .files
                        .get(file)?
                        .object
                        .as_ref()?
                        .section(section)?
                        .header
                        .sh_size;
                    // A symbol at the very end of the section points past its
                    // last piece.
                    if offset == size && size != 0 {
                        let last = self.merged.offset_in_group(id, offset.checked_sub(1)?)?;
                        return base.checked_add(last)?.checked_add(1);
                    }
                    return base.checked_add(self.merged.offset_in_group(id, offset)?);
                }
                Some(self.section_address(id)?.wrapping_add(offset))
            }
            SectionKind::EhFrame => {
                let start = self.section_address(id)?;
                let eh = self.eh_frames.sections.get(self.eh_frames.find(id)?)?;
                let offset32 = u32::try_from(offset).ok()?;
                for record in &eh.records {
                    if record.live && offset32 >= record.offset {
                        let delta = offset32.checked_sub(record.offset)?;
                        if delta < record.size {
                            return start
                                .checked_add(u64::from(record.out_offset.checked_add(delta)?));
                        }
                    }
                }
                // Anywhere else (usually offset 0 of an empty section): the
                // start of the section's contribution.
                Some(start)
            }
            _ => Some(self.section_address(id)?.wrapping_add(offset)),
        }
    }

    /// The address of input section `id` in the output.
    #[must_use]
    pub fn section_address(&self, id: SectionId) -> Option<u64> {
        let address = *self.layout.section_addr.get(id.index())?;
        (self
            .layout
            .section_shndx
            .get(id.index())
            .copied()
            .unwrap_or(0)
            != 0)
            .then_some(address)
    }

    /// The address `S` of a relocation target with addend `addend`, and the
    /// addend still to add. Section symbols in merge sections consume the
    /// addend (the piece is chosen by symbol value plus addend). `None` if
    /// the target lies in a section that is not in the output.
    #[must_use]
    pub fn symbol_address(&self, target: &Target, addend: i64) -> Option<(u64, i64)> {
        match target.def {
            Def::Section {
                file,
                section,
                value,
            } => {
                let merge = self
                    .refs
                    .files
                    .get(file)
                    .and_then(|f| f.object.as_ref())
                    .and_then(|o| o.section(section))
                    .is_some_and(|s| s.kind == SectionKind::Merge);
                if merge && target.is_section_symbol() {
                    let offset = value.checked_add_signed(addend)?;
                    return Some((self.section_offset_address(file, section, offset)?, 0));
                }
                if let Some(global) = target.global {
                    if !self.refs.sections.is_present_in(file, section) {
                        return None;
                    }
                    return Some((*self.globals.get(global.index())?, addend));
                }
                Some((self.section_offset_address(file, section, value)?, addend))
            }
            Def::Absolute(value) => Some((value, addend)),
            Def::Common(id) | Def::Linker(id) | Def::Shared(id) => {
                Some((*self.globals.get(id.index())?, addend))
            }
            Def::Undefined { .. } => Some((0, addend)),
        }
    }

    /// The canonical address of an IFUNC: its PLT stub (in a dynamic
    /// output, its PLT entry).
    #[must_use]
    pub fn iplt_address(&self, owner: Owner) -> Option<u64> {
        self.synth.iplt.index(owner)?;
        if self.synth.dynamic() {
            return self.plt_address(owner);
        }
        let index = u64::try_from(self.synth.iplt.index(owner)?).ok()?;
        let (base, ..) = self.layout.synthetic(Synthetic::Plt)?;
        base.checked_add(index.checked_mul(super::arch::x86_64::IPLT_ENTRY_SIZE)?)
    }

    /// The address code jumps to for `owner`'s PLT entry: `.plt.sec` with
    /// IBT, `.plt` without, or `.plt.got`.
    #[must_use]
    pub fn plt_address(&self, owner: Owner) -> Option<u64> {
        use super::arch::x86_64::{PLT_ENTRY_SIZE, PLT_GOT_ENTRY_SIZE};
        if let Some(index) = self.synth.plt_got.index(owner) {
            let entry = if self.synth.ibt {
                PLT_ENTRY_SIZE
            } else {
                PLT_GOT_ENTRY_SIZE
            };
            let (base, ..) = self.layout.synthetic(Synthetic::PltGot)?;
            return base.checked_add(u64::try_from(index).ok()?.checked_mul(entry)?);
        }
        if !self.synth.dynamic() {
            return self.iplt_address(owner);
        }
        let index = self.synth.plt_index(owner)?;
        if self.synth.ibt {
            let (base, ..) = self.layout.synthetic(Synthetic::PltSec)?;
            base.checked_add(index.checked_mul(PLT_ENTRY_SIZE)?)
        } else {
            self.lazy_plt_address(index)
        }
    }

    /// The address of lazy `.plt` entry `index` (after the header).
    #[must_use]
    pub fn lazy_plt_address(&self, index: u64) -> Option<u64> {
        use super::arch::x86_64::PLT_ENTRY_SIZE;
        let (base, ..) = self.layout.synthetic(Synthetic::Plt)?;
        base.checked_add(index.checked_add(1)?.checked_mul(PLT_ENTRY_SIZE)?)
    }

    /// The address of the address GOT entry for `owner`.
    #[must_use]
    pub fn got_address(&self, owner: Owner) -> Option<u64> {
        self.got_entry_address(owner, GotKind::Address)
    }

    /// The address of `owner`'s GOT entry of `kind`.
    #[must_use]
    pub fn got_entry_address(&self, owner: Owner, kind: GotKind) -> Option<u64> {
        let word = self.synth.got_word(owner, kind)?;
        let (base, ..) = self.layout.synthetic(Synthetic::Got)?;
        base.checked_add(word.checked_mul(8)?)
    }

    /// The address of the `.got.plt` slot of PLT entry `index` (for a
    /// static executable, IFUNC entry `index`).
    #[must_use]
    pub fn igot_address(&self, index: usize) -> Option<u64> {
        let (base, ..) = self.layout.synthetic(Synthetic::GotPlt)?;
        let slot = u64::try_from(index)
            .ok()?
            .checked_add(self.synth.got_plt_reserved)?;
        base.checked_add(slot.checked_mul(8)?)
    }

    /// The GOT base (`_GLOBAL_OFFSET_TABLE_`).
    #[must_use]
    pub fn got_base(&self) -> u64 {
        self.layout
            .synthetic(Synthetic::GotPlt)
            .or_else(|| self.layout.synthetic(Synthetic::Got))
            .map_or(0, |(addr, ..)| addr)
    }

    /// The owner key of a relocation target in `file`.
    #[must_use]
    pub fn owner(target: &Target, file: usize, symbol: u32) -> Owner {
        match target.global {
            Some(id) => Owner::Global(id),
            None => Owner::Local {
                file: u32::try_from(file).unwrap_or(u32::MAX),
                symbol,
            },
        }
    }
}
