//! `--emit-relocs` (`-q`): input relocations kept in the output.
//!
//! Every output section whose input sections have relocations gets a
//! non-allocated `.rela<name>` section (a layout trailer), holding each live
//! input relocation with its offset turned into the output address (the
//! section offset for non-allocated sections) and its symbol turned into an
//! index in `.symtab`:
//!
//! - global symbols, and local symbols the symbol table keeps, stay symbols;
//! - section symbols and dropped locals (`.L*` labels) become the section
//!   symbol of the output section they lie in, with the addend adjusted to
//!   the same place, including pieces of merged sections;
//! - a target that is not in the output (a discarded COMDAT copy, a
//!   garbage-collected section) gives `R_*_NONE` (0), keeping the counts
//!   known before layout.
//!
//! `.eh_frame` relocations follow their records to their output offsets;
//! those of dropped records become `R_*_NONE`. Entries keep the form the
//! architecture's inputs use, in the output's class and byte order:
//! `Elf_Rela` in `.rela<name>` sections everywhere except i386 and 32-bit
//! Arm, whose `Elf_Rel` entries go into `.rel<name>` sections and carry no
//! addend (the value is in the field the relocation patched, as the link
//! left it). Relocation types are the input's, except that `GOTPCRELX`
//! relocations the link relaxed become `R_X86_64_PC32` (or
//! `R_X86_64_32S`/`R_X86_64_32` for immediates), as in GNU ld; TLS
//! relaxations are not reflected.

#![deny(clippy::arithmetic_side_effects)]

use rayon::prelude::*;

use crate::elf::read::consts::x86_64::{
    R_X86_64_32, R_X86_64_32S, R_X86_64_GOTPCREL, R_X86_64_GOTPCRELX, R_X86_64_NONE, R_X86_64_PC32,
    R_X86_64_REX_GOTPCRELX,
};
use crate::elf::read::{RawRecord, Relocation, Relocations};
use crate::error::{Error, Result};

use super::arch::x86_64::Kind;
use super::ehframe::EhFrames;
use super::inputs::ElfInput;
use super::layout::{EMPTY_SHNDX, Member, Placed};
use super::object::{InputSection, ObjectInput, SectionKind};
use super::reloc;
use super::sections::Sections;
use super::symtab::SymtabPlan;
use super::values::Addresses;
use super::write::WriteInput;
use crate::symbols::SymbolFlags;

fn relocations<'a, F: crate::elf::read::ElfFormat>(
    object: &ObjectInput<'a, F>,
    index: u32,
) -> Option<Relocations<'a, F>> {
    let section = object.section(index)?;
    if section.relocs == 0 {
        return None;
    }
    let header = object.section(section.relocs)?.header;
    Some(
        object
            .elf
            .relocation_section(section.relocs, &header)
            .ok()??
            .relocations,
    )
}

