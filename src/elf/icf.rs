//! Identical code folding (`--icf=all`, `--icf=safe`), pipeline stage 8
//! second half: builds [`IcfInput`] from the live sections and applies the
//! result of [`fold_identical`].
//!
//! **Foldable** sections are live, allocated, read-only, non-TLS regular
//! sections with contents, outside `KEEP` output sections. The *key* that
//! must match exactly is `(type, flags, entry size, alignment, output
//! section, size)`, interned to a dense number.
//!
//! **Relocation targets** are input sections plus offsets (compared by
//! equivalence class), merged-string pieces (compared by output position,
//! since merging runs first), or symbols and absolute values (compared by
//! identity).
//!
//! **Address significance** for `--icf=safe`: a file's `.llvm_addrsig`
//! table when it has one; otherwise (GCC objects) an executable section is
//! significant when any relocation other than `R_X86_64_PLT32` refers to
//! it, and read-only data never is (`docs/optimizations.md`).
//!
//! Folded sections leave the output; references to them, their symbols and
//! their `.eh_frame` FDEs go to the kept section.

#![deny(clippy::arithmetic_side_effects)]

use std::sync::atomic::{AtomicBool, Ordering};

use hashbrown::HashMap;
use rayon::prelude::*;

use crate::diag::{Diagnostic, DiagnosticSink, Severity};
use crate::elf::read::Relocations;
use crate::elf::read::consts::{
    SHF_ALLOC, SHF_EXECINSTR, SHF_TLS, SHF_WRITE, SHT_NOBITS, x86_64::R_X86_64_PLT32,
};
use crate::error::{Error, Result};
use crate::ids::SectionId;
use crate::passes::{Csr, IcfInput, IcfMode, IcfReloc, IcfSection, IcfTarget, fold_identical};

use super::merge::Merged;
use super::object::SectionKind;
use super::place::Placement;
use super::refs::{Def, Refs};
use super::sections::NONE;

/// Section properties that must match for two sections to fold.
type Key = (u32, u64, u64, u64, u32, u64);

fn foldable_section(refs: &Refs<'_, '_>, placement: &Placement<'_>, id: SectionId) -> Option<Key> {
    if !refs.sections.is_live(id) {
        return None;
    }
    let (file, index) = refs.sections.locate(id)?;
    let section = refs.files.get(file)?.object.as_ref()?.section(index)?;
    let header = &section.header;
    let flags = header.sh_flags;
    if section.kind != SectionKind::Regular
        || flags & SHF_ALLOC == 0
        || flags & (SHF_WRITE | SHF_TLS) != 0
        || header.sh_type == SHT_NOBITS
        || header.sh_size == 0
    {
        return None;
    }
    let output = placement.output_of(id)?;
    if placement
        .outputs
        .get(output as usize)
        .is_none_or(|o| o.keep)
    {
        return None;
    }
    Some((
        header.sh_type,
        flags,
        header.sh_entsize,
        header.sh_addralign,
        output,
        header.sh_size,
    ))
}

/// Decodes the ULEB128 symbol indices of an `.llvm_addrsig` section.
fn addrsig_symbols(data: &[u8]) -> Vec<usize> {
    let mut out = Vec::new();
    let mut value = 0u64;
    let mut shift = 0u32;
    for &byte in data {
        if shift < 64 {
            value |= u64::from(byte & 0x7f) << shift;
        }
        if byte & 0x80 == 0 {
            if let Ok(index) = usize::try_from(value) {
                out.push(index);
            }
            value = 0;
            shift = 0;
        } else {
            shift = shift.saturating_add(7);
        }
    }
    out
}

