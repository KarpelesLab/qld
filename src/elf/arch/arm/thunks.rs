//! Planning Arm range-extension and interworking thunks.
//!
//! The shared framework ([`crate::elf::arch::thunk`]) places thunks at the
//! end of the output section holding their callers and repeats layout
//! until the set stops changing; this module decides which branches need
//! one and what it must do. A thunk is keyed by its destination, the state
//! of its callers (the thunk is in that state, so the branch to it needs
//! no `blx`) and whether the output is position-independent
//! ([`super::thunk_key`]), so callers that agree on all three share one.

#![deny(clippy::arithmetic_side_effects)]

use crate::elf::arch::thunk::Thunks;
use crate::elf::layout::{Layout, LayoutInput};
use crate::elf::object::SectionKind;
use crate::elf::read::consts::{SHF_ALLOC, SHF_EXECINSTR};
use crate::elf::refs::Def;
use crate::elf::synth::Owner;
use crate::symbols::SymbolFlags;

use super::{Branch, is_thunk_branch, pc_bias, thunk_key};

/// Where a branch goes, before any thunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Destination {
    /// The address the branch computes: `S + A` plus the PC bias, with
    /// bit 0 set for a Thumb function.
    pub address: u64,
    /// The symbol is a function, so bit 0 of its address gives its state.
    pub function: bool,
    /// The branch goes to a PLT entry or an IFUNC stub.
    pub via_stub: bool,
}

/// Where the branch of relocation `r_type` against symbol `symbol` of
/// `file` goes. `None` when it goes nowhere a thunk could help: an
/// undefined symbol without a PLT entry (the writer makes those a `nop`),
/// or a symbol in a section that is not in the output.
#[must_use]
pub fn destination<F: crate::elf::read::ElfFormat>(
    input: &LayoutInput<'_, '_, F>,
    layout: &Layout<'_>,
    file: usize,
    symbol: u32,
    addend: i64,
    r_type: u32,
) -> Option<Destination> {
    let refs = &input.refs;
    let target = refs.target(file, symbol as usize)?;
    let owner = match target.global {
        Some(id) => Owner::Global(id),
        None => Owner::Local {
            file: u32::try_from(file).unwrap_or(u32::MAX),
            symbol,
        },
    };
    let bias = pc_bias(r_type);
    let stub = |address: u64| {
        Some(Destination {
            address: address.wrapping_add_signed(addend).wrapping_add(bias),
            function: true,
            via_stub: true,
        })
    };
    if target.is_ifunc()
        && let Some(address) = crate::elf::values::iplt_address(input.synth, layout, owner)
    {
        return stub(address);
    }
    let flags = target
        .global
        .map_or(SymbolFlags::EMPTY, |id| refs.symbols.flags(id));
    if flags.contains(SymbolFlags::NEEDS_PLT | crate::elf::export::PREEMPTIBLE)
        && let Some(address) = crate::elf::values::plt_address(input.synth, layout, owner)
    {
        return stub(address);
    }
    let address = match target.def {
        Def::Section {
            file,
            section,
            value,
        } => {
            let id = refs.sections.id(file, section)?;
            if layout.section_shndx.get(id.index()).copied().unwrap_or(0) == 0 {
                return None;
            }
            layout
                .section_addr
                .get(id.index())
                .copied()?
                .wrapping_add(value)
        }
        Def::Absolute(value) => value,
        // Anything else (undefined, common, shared, linker-defined) is
        // not a branch destination a thunk can serve.
        _ => return None,
    };
    let function = target.raw.is_some_and(|raw| {
        matches!(
            raw.kind(),
            crate::elf::read::consts::STT_FUNC | crate::elf::read::consts::STT_GNU_IFUNC
        )
    });
    Some(Destination {
        address: address.wrapping_add_signed(addend).wrapping_add(bias),
        function,
        via_stub: false,
    })
}

/// The thunk key of `branch`, if it needs one, given how it is resolved.
#[must_use]
pub fn key_of(branch: &Branch, pic: bool) -> Option<u64> {
    let plan = branch.plan();
    if !plan.thunk {
        return None;
    }
    // The thunk `bx`es to the destination, so bit 0 must say which state
    // it is in; it is itself in its callers' state.
    let destination = (branch.destination & !1) | u64::from(plan.thumb_target);
    Some(thunk_key(destination, branch.thumb_caller(), pic))
}

/// Plans the thunks the layout in `layout` needs, given the ones
/// `previous` planned (whose space `layout` already reserves).
#[must_use]
pub fn plan<F: crate::elf::read::ElfFormat>(
    input: &LayoutInput<'_, '_, F>,
    layout: &Layout<'_>,
    previous: &Thunks,
) -> Thunks {
    let refs = &input.refs;
    let pic = input.mode.pic;
    let mut needed: Vec<(u32, u64)> = Vec::new();
    for (file_index, file) in refs.files.iter().enumerate() {
        let Some(object) = &file.object else {
            continue;
        };
        for (section_index, section) in object.sections.iter().enumerate() {
            let section_index = u32::try_from(section_index).unwrap_or(u32::MAX);
            let flags = section.header.sh_flags;
            if section.relocs == 0
                || flags & (SHF_ALLOC | SHF_EXECINSTR) != (SHF_ALLOC | SHF_EXECINSTR)
                || section.kind == SectionKind::Ignored
                || !refs.sections.is_live_in(file_index, section_index)
            {
                continue;
            }
            let Some(id) = refs.sections.id(file_index, section_index) else {
                continue;
            };
            let shndx = layout.section_shndx.get(id.index()).copied().unwrap_or(0);
            let Some(out) = shndx
                .checked_sub(1)
                .and_then(|p| layout.sections.get(p as usize))
            else {
                continue;
            };
            let output = out.output;
            let base = layout.section_addr.get(id.index()).copied().unwrap_or(0);
            let Some(Ok(Some(relocations))) = object
                .section(section.relocs)
                .map(|r| object.elf.relocation_section(section.relocs, &r.header))
            else {
                continue;
            };
            let Ok(data) = object.section_data(section) else {
                continue;
            };
            crate::elf::arch::for_each_relocation!(
                crate::elf::arch::Arch::Arm,
                relocations.relocations,
                data,
                |rel| {
                    if !is_thunk_branch(rel.r_type) {
                        continue;
                    }
                    let Some(destination) = destination(
                        input, layout, file_index, rel.symbol, rel.addend, rel.r_type,
                    ) else {
                        continue;
                    };
                    let at = usize::try_from(rel.offset).unwrap_or(usize::MAX);
                    let insn = match super::patch_of(rel.r_type) {
                        super::Patch::Insn(field) => field.read(data, at).unwrap_or(0),
                        _ => 0,
                    };
                    let branch = Branch {
                        r_type: rel.r_type,
                        insn,
                        place: base.wrapping_add(rel.offset),
                        destination: destination.address,
                        function: destination.function,
                        via_stub: destination.via_stub,
                    };
                    if let Some(key) = key_of(&branch, pic) {
                        needed.push((output, key));
                    }
                }
            );
        }
    }
    if needed.is_empty() {
        return Thunks::default();
    }
    let pool_start = |output: u32| -> u64 {
        let size = layout
            .sections
            .iter()
            .find(|s| s.output == output)
            .map_or(0, |s| s.size);
        let base = size.saturating_sub(previous.size_of(output));
        base.saturating_add(3) & !3
    };
    Thunks::build_for(crate::elf::arch::Arch::Arm, needed, &pool_start)
}