/// The number of relocations one output section member contributes.
fn member_count<F: crate::elf::read::ElfFormat>(
    files: &[ElfInput<'_, F>],
    sections: &Sections,
    eh_frames: &EhFrames<'_, F>,
    member: Member,
) -> u64 {
    let Member::Input(id) = member else {
        return 0;
    };
    let Some((file, index)) = sections.locate(id) else {
        return 0;
    };
    let Some(object) = files.get(file).and_then(|f| f.object.as_ref()) else {
        return 0;
    };
    let Some(section) = object.section(index) else {
        return 0;
    };
    if section.kind == SectionKind::EhFrame {
        return eh_frames
            .find(id)
            .and_then(|i| eh_frames.sections.get(i))
            .map_or(0, |eh| {
                eh.records
                    .iter()
                    .map(|r| u64::from(r.relocs.1.saturating_sub(r.relocs.0)))
                    .fold(0u64, u64::saturating_add)
            });
    }
    relocations(object, index).map_or(0, |r| r.len() as u64)
}

/// Whether `--emit-relocs` writes `SHT_REL` sections: the form the
/// architecture's inputs use, as GNU ld copies it.
#[must_use]
pub fn uses_rel(arch: super::arch::Arch) -> bool {
    arch.uses_rel()
}

/// The number of relocations `--emit-relocs` writes for an output section
/// with these members.
///
/// # Errors
///
/// Returns [`Error::Unimplemented`] when a member's relocations are not in
/// the form the output writes.
pub fn count<F: crate::elf::read::ElfFormat>(
    files: &[ElfInput<'_, F>],
    sections: &Sections,
    eh_frames: &EhFrames<'_, F>,
    members: &[Placed],
    use_rel: bool,
) -> Result<u64> {
    for placed in members {
        if let Member::Input(id) = placed.member
            && let Some((file, index)) = sections.locate(id)
            && let Some(object) = files.get(file).and_then(|f| f.object.as_ref())
            && let Some(section) = object.section(index)
            && section.relocs != 0
            && let Some(relocs) = object.section(section.relocs)
            && (relocs.header.sh_type == crate::elf::read::consts::SHT_REL) != use_rel
        {
            return Err(Error::Unimplemented(format!(
                "--emit-relocs with {} relocations in {} (the output uses {})",
                if use_rel { "SHT_RELA" } else { "SHT_REL" },
                object.source().path.display(),
                if use_rel { "SHT_REL" } else { "SHT_RELA" },
            )));
        }
    }
    Ok(members
        .par_iter()
        .map(|p| member_count(files, sections, eh_frames, p.member))
        .reduce(|| 0, u64::saturating_add))
}

/// Writes one output relocation, an `Elf_Rel` or `Elf_Rela` of the
/// output's class and byte order.
fn put<F: crate::elf::read::ElfFormat>(
    out: &mut [u8],
    use_rel: bool,
    offset: u64,
    symbol: usize,
    r_type: u32,
    addend: i64,
) {
    let rel = Relocation {
        offset,
        symbol: u32::try_from(symbol).unwrap_or(0),
        r_type,
        addend,
    };
    if use_rel {
        let encoded = F::encode_rel(&rel);
        if let Some(entry) = out.get_mut(..<F::Rel as RawRecord>::SIZE) {
            entry.copy_from_slice(encoded.as_bytes());
        }
    } else {
        let encoded = F::encode_rela(&rel);
        if let Some(entry) = out.get_mut(..<F::Rela as RawRecord>::SIZE) {
            entry.copy_from_slice(encoded.as_bytes());
        }
    }
}

/// Size of one output relocation.
fn entry_size<F: crate::elf::read::ElfFormat>(use_rel: bool) -> usize {
    if use_rel {
        <F::Rel as RawRecord>::SIZE
    } else {
        <F::Rela as RawRecord>::SIZE
    }
}

/// The output symbol and addend of relocation `rel` of `file`.
fn output_symbol<F: crate::elf::read::ElfFormat>(
    addresses: &Addresses<'_, '_, F>,
    plan: &SymtabPlan,
    section_symbols: u32,
    file: usize,
    rel: &Relocation,
) -> Option<(usize, i64)> {
    if rel.symbol == 0 {
        return Some((0, rel.addend));
    }
    let refs = &addresses.refs;
    let target = refs.target(file, rel.symbol as usize)?;
    if let Some(id) = target.global {
        return plan.global_index(id).map(|index| (index, rel.addend));
    }
    if !target.is_section_symbol()
        && let Some(index) = plan.local_index(file, rel.symbol)
    {
        return Some((index, rel.addend));
    }
    // A section symbol or a dropped local: the output section's symbol.
    let super::refs::Def::Section { section, .. } = target.def else {
        return None;
    };
    let id = refs.sections.id(file, section)?;
    let id = refs.sections.resolve(id)?;
    let (s, a) = addresses.symbol_address(&target, rel.addend)?;
    let layout = addresses.layout;
    let header = *layout.section_shndx.get(id.index())?;
    if header == 0 || header == EMPTY_SHNDX || header > section_symbols {
        return None;
    }
    let base = layout.sections.get(header.checked_sub(1)? as usize)?.addr;
    let addend = s.wrapping_add_signed(a).wrapping_sub(base) as i64;
    Some((header as usize, addend))
}

/// The type GNU ld writes for a relocation: its `GOTPCRELX` conversions
/// show, other relaxations do not.
fn output_type<F: crate::elf::read::ElfFormat>(
    input: &WriteInput<'_, '_, '_, F>,
    file: usize,
    section: &InputSection<'_>,
    data: &[u8],
    rel: &Relocation,
) -> u32 {
    if !matches!(
        rel.r_type,
        R_X86_64_GOTPCREL | R_X86_64_GOTPCRELX | R_X86_64_REX_GOTPCRELX
    ) {
        return rel.r_type;
    }
    let refs = &input.addresses.refs;
    let Some(target) = refs.target(file, rel.symbol as usize) else {
        return rel.r_type;
    };
    let flags = target
        .global
        .map_or(SymbolFlags::EMPTY, |id| refs.symbols.flags(id));
    let Ok(decision) = reloc::decide::<F>(
        &input.context,
        rel,
        data,
        &target,
        flags,
        section.header.sh_flags,
    ) else {
        return rel.r_type;
    };
    match decision.class.kind {
        Kind::RelaxGotPc => R_X86_64_PC32,
        Kind::RelaxGotPcNoPic => {
            let byte = |back: usize| {
                usize::try_from(rel.offset)
                    .ok()
                    .and_then(|o| o.checked_sub(back))
                    .and_then(|o| data.get(o))
                    .copied()
                    .unwrap_or(0)
            };
            let rex = byte(3);
            // x32 clears REX.W of a load, whose immediate is an unsigned
            // 32-bit address; `test` and the binary operators keep their
            // operand size, and may have no REX prefix at all.
            let wide = if input.context.arch == super::arch::Arch::X32 {
                byte(2) != 0x8b && rex & 0xf0 == 0x40 && rex & 0x08 != 0
            } else {
                rex & 0x08 != 0
            };
            if wide { R_X86_64_32S } else { R_X86_64_32 }
        }
        _ => rel.r_type,
    }
}

/// Writes the `.rela` section of the output section at `position` in the
/// layout.
///
/// # Errors
///
/// Returns [`Error::Internal`] if the relocations do not fill the section.
pub fn write<F: crate::elf::read::ElfFormat>(
    input: &WriteInput<'_, '_, '_, F>,
    position: u32,
    out: &mut [u8],
) -> Result<()> {
    let addresses = input.addresses;
    let plan = input.symtab;
    let refs = &addresses.refs;
    let layout = addresses.layout;
    let section_symbols = layout.section_symbols;
    let use_rel = uses_rel(input.context.arch);
    let size = entry_size::<F>(use_rel);
    let Some(section) = layout.sections.get(position as usize) else {
        return Ok(());
    };
    let mut slices = Vec::with_capacity(section.members.len());
    let mut rest = out;
    for placed in &section.members {
        let count = member_count(
            refs.files,
            refs.sections,
            addresses.eh_frames,
            placed.member,
        );
        let len = usize::try_from(count)
            .unwrap_or(usize::MAX)
            .saturating_mul(size)
            .min(rest.len());
        let (head, tail) = std::mem::take(&mut rest).split_at_mut(len);
        slices.push((placed.member, head));
        rest = tail;
    }
    if !rest.is_empty() {
        return Err(Error::Internal(
            "--emit-relocs: relocation section size mismatch".into(),
        ));
    }
    slices.into_par_iter().for_each(|(member, out)| {
        let Member::Input(id) = member else {
            return;
        };
        let Some((file, index)) = refs.sections.locate(id) else {
            return;
        };
        let Some(object) = refs.files.get(file).and_then(|f| f.object.as_ref()) else {
            return;
        };
        let Some(section) = object.section(index) else {
            return;
        };
        let base = addresses.section_address(id).unwrap_or(0);
        // Whole entries: the slice was sized above from the same count.
        let mut entries = out.chunks_mut(size);
        if section.kind == SectionKind::EhFrame {
            let Some(eh) = addresses
                .eh_frames
                .find(id)
                .and_then(|i| addresses.eh_frames.sections.get(i))
            else {
                return;
            };
            // Relocations of dropped records stay, as R_*_NONE.
            for record in &eh.records {
                for reloc in record.relocs.0..record.relocs.1 {
                    let (Some(rel), Some(entry)) = (eh.reloc(reloc as usize), entries.next())
                    else {
                        continue;
                    };
                    if !record.live {
                        put::<F>(entry, use_rel, 0, 0, R_X86_64_NONE, 0);
                        continue;
                    }
                    let local = rel
                        .offset
                        .wrapping_sub(u64::from(record.offset))
                        .wrapping_add(u64::from(record.out_offset));
                    let place = base.wrapping_add(local);
                    match output_symbol(addresses, plan, section_symbols, file, &rel) {
                        Some((symbol, addend)) => {
                            put::<F>(entry, use_rel, place, symbol, rel.r_type, addend);
                        }
                        None => put::<F>(entry, use_rel, place, 0, R_X86_64_NONE, 0),
                    }
                }
            }
            return;
        }
        let data = if section.is_nobits() {
            &[][..]
        } else {
            object.section_data(section).unwrap_or_default()
        };
        if let Some(relas) = relocations(object, index) {
            let all = (0..relas.len()).filter_map(|i| relas.get(i));
            for (index, (rel, entry)) in all.zip(entries).enumerate() {
                // Linker relaxation (RISC-V) moves offsets in code.
                let relax = &addresses.layout.relax;
                let place = base.wrapping_add(relax.map(id, rel.offset));
                match output_symbol(addresses, plan, section_symbols, file, &rel) {
                    Some((symbol, addend)) => put::<F>(
                        entry,
                        use_rel,
                        place,
                        symbol,
                        relax.emitted_type(
                            id,
                            index,
                            output_type(input, file, section, data, &rel),
                        ),
                        addend,
                    ),
                    None => put::<F>(entry, use_rel, place, 0, R_X86_64_NONE, 0),
                }
            }
        }
    });
    Ok(())
}
