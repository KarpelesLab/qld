//! `.eh_frame` handling and `.eh_frame_hdr` synthesis.
//!
//! Every live `.eh_frame` input section is split into CIE and FDE records
//! when objects are scanned ([`split`]). Records are then treated as units:
//!
//! - **GC** does not follow FDE relocations, which would keep every function
//!   alive. Instead a function section, once live, keeps what its FDE needs:
//!   the LSDA (`.gcc_except_table`) and the CIE's personality routine
//!   ([`EhFrames::gc_edges`]).
//! - An FDE survives only if the section its `pc_begin` points into is live
//!   ([`EhFrames::finalize`]); FDEs of GC'ed, COMDAT-discarded and folded
//!   sections are dropped.
//! - CIEs are deduplicated across the link by content and relocation
//!   targets, and kept only when a live FDE uses them.
//!
//! The output `.eh_frame` is the live records of each input section, in
//! input order, followed by a zero terminator (for unwinders that walk the
//! section from `__EH_FRAME_BEGIN__`). `.eh_frame_hdr` (`--eh-frame-hdr`)
//! holds the binary search table of FDE initial locations.

#![deny(clippy::arithmetic_side_effects)]

use hashbrown::HashMap;
use rayon::prelude::*;

use crate::elf::read::{EhFrameRecordKind, Elf64Le, ElfFormat, RelaSlice, Relocations};
use crate::error::Result;
use crate::ids::SectionId;

use super::inputs::ElfInput;
use super::object::SectionKind;
use super::refs::{Def, Refs};
use super::sections::Sections;

/// One record of an input `.eh_frame` section.
#[derive(Clone, Debug)]
pub struct Record {
    /// Offset in the input section.
    pub offset: u32,
    /// Size, including the length field.
    pub size: u32,
    /// For FDEs, the index (in this section's records) of the CIE.
    pub cie: Option<u32>,
    /// Relocation index range.
    pub relocs: (u32, u32),
    /// For FDEs, the relocation of `pc_begin`.
    pub pc_begin: Option<u32>,
    /// Whether the record is part of the output.
    pub live: bool,
    /// For live records, the offset in the output section relative to the
    /// start of this input section's contribution.
    pub out_offset: u32,
    /// For CIEs that are duplicates, `(section, record)` of the kept copy.
    pub canonical: Option<(u32, u32)>,
}

/// One input `.eh_frame` section.
#[derive(Clone, Debug)]
pub struct EhSection<'a, F: ElfFormat = Elf64Le> {
    /// Its section ID.
    pub id: SectionId,
    /// Defining file.
    pub file: usize,
    /// Section index.
    pub index: u32,
    /// The section contents.
    pub data: &'a [u8],
    /// Its relocations.
    pub relocs: RelaSlice<'a, F>,
    /// The records.
    pub records: Vec<Record>,
    /// Output size (live records only).
    pub size: u64,
}

/// All `.eh_frame` input sections of the link, in section ID order.
#[derive(Debug, Default)]
pub struct EhFrames<'a, F: crate::elf::read::ElfFormat = crate::elf::read::Elf64Le> {
    /// The sections.
    pub sections: Vec<EhSection<'a, F>>,
}

