//! Turning a laid-out m68k image into an AmigaDOS load file.
//!
//! The ELF backend does the whole link — resolution, garbage collection,
//! section placement, addresses and relocation — and renders the image in
//! memory, exactly as `--oformat binary` does
//! ([`crate::elf::rawout`]). This module then cuts that image into hunks.
//!
//! - **One hunk per allocated output section.** Executable sections become
//!   [`HUNK_CODE`](super::format::HUNK_CODE), `SHT_NOBITS` ones
//!   [`HUNK_BSS`](super::format::HUNK_BSS) and the rest
//!   [`HUNK_DATA`](super::format::HUNK_DATA). The hunks are ordered code,
//!   data, then bss, keeping layout order inside each group, because
//!   AmigaDOS enters the image at offset 0 of the first hunk, and because
//!   that is the order vlink's `amigahunk` target writes.
//! - **Every hunk is based at 0.** The image was laid out at real
//!   addresses, so each 32-bit absolute field holds an address; the hunk it
//!   points into is found from the relocation's symbol, and the field is
//!   rewritten as the offset inside that hunk. The pair (hunk, offset of
//!   the field) becomes a [`HUNK_RELOC32`](super::format::HUNK_RELOC32)
//!   entry, which the loader adds the hunk's load address back to.
//! - **A field whose target is not in the image** (an absolute symbol: a
//!   hardware register, an `--defsym` constant) keeps its value and gets no
//!   relocation entry, as in vlink.
//! - **PC-relative fields** are resolved by the ELF writer and need no
//!   entry, but only if both ends are in the same hunk: hunks move
//!   independently at load time. One that crosses hunks is an error, as it
//!   is in vlink.
//!
//! What is deliberately not supported yet: a GOT or PLT (Amiga code is not
//! position-independent in the ELF sense), TLS, overlays, and Hunk object
//! output (`HUNK_UNIT`). Each is rejected rather than written wrongly.

#![deny(clippy::arithmetic_side_effects)]

use crate::diag::Diagnostic;
use crate::elf::arch::Arch;
use crate::elf::arch::m68k::{
    R_68K_8, R_68K_16, R_68K_32, R_68K_GNU_VTENTRY, R_68K_GNU_VTINHERIT, R_68K_NONE, R_68K_PC8,
    R_68K_PC16, R_68K_PC32, R_68K_PLT8, R_68K_PLT16, R_68K_PLT32,
};
use crate::elf::layout::{Member, Trailer};
use crate::elf::read::SectionIndex;
use crate::elf::read::consts::{SHF_EXECINSTR, SHT_NOBITS};
use crate::elf::refs::Def;
use crate::elf::rules::Synthetic;
use crate::elf::values::Addresses;
use crate::elf::write::WriteInput;
use crate::error::{Error, Result};

use super::format::{self, Kind, MemFlags, RelocForm};

/// Where one hunk came from.
struct Plan {
    /// Its position in [`crate::elf::layout::Layout::sections`].
    position: usize,
    /// What it holds.
    kind: Kind,
    /// Its base address in the laid-out image.
    addr: u64,
    /// Its size in bytes.
    size: u64,
}

