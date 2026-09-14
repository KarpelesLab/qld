//! Output section assignment.
//!
//! Every live input section is matched against the [`RuleSet`] in parallel.
//! Sections matched by a rule go to that rule's output section; orphans get
//! an output section named after them, grouped by name and flag class, and
//! placed after the rule their class follows. The result is a list of
//! [`OutputSection`]s in final order plus, per input section, its output
//! section and the index of the input description that matched it (which
//! orders sections within the output).

#![deny(clippy::arithmetic_side_effects)]

use hashbrown::HashMap;
use rayon::prelude::*;

use crate::elf::read::consts::{
    SHF_ALLOC, SHF_EXECINSTR, SHF_GNU_RETAIN, SHF_INFO_LINK, SHF_LINK_ORDER, SHF_MERGE,
    SHF_STRINGS, SHF_TLS, SHF_WRITE, SHT_NOBITS, SHT_NOTE, SHT_PROGBITS,
};
use crate::ids::SectionId;

use super::inputs::ElfInput;
use super::object::SectionKind;
use super::rules::{OrphanClass, RuleSet, Synthetic, orphan_class};
use super::sections::{NONE, Sections, split_per_file};

/// One output section.
#[derive(Clone, Debug)]
pub struct OutputSection<'a> {
    /// The name.
    pub name: &'a [u8],
    /// The rule it comes from, or `u16::MAX` for an orphan.
    pub rule: u16,
    /// Sort key: rule order, then orphan order.
    pub rank: (u16, u32),
    /// Section type.
    pub sh_type: u32,
    /// Flags, the union of the input sections' flags.
    pub flags: u64,
    /// Synthetic content.
    pub synthetic: Synthetic,
    /// Sections here are GC roots.
    pub keep: bool,
}

impl OutputSection<'_> {
    /// Whether the section is allocated.
    #[must_use]
    pub fn is_alloc(&self) -> bool {
        self.flags & SHF_ALLOC != 0
    }
}

/// The result of placement.
#[derive(Debug)]
pub struct Placement<'a> {
    /// Output sections, indexed by output ID (not in final order).
    pub outputs: Vec<OutputSection<'a>>,
    /// Output section of each input section, or [`NONE`].
    pub out: Vec<u32>,
    /// Index of the matching input description of each input section.
    pub sub: Vec<u16>,
    /// Whether each input section is a GC root because of its placement
    /// (`KEEP`) or its flags (`SHF_GNU_RETAIN`, notes).
    pub keep: Vec<bool>,
}

/// Flags that are not carried from input to output sections.
const OUTPUT_FLAG_MASK: u64 =
    SHF_WRITE | SHF_ALLOC | SHF_EXECINSTR | SHF_MERGE | SHF_STRINGS | SHF_TLS;

enum Assigned<'a> {
    Rule(u16, u16),
    Orphan(&'a [u8], OrphanClass),
}