/// Splits every live `.eh_frame` section into records.
///
/// # Errors
///
/// Returns [`crate::Error::Malformed`] for malformed sections.
pub fn split<'a, F: crate::elf::read::ElfFormat>(
    files: &[ElfInput<'a, F>],
    sections: &Sections,
) -> Result<EhFrames<'a, F>> {
    let per_file: Vec<Result<Vec<EhSection<'a, F>>>> = files
        .par_iter()
        .enumerate()
        .map(|(file_index, file)| {
            let mut out = Vec::new();
            let Some(object) = &file.object else {
                return Ok(out);
            };
            for (index, section) in object.sections.iter().enumerate() {
                let index = u32::try_from(index).unwrap_or(u32::MAX);
                if section.kind != SectionKind::EhFrame {
                    continue;
                }
                let Some(id) = sections.id(file_index, index) else {
                    continue;
                };
                if !sections.is_live(id) {
                    continue;
                }
                let reloc_section = match object.section(section.relocs) {
                    Some(r) if section.relocs != 0 => {
                        object.elf.relocation_section(section.relocs, &r.header)?
                    }
                    _ => None,
                };
                let relocs = match reloc_section.map(|r| r.relocations) {
                    Some(Relocations::Rela(relas)) => relas,
                    Some(Relocations::Rel(_)) => {
                        return Err(object.malformed(
                            section.header.sh_offset,
                            ".eh_frame relocations (SHT_REL on x86-64)",
                        ));
                    }
                    None => RelaSlice::default(),
                };
                let entries = object
                    .elf
                    .eh_frame(&section.header, reloc_section.as_ref())?;
                let mut records = Vec::with_capacity(entries.len());
                let mut by_offset: Vec<(usize, u32)> = Vec::new();
                for entry in &entries {
                    let position = u32::try_from(records.len()).unwrap_or(u32::MAX);
                    if entry.record.is_cie() {
                        by_offset.push((entry.record.offset, position));
                    }
                    let to_u32 = |v: usize| u32::try_from(v).unwrap_or(u32::MAX);
                    records.push(Record {
                        offset: to_u32(entry.record.offset),
                        size: to_u32(entry.record.data.len()),
                        cie: None,
                        relocs: (
                            to_u32(entry.relocations.start),
                            to_u32(entry.relocations.end),
                        ),
                        pc_begin: entry.pc_begin_relocation.map(to_u32),
                        live: false,
                        out_offset: 0,
                        canonical: None,
                    });
                }
                for (record, entry) in records.iter_mut().zip(&entries) {
                    if let EhFrameRecordKind::Fde { cie_offset } = entry.record.kind {
                        let found = by_offset
                            .iter()
                            .find(|(offset, _)| *offset == cie_offset)
                            .map(|(_, position)| *position);
                        match found {
                            Some(cie) => record.cie = Some(cie),
                            None => {
                                return Err(object.malformed(
                                    section
                                        .header
                                        .sh_offset
                                        .saturating_add(u64::from(record.offset)),
                                    ".eh_frame FDE (CIE pointer does not name a CIE)",
                                ));
                            }
                        }
                    }
                }
                let data = object.elf.section_data(&section.header)?;
                out.push(EhSection {
                    id,
                    file: file_index,
                    index,
                    data,
                    relocs,
                    records,
                    size: 0,
                });
            }
            Ok(out)
        })
        .collect();
    let mut all = Vec::new();
    for sections in per_file {
        all.extend(sections?);
    }
    Ok(EhFrames { sections: all })
}

/// CIE contents and relocation shapes, mapped to the first copy.
type CieMap<'d> =
    HashMap<(&'d [u8], Vec<(u32, u32, i64, TargetKey)>), (u32, u32), foldhash::fast::FixedState>;

/// Identity of a relocation target, for CIE deduplication.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum TargetKey {
    Global(u32),
    Local(u32, u32),
    Invalid,
}

impl<'a, F: crate::elf::read::ElfFormat> EhFrames<'a, F> {
    /// GC edges implied by FDEs: `(function section, section it needs)`.
    #[must_use]
    pub fn gc_edges(&self, refs: &Refs<'_, '_, F>) -> Vec<(SectionId, SectionId)> {
        self.sections
            .par_iter()
            .flat_map_iter(|section| {
                let mut edges = Vec::new();
                for record in &section.records {
                    let (Some(pc_begin), Some(cie)) = (record.pc_begin, record.cie) else {
                        continue;
                    };
                    let Some(function) = reloc_section(refs, section, pc_begin) else {
                        continue;
                    };
                    let cie_relocs = section
                        .records
                        .get(cie as usize)
                        .map_or((0, 0), |c| c.relocs);
                    for index in
                        (record.relocs.0..record.relocs.1).chain(cie_relocs.0..cie_relocs.1)
                    {
                        if index == pc_begin {
                            continue;
                        }
                        if let Some(target) = reloc_section(refs, section, index) {
                            edges.push((function, target));
                        }
                    }
                }
                edges
            })
            .collect()
    }