/// Renders the load file for the image the ELF writer produced.
///
/// # Errors
///
/// [`Error::Unimplemented`] for anything the load file cannot express (a
/// GOT or PLT, TLS, a relocation type with no Hunk equivalent),
/// [`Error::Reported`] when a relocation was rejected, and [`Error::Limit`]
/// when the image is larger than the format's fields allow.
pub fn render<F: crate::elf::read::ElfFormat>(
    input: &WriteInput<'_, '_, '_, F>,
    image: &[u8],
) -> Result<Vec<u8>> {
    let addresses = input.addresses;
    let layout = addresses.layout;
    if input.context.arch != Arch::M68k {
        return Err(Error::Unimplemented(format!(
            "AmigaOS Hunk output for {} (only m68k is a Hunk architecture)",
            input.context.arch.emulation()
        )));
    }
    let mut plans: Vec<Plan> = Vec::new();
    for (position, section) in layout.sections.iter().enumerate() {
        if section.trailer != Trailer::None || !section.is_alloc() || section.size == 0 {
            continue;
        }
        let kind = if section.sh_type == SHT_NOBITS {
            Kind::Bss
        } else if section.flags & SHF_EXECINSTR != 0 {
            Kind::Code
        } else {
            Kind::Data
        };
        plans.push(Plan {
            position,
            kind,
            addr: section.addr,
            size: section.size,
        });
    }
    // Code, then data, then bss; layout order inside each group.
    plans.sort_by_key(|plan| plan.kind);
    if plans.is_empty() {
        return Err(Error::Internal(
            "an AmigaOS Hunk load file needs at least one hunk".into(),
        ));
    }
    let mut hunks: Vec<format::Hunk> = Vec::with_capacity(plans.len());
    for plan in &plans {
        let data = if plan.kind == Kind::Bss {
            Vec::new()
        } else {
            let section = layout
                .sections
                .get(plan.position)
                .ok_or_else(|| Error::Internal("hunk plan outside the layout".into()))?;
            let start = usize::try_from(section.offset)
                .map_err(|_| Error::Limit("the image does not fit in memory".into()))?;
            let len = usize::try_from(plan.size)
                .map_err(|_| Error::Limit("the image does not fit in memory".into()))?;
            let end = start
                .checked_add(len)
                .ok_or_else(|| Error::Internal("a hunk ends outside the image".into()))?;
            image
                .get(start..end)
                .ok_or_else(|| Error::Internal("a hunk lies outside the image".into()))?
                .to_vec()
        };
        hunks.push(format::Hunk {
            kind: plan.kind,
            alloc: plan.size,
            data,
            memory: MemFlags::Any,
            relocs: Vec::new(),
            symbols: Vec::new(),
        });
    }
    let mut relocs = collect_relocations(input, &plans)?;
    apply(&mut hunks, &plans, &mut relocs)?;
    if input.options.strip != crate::args::StripMode::All {
        collect_symbols(input, &plans, &mut hunks);
    }
    format::write(&hunks, RelocForm::Long)
}

/// One 32-bit absolute field that the loader must adjust.
struct Fixup {
    /// The hunk the field is in.
    hunk: u32,
    /// Its offset in that hunk.
    offset: u64,
    /// The hunk it points into.
    target: u32,
}

/// The hunk `address` lies in, if any. `anchor` is an address known to be
/// inside the wanted hunk, which settles the boundary between two adjacent
/// hunks (an end-of-section marker, or an addend that reaches the end).
fn hunk_of(plans: &[Plan], address: u64, anchor: Option<u64>) -> Option<u32> {
    let find = |address: u64| {
        plans
            .iter()
            .position(|plan| address >= plan.addr && address.wrapping_sub(plan.addr) < plan.size)
    };
    let index = anchor.and_then(find).or_else(|| find(address))?;
    u32::try_from(index).ok()
}