/// Runs ICF and marks folded sections in `refs.sections`' fold map, which
/// the caller installs. Returns `fold_into` (with [`NONE`] for sections that
/// are not folded).
///
/// # Errors
///
/// Returns [`Error::Internal`] if the pass rejects its input.
pub fn fold(
    refs: &Refs<'_, '_>,
    placement: &Placement<'_>,
    merged: &Merged<'_, '_>,
    mode: IcfMode,
    print: bool,
    diagnostics: &dyn DiagnosticSink,
) -> Result<Vec<u32>> {
    let total = refs.sections.len();

    // Keys, interned in section order.
    let raw_keys: Vec<Option<Key>> = (0..total)
        .into_par_iter()
        .map(|index| foldable_section(refs, placement, SectionId::new(index)))
        .collect();
    let mut interned: HashMap<Key, u64, foldhash::fast::FixedState> =
        HashMap::with_hasher(foldhash::fast::FixedState::with_seed(0x0069_6366));
    let keys: Vec<Option<u64>> = raw_keys
        .iter()
        .map(|key| {
            key.map(|k| {
                let next = u64::try_from(interned.len()).unwrap_or(u64::MAX);
                *interned.entry(k).or_insert(next)
            })
        })
        .collect();

    // Address significance.
    let significant: Vec<AtomicBool> = (0..total).map(|_| AtomicBool::new(false)).collect();
    refs.files
        .par_iter()
        .enumerate()
        .for_each(|(file_index, file)| {
            let Some(object) = &file.object else {
                return;
            };
            if object.addrsig != 0
                && let Some(section) = object.section(object.addrsig)
                && let Ok(data) = object.elf.section_data(&section.header)
            {
                for symbol in addrsig_symbols(data) {
                    if let Some(target) = refs.target(file_index, symbol)
                        && let Some(id) = refs.target_section(&target)
                        && let Some(flag) = significant.get(id.index())
                    {
                        flag.store(true, Ordering::Relaxed);
                    }
                }
            }
            for (index, section) in object.sections.iter().enumerate() {
                let Some(id) = refs
                    .sections
                    .id(file_index, u32::try_from(index).unwrap_or(NONE))
                else {
                    continue;
                };
                if section.relocs == 0 || !refs.sections.is_live(id) {
                    continue;
                }
                let Some(Ok(Some(relocations))) = object
                    .section(section.relocs)
                    .map(|r| object.elf.relocation_section(section.relocs, &r.header))
                else {
                    continue;
                };
                let Relocations::Rela(relas) = relocations.relocations else {
                    continue;
                };
                for rel in relas.iter() {
                    if rel.r_type == R_X86_64_PLT32 {
                        continue;
                    }
                    let Some(target) = refs.target(file_index, rel.symbol as usize) else {
                        continue;
                    };
                    let Some(target_id) = refs.target_section(&target) else {
                        continue;
                    };
                    let target_file = refs
                        .files
                        .get(refs.sections.locate(target_id).map_or(0, |l| l.0));
                    let has_addrsig = target_file
                        .and_then(|f| f.object.as_ref())
                        .is_some_and(|o| o.addrsig != 0);
                    if !has_addrsig && let Some(flag) = significant.get(target_id.index()) {
                        flag.store(true, Ordering::Relaxed);
                    }
                }
            }
        });

    let sections: Vec<IcfSection<'_>> = (0..total)
        .into_par_iter()
        .map(|index| {
            let id = SectionId::new(index);
            let key = keys.get(index).copied().flatten();
            let located = refs.sections.locate(id);
            let section = located.and_then(|(file, section)| {
                refs.files.get(file)?.object.as_ref()?.section(section)
            });
            let contents = match (key, section, located) {
                (Some(_), Some(section), Some((file, _))) => refs
                    .files
                    .get(file)
                    .and_then(|f| f.object.as_ref())
                    .and_then(|o| o.elf.section_data(&section.header).ok())
                    .unwrap_or_default(),
                _ => &[],
            };
            let executable = section.is_some_and(|s| s.header.sh_flags & SHF_EXECINSTR != 0);
            IcfSection {
                contents,
                key: key.unwrap_or(u64::MAX),
                foldable: key.is_some(),
                address_significant: executable
                    && significant
                        .get(index)
                        .is_some_and(|f| f.load(Ordering::Relaxed)),
            }
        })
        .collect();

    let relocs = Csr::build_parallel(
        total,
        IcfReloc {
            offset: 0,
            kind: 0,
            addend: 0,
            target: IcfTarget::Value(0),
        },
        |row| {
            if sections.get(row).is_none_or(|s| !s.foldable) {
                return 0;
            }
            section_relocs(refs, SectionId::new(row)).map_or(0, |(_, relas)| relas.len())
        },
        |row, slot| {
            if sections.get(row).is_none_or(|s| !s.foldable) {
                return 0;
            }
            let Some((file, relas)) = section_relocs(refs, SectionId::new(row)) else {
                return 0;
            };
            let mut written = 0usize;
            for (rel, out) in relas.iter().zip(slot.iter_mut()) {
                let Some(target) = refs.target(file, rel.symbol as usize) else {
                    continue;
                };
                let (target, addend) = icf_target(refs, merged, &target, rel.addend);
                *out = IcfReloc {
                    offset: rel.offset,
                    kind: rel.r_type,
                    addend,
                    target,
                };
                written = written.saturating_add(1);
            }
            written
        },
    )
    .map_err(|e| Error::Internal(format!("ICF relocations: {e}")))?;
    let input =
        IcfInput::new(sections, relocs).map_err(|e| Error::Internal(format!("ICF input: {e}")))?;
    let result = fold_identical(&input, mode);

    if print {
        for group in result.report().groups() {
            let mut diagnostic = Diagnostic::new(
                Severity::Note,
                format!("selected section {}", describe(refs, group.kept)),
            );
            for &folded in group.folded {
                diagnostic = diagnostic.note(format!(
                    "removing identical section {}",
                    describe(refs, folded)
                ));
            }
            diagnostics.emit(diagnostic.order(u64::from(group.kept.as_u32())));
        }
    }

    Ok(result
        .fold_into()
        .par_iter()
        .enumerate()
        .map(|(index, rep)| {
            if rep.index() == index {
                NONE
            } else {
                rep.as_u32()
            }
        })
        .collect())
}