/// Assigns output sections to every live input section.
#[must_use]
pub fn place<'a>(rules: &RuleSet, files: &[ElfInput<'a>], sections: &Sections) -> Placement<'a> {
    let total = sections.len();
    let mut out = vec![NONE; total];
    let mut sub = vec![0u16; total];
    let mut keep = vec![false; total];

    // Parallel: match rules, and collect orphans per file.
    let orphans: Vec<Vec<(u32, &'a [u8], OrphanClass)>> = {
        let outs = split_per_file(&sections.count, &mut out);
        let subs = split_per_file(&sections.count, &mut sub);
        let keeps = split_per_file(&sections.count, &mut keep);
        outs.into_par_iter()
            .zip(subs)
            .zip(keeps)
            .enumerate()
            .map(|(file_index, ((out, sub), keep))| {
                let mut orphans = Vec::new();
                let Some(file) = files.get(file_index) else {
                    return orphans;
                };
                let Some(object) = &file.object else {
                    return orphans;
                };
                let base = sections.base.get(file_index).copied().unwrap_or(NONE);
                if base == NONE {
                    return orphans;
                }
                let file_name = file
                    .file
                    .map(|f| match f.member() {
                        Some(member) => member.as_bytes(),
                        None => f.path().as_os_str().as_encoded_bytes(),
                    })
                    .unwrap_or_default();
                for (index, section) in object.sections.iter().enumerate() {
                    let Some(live) = sections.live.get((base as usize).saturating_add(index))
                    else {
                        break;
                    };
                    if !*live || section.kind == SectionKind::Ignored {
                        continue;
                    }
                    let header = &section.header;
                    let assigned = match rules.place(section.name, file_name) {
                        Some(placement) => Assigned::Rule(placement.output, placement.input),
                        None => Assigned::Orphan(
                            section.name,
                            orphan_class(header.sh_flags, header.sh_type),
                        ),
                    };
                    let flags_keep = header.sh_flags & SHF_GNU_RETAIN != 0
                        || header.sh_flags & SHF_ALLOC == 0
                        || header.sh_type == SHT_NOTE;
                    if let Some(slot) = keep.get_mut(index) {
                        *slot = flags_keep;
                    }
                    match assigned {
                        Assigned::Rule(output, input) => {
                            if let Some(slot) = out.get_mut(index) {
                                *slot = u32::from(output);
                            }
                            if let Some(slot) = sub.get_mut(index) {
                                *slot = input;
                            }
                            if rules
                                .outputs
                                .get(usize::from(output))
                                .is_some_and(|rule| rule.keep)
                                && let Some(slot) = keep.get_mut(index)
                            {
                                *slot = true;
                            }
                        }
                        Assigned::Orphan(name, class) => {
                            orphans.push((u32::try_from(index).unwrap_or(NONE), name, class));
                        }
                    }
                }
                orphans
            })
            .collect()
    };

    let mut outputs: Vec<OutputSection<'a>> = rules
        .outputs
        .iter()
        .enumerate()
        .map(|(index, rule)| {
            let index = u16::try_from(index).unwrap_or(u16::MAX);
            OutputSection {
                name: rule.name.as_bytes(),
                rule: index,
                rank: (index, 0),
                sh_type: SHT_PROGBITS,
                flags: 0,
                synthetic: rule.synthetic,
                keep: rule.keep,
            }
        })
        .collect();

    // Sequential: number orphans by first appearance.
    let mut orphan_ids: HashMap<(&'a [u8], OrphanClass), u32, foldhash::fast::FixedState> =
        HashMap::with_hasher(foldhash::fast::FixedState::with_seed(0x6f72_7068));
    for (file_index, list) in orphans.into_iter().enumerate() {
        let base = sections.base.get(file_index).copied().unwrap_or(NONE);
        for (index, name, class) in list {
            let next = u32::try_from(outputs.len()).unwrap_or(NONE);
            let id = *orphan_ids.entry((name, class)).or_insert_with(|| {
                let order = next;
                outputs.push(OutputSection {
                    name,
                    rule: u16::MAX,
                    rank: (rules.hold(class), order),
                    sh_type: SHT_PROGBITS,
                    flags: 0,
                    synthetic: Synthetic::None,
                    keep: false,
                });
                next
            });
            if let Some(slot) = base
                .checked_add(index)
                .and_then(|at| out.get_mut(at as usize))
            {
                *slot = id;
            }
        }
    }

    // Output types and flags from the inputs.
    // (flags, first non-NOBITS type, all NOBITS, MERGE/STRINGS bits every
    // input has).
    let mut types: Vec<(u64, Option<u32>, bool, u64)> =
        vec![(0, None, true, SHF_MERGE | SHF_STRINGS); outputs.len()];
    for (file_index, file) in files.iter().enumerate() {
        let Some(object) = &file.object else {
            continue;
        };
        for index in 0..object.sections.len() {
            let Some(id) = sections.id(file_index, u32::try_from(index).unwrap_or(NONE)) else {
                continue;
            };
            let output = out.get(id.index()).copied().unwrap_or(NONE);
            let (Some(slot), Some(section)) =
                (types.get_mut(output as usize), object.sections.get(index))
            else {
                continue;
            };
            let header = &section.header;
            slot.0 |= header.sh_flags & OUTPUT_FLAG_MASK;
            slot.3 &= header.sh_flags;
            if header.sh_type != SHT_NOBITS {
                slot.2 = false;
                if slot.1.is_none() {
                    slot.1 = Some(header.sh_type);
                }
            }
        }
    }
    for (output, (flags, sh_type, all_nobits, merge_bits)) in outputs.iter_mut().zip(types) {
        // An output is mergeable only if every input section is.
        let flags = flags & !(SHF_MERGE | SHF_STRINGS) | (flags & merge_bits);
        output.flags = flags;
        output.sh_type = match (sh_type, all_nobits && flags != 0) {
            (_, true) => SHT_NOBITS,
            (Some(t), false) => t,
            (None, false) => SHT_PROGBITS,
        };
    }
    let _ = (SHF_INFO_LINK, SHF_LINK_ORDER);

    Placement {
        outputs,
        out,
        sub,
        keep,
    }
}

impl Placement<'_> {
    /// The output section of `id`, if any.
    #[must_use]
    pub fn output_of(&self, id: SectionId) -> Option<u32> {
        self.out.get(id.index()).copied().filter(|&o| o != NONE)
    }
}