/// Collects the fields the loader must adjust, and rejects the relocations
/// a load file cannot express.
fn collect_relocations<F: crate::elf::read::ElfFormat>(
    input: &WriteInput<'_, '_, '_, F>,
    plans: &[Plan],
) -> Result<Vec<Fixup>> {
    let addresses = input.addresses;
    let refs = &addresses.refs;
    let layout = addresses.layout;
    let mut out = Vec::new();
    let mut errors = 0usize;
    let mut report = |message: String| {
        errors = errors.saturating_add(1);
        input.diagnostics.emit(Diagnostic::error(message));
    };
    for (hunk, plan) in plans.iter().enumerate() {
        let hunk = u32::try_from(hunk).unwrap_or(0);
        let section = layout
            .sections
            .get(plan.position)
            .ok_or_else(|| Error::Internal("hunk plan outside the layout".into()))?;
        for placed in &section.members {
            let id = match placed.member {
                Member::Input(id) => id,
                // Merged string and constant pools hold no relocations.
                Member::Merge(_) => continue,
                Member::Synthetic(Synthetic::Common | Synthetic::None) => continue,
                Member::Synthetic(kind) => {
                    return Err(Error::Unimplemented(format!(
                        "{kind:?} in an AmigaOS Hunk output: a load file has no GOT, PLT or \
                         dynamic section"
                    )));
                }
            };
            let Some((file, index)) = refs.sections.locate(id) else {
                continue;
            };
            let Some(object) = refs.files.get(file).and_then(|f| f.object.as_ref()) else {
                continue;
            };
            let Some(section) = object.section(index) else {
                continue;
            };
            if section.relocs == 0 {
                continue;
            }
            let Some(header) = object.section(section.relocs) else {
                continue;
            };
            let Some(relas) = object
                .elf
                .relocation_section(section.relocs, &header.header)?
            else {
                continue;
            };
            let base = addresses.section_address(id).unwrap_or(0);
            let Some(targets) = refs.for_file(file) else {
                continue;
            };
            for i in 0..relas.relocations.len() {
                let Some(rel) = relas.relocations.get(i) else {
                    continue;
                };
                let place = base.wrapping_add(rel.offset);
                let target = targets.target(rel.symbol as usize);
                let value = target
                    .as_ref()
                    .and_then(|t| addresses.symbol_address(t, rel.addend))
                    .map(|(s, a)| s.wrapping_add_signed(a));
                let anchor = target.as_ref().and_then(|t| section_anchor(addresses, t));
                match rel.r_type {
                    R_68K_NONE | R_68K_GNU_VTINHERIT | R_68K_GNU_VTENTRY => {}
                    R_68K_32 => {
                        let Some(value) = value else { continue };
                        // Absolute symbols and undefined weak references
                        // keep the value the writer stored.
                        if matches!(target.as_ref().map(|t| t.def), Some(Def::Absolute(_))) {
                            continue;
                        }
                        let Some(to) = hunk_of(plans, value, anchor) else {
                            continue;
                        };
                        out.push(Fixup {
                            hunk,
                            offset: place.wrapping_sub(plan.addr),
                            target: to,
                        });
                    }
                    R_68K_16 | R_68K_8 => {
                        if !matches!(target.as_ref().map(|t| t.def), Some(Def::Absolute(_)))
                            && value.and_then(|v| hunk_of(plans, v, anchor)).is_some()
                        {
                            report(format!(
                                "{}: {} to a relocatable address cannot be stored in an \
                                 AmigaOS Hunk load file, which only relocates longwords",
                                describe(input, file, place),
                                Arch::M68k.reloc_label(rel.r_type),
                            ));
                        }
                    }
                    R_68K_PC32 | R_68K_PC16 | R_68K_PC8 | R_68K_PLT32 | R_68K_PLT16
                    | R_68K_PLT8 => {
                        let Some(value) = value else { continue };
                        let Some(to) = hunk_of(plans, value, anchor) else {
                            continue;
                        };
                        if to != hunk {
                            report(format!(
                                "{}: {} reaches hunk {to} from hunk {hunk}; the hunks of a \
                                 load file are relocated independently, so a PC-relative \
                                 reference cannot cross them",
                                describe(input, file, place),
                                Arch::M68k.reloc_label(rel.r_type),
                            ));
                        }
                    }
                    other => {
                        return Err(Error::Unimplemented(format!(
                            "{} in an AmigaOS Hunk output",
                            Arch::M68k.reloc_label(other)
                        )));
                    }
                }
            }
        }
    }
    if errors > 0 && !input.options.noinhibit_exec {
        return Err(Error::Reported { errors });
    }
    Ok(out)
}

/// An address inside the output section that holds `target`'s definition,
/// for [`hunk_of`].
fn section_anchor<F: crate::elf::read::ElfFormat>(
    addresses: &Addresses<'_, '_, F>,
    target: &crate::elf::refs::Target,
) -> Option<u64> {
    let Def::Section { file, section, .. } = target.def else {
        return None;
    };
    let id = addresses.refs.sections.id(file, section)?;
    let id = addresses.refs.sections.resolve(id)?;
    addresses.section_address(id)
}

/// Names the place a relocation problem is at, as a diagnostic does.
fn describe<F: crate::elf::read::ElfFormat>(
    input: &WriteInput<'_, '_, '_, F>,
    file: usize,
    place: u64,
) -> String {
    let path = input.addresses.refs.files.get(file).map_or_else(
        || "<unknown>".to_owned(),
        |f| f.path().display().to_string(),
    );
    format!("{path}: relocation at 0x{place:x}")
}