fn section_relocs<'a>(
    refs: &Refs<'_, 'a>,
    id: SectionId,
) -> Option<(
    usize,
    crate::elf::read::RelaSlice<'a, crate::elf::read::Elf64Le>,
)> {
    let (file, index) = refs.sections.locate(id)?;
    let object = refs.files.get(file)?.object.as_ref()?;
    let section = object.section(index)?;
    if section.relocs == 0 {
        return None;
    }
    let header = object.section(section.relocs)?.header;
    match object
        .elf
        .relocation_section(section.relocs, &header)
        .ok()??
        .relocations
    {
        Relocations::Rela(relas) => Some((file, relas)),
        Relocations::Rel(_) => None,
    }
}

fn icf_target(
    refs: &Refs<'_, '_>,
    merged: &Merged<'_, '_>,
    target: &super::refs::Target,
    addend: i64,
) -> (IcfTarget, i64) {
    if target.is_ifunc()
        && let Some(id) = target.global
    {
        return (IcfTarget::Symbol(id), addend);
    }
    match target.def {
        Def::Section {
            file,
            section,
            value,
        } => {
            let Some(id) = refs.sections.id(file, section) else {
                return (IcfTarget::Value(u64::MAX), addend);
            };
            if let Some(group) = merged.group_of(id) {
                let (offset, addend) = if target.is_section_symbol() {
                    (value.checked_add_signed(addend).unwrap_or(0), 0)
                } else {
                    (value, addend)
                };
                let piece = merged.offset_in_group(id, offset).unwrap_or(u64::MAX);
                // Group numbers are small; pieces fit in the low 48 bits of
                // any real section.
                return (IcfTarget::Value((u64::from(group) << 48) ^ piece), addend);
            }
            (
                IcfTarget::Section {
                    section: id,
                    offset: value,
                },
                addend,
            )
        }
        Def::Absolute(value) => (IcfTarget::Value(value), addend),
        Def::Common(id) | Def::Linker(id) | Def::Shared(id) => (IcfTarget::Symbol(id), addend),
        Def::Undefined { .. } => match target.global {
            Some(id) => (IcfTarget::Symbol(id), addend),
            None => (IcfTarget::Value(0), addend),
        },
    }
}

fn describe(refs: &Refs<'_, '_>, id: SectionId) -> String {
    let Some((file, index)) = refs.sections.locate(id) else {
        return format!("section {}", id.as_u32());
    };
    let Some(input) = refs.files.get(file) else {
        return format!("section {}", id.as_u32());
    };
    let name = input
        .object
        .as_ref()
        .and_then(|o| o.section(index))
        .map_or_else(String::new, |s| {
            String::from_utf8_lossy(s.name).into_owned()
        });
    format!("{}:({name})", input.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_addrsig() {
        assert_eq!(addrsig_symbols(&[1, 0x85, 0x01, 3]), vec![1, 133, 3]);
        assert_eq!(addrsig_symbols(&[0xff; 12]), Vec::<usize>::new());
    }
}