    /// Decides which records survive, deduplicates CIEs, and assigns
    /// offsets within each section's contribution.
    pub fn finalize(&mut self, refs: &Refs<'_, '_, F>) {
        // FDE liveness, in parallel.
        self.sections.par_iter_mut().for_each(|section| {
            let mut used = vec![false; section.records.len()];
            let EhSection { records, .. } = section;
            for record in records.iter_mut() {
                let Some(pc_begin) = record.pc_begin else {
                    record.live = false;
                    continue;
                };
                if record.cie.is_none() {
                    continue;
                }
                let target = reloc_section_of(refs, section.file, &section.relocs, pc_begin);
                record.live = target.is_some_and(|t| refs.sections.is_live(t));
                if record.live
                    && let Some(slot) = record.cie.and_then(|c| used.get_mut(c as usize))
                {
                    *slot = true;
                }
            }
            for (record, used) in records.iter_mut().zip(used) {
                if record.cie.is_none() {
                    record.live = used;
                }
            }
        });

        // CIE deduplication, sequential in input order.
        let mut seen: CieMap<'_> =
            HashMap::with_hasher(foldhash::fast::FixedState::with_seed(0x6369_6573));
        let mut canonical: Vec<(usize, usize, (u32, u32))> = Vec::new();
        for (section_index, section) in self.sections.iter().enumerate() {
            for (record_index, record) in section.records.iter().enumerate() {
                if record.cie.is_some() || !record.live {
                    continue;
                }
                let Some(bytes) = section.data.get(
                    record.offset as usize
                        ..(record.offset as usize).saturating_add(record.size as usize),
                ) else {
                    continue;
                };
                let mut key_relocs = Vec::new();
                for index in record.relocs.0..record.relocs.1 {
                    let Some(rel) = section.relocs.get(index as usize) else {
                        continue;
                    };
                    let target = match refs.global_id(section.file, rel.symbol as usize) {
                        Some(id) => TargetKey::Global(id.as_u32()),
                        None => TargetKey::Local(
                            u32::try_from(section.file).unwrap_or(u32::MAX),
                            rel.symbol,
                        ),
                    };
                    let target = if rel.symbol == 0 {
                        TargetKey::Invalid
                    } else {
                        target
                    };
                    key_relocs.push((
                        u32::try_from(rel.offset.saturating_sub(u64::from(record.offset)))
                            .unwrap_or(u32::MAX),
                        rel.r_type,
                        rel.addend,
                        target,
                    ));
                }
                let here = (
                    u32::try_from(section_index).unwrap_or(u32::MAX),
                    u32::try_from(record_index).unwrap_or(u32::MAX),
                );
                let first = *seen.entry((bytes, key_relocs)).or_insert(here);
                if first != here {
                    canonical.push((section_index, record_index, first));
                }
            }
        }
        for (section_index, record_index, first) in canonical {
            if let Some(record) = self
                .sections
                .get_mut(section_index)
                .and_then(|s| s.records.get_mut(record_index))
            {
                record.live = false;
                record.canonical = Some(first);
            }
        }

        // Offsets.
        self.sections.par_iter_mut().for_each(|section| {
            let mut offset = 0u32;
            for record in &mut section.records {
                if record.live {
                    record.out_offset = offset;
                    offset = offset.saturating_add(record.size);
                }
            }
            section.size = u64::from(offset);
        });
    }

    /// The section at `index` in [`EhFrames::sections`] for input section
    /// `id`, if it is an `.eh_frame` section.
    #[must_use]
    pub fn find(&self, id: SectionId) -> Option<usize> {
        self.sections.binary_search_by_key(&id, |s| s.id).ok()
    }

    /// The kept CIE for record `record` of section `section`: `(section,
    /// record)`.
    #[must_use]
    pub fn cie_of(&self, section: usize, record: usize) -> Option<(usize, usize)> {
        let cie = self.sections.get(section)?.records.get(record)?.cie? as usize;
        let cie_record = self.sections.get(section)?.records.get(cie)?;
        Some(match cie_record.canonical {
            Some((s, r)) => (s as usize, r as usize),
            None => (section, cie),
        })
    }

    /// Number of live FDEs.
    #[must_use]
    pub fn live_fdes(&self) -> usize {
        self.sections
            .iter()
            .map(|s| {
                s.records
                    .iter()
                    .filter(|r| r.live && r.cie.is_some())
                    .count()
            })
            .sum()
    }
}

fn reloc_section<F: crate::elf::read::ElfFormat>(
    refs: &Refs<'_, '_, F>,
    section: &EhSection<'_, F>,
    index: u32,
) -> Option<SectionId> {
    reloc_section_of(refs, section.file, &section.relocs, index)
}

fn reloc_section_of<F: crate::elf::read::ElfFormat>(
    refs: &Refs<'_, '_, F>,
    file: usize,
    relocs: &RelaSlice<'_, F>,
    index: u32,
) -> Option<SectionId> {
    let rel = relocs.get(index as usize)?;
    let target = refs.target(file, rel.symbol as usize)?;
    match target.def {
        Def::Section { .. } => refs.target_section(&target),
        _ => None,
    }
}