/// Rewrites each relocated longword as an offset in the hunk it points
/// into, and records the fixups in the hunks they belong to.
fn apply(hunks: &mut [format::Hunk], plans: &[Plan], fixups: &mut [Fixup]) -> Result<()> {
    // Deterministic: by hunk, then target hunk, then offset, whatever
    // order the layout walk produced.
    fixups.sort_by_key(|f| (f.hunk, f.target, f.offset));
    for fixup in fixups.iter() {
        let Some(base) = plans.get(fixup.target as usize).map(|plan| plan.addr) else {
            continue;
        };
        let Some(hunk) = hunks.get_mut(fixup.hunk as usize) else {
            continue;
        };
        let at = usize::try_from(fixup.offset)
            .map_err(|_| Error::Limit("a Hunk relocation lies outside the image".into()))?;
        let end = at
            .checked_add(4)
            .ok_or_else(|| Error::Limit("a Hunk relocation lies outside the image".into()))?;
        let slot = hunk
            .data
            .get_mut(at..end)
            .ok_or_else(|| Error::Internal("a Hunk relocation lies outside its hunk".into()))?;
        let mut word = [0u8; 4];
        word.copy_from_slice(slot);
        let value = u32::from_be_bytes(word);
        let offset = value.wrapping_sub(base as u32);
        slot.copy_from_slice(&offset.to_be_bytes());
        let offset = u32::try_from(fixup.offset)
            .map_err(|_| Error::Limit("a Hunk relocation lies outside the image".into()))?;
        match hunk.relocs.last_mut() {
            Some((target, offsets)) if *target == fixup.target => offsets.push(offset),
            _ => hunk.relocs.push((fixup.target, vec![offset])),
        }
    }
    Ok(())
}

/// Fills each hunk's symbol list: the symbols the ELF `.symtab` plan keeps,
/// globals first and then locals, each in address order, which is the order
/// vlink writes them in.
fn collect_symbols<F: crate::elf::read::ElfFormat>(
    input: &WriteInput<'_, '_, '_, F>,
    plans: &[Plan],
    hunks: &mut [format::Hunk],
) {
    let addresses = input.addresses;
    let refs = &addresses.refs;
    let plan = input.symtab;
    let mut globals: Vec<(u32, u32, Vec<u8>)> = Vec::new();
    for &id in plan.globals.iter().chain(&plan.hidden) {
        let target = refs.global_target(id, true);
        if !matches!(target.def, Def::Section { .. } | Def::Common(_)) {
            continue;
        }
        let Some(&address) = addresses.globals.get(id.index()) else {
            continue;
        };
        let anchor = section_anchor(addresses, &target);
        let Some(hunk) = hunk_of(plans, address, anchor) else {
            continue;
        };
        let name = refs.symbols.name(id).bytes();
        if name.is_empty() {
            continue;
        }
        let Some(offset) = offset_in(plans, hunk, address) else {
            continue;
        };
        globals.push((hunk, offset, name.to_vec()));
    }
    let mut locals: Vec<(u32, u32, Vec<u8>)> = Vec::new();
    for (file, kept) in plan.locals.iter().enumerate() {
        let Some(object) = refs.files.get(file).and_then(|f| f.object.as_ref()) else {
            continue;
        };
        let symbols = object.elf.symbols();
        for &index in kept {
            let index = index as usize;
            let Some(raw) = symbols.get_raw(index) else {
                continue;
            };
            let Ok(SectionIndex::Section(section)) = symbols.section(index, &raw) else {
                continue;
            };
            let Some(address) = addresses.section_offset_address(file, section, raw.st_value)
            else {
                continue;
            };
            let anchor = refs
                .sections
                .id(file, section)
                .and_then(|id| refs.sections.resolve(id))
                .and_then(|id| addresses.section_address(id));
            let Some(hunk) = hunk_of(plans, address, anchor) else {
                continue;
            };
            let Ok(name) = symbols.name(index, &raw) else {
                continue;
            };
            if name.is_empty() {
                continue;
            }
            let Some(offset) = offset_in(plans, hunk, address) else {
                continue;
            };
            locals.push((hunk, offset, name.to_vec()));
        }
    }
    for list in [&mut globals, &mut locals] {
        list.sort_by(|a, b| (a.0, a.1, &a.2).cmp(&(b.0, b.1, &b.2)));
    }
    for (hunk, offset, name) in globals.into_iter().chain(locals) {
        if let Some(hunk) = hunks.get_mut(hunk as usize) {
            hunk.symbols.push((name, offset));
        }
    }
}

/// `address` as an offset in hunk `hunk`.
fn offset_in(plans: &[Plan], hunk: u32, address: u64) -> Option<u32> {
    let plan = plans.get(hunk as usize)?;
    u32::try_from(address.wrapping_sub(plan.addr)).ok()
}
