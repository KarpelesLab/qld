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
    SHF_ALLOC, SHF_EXECINSTR, SHF_GNU_RETAIN, SHF_MERGE, SHF_STRINGS, SHF_TLS, SHF_WRITE,
    SHF_X86_64_LARGE, SHT_NOBITS, SHT_NOTE, SHT_PROGBITS,
};
use crate::ids::SectionId;

use super::inputs::ElfInput;
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
    /// Under a linker script, the output section statement (index in the
    /// plan's outputs, then orphans); [`NONE`] with the default rules.
    pub stmt: u32,
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
    /// Sections a script's `/DISCARD/` (or `--orphan-handling=discard`)
    /// removes, sorted; the driver clears their live bits.
    pub discarded: Vec<SectionId>,
    /// Statement order, orphans and linker-generated sections under a
    /// linker script.
    pub script: Option<Box<crate::elf::script_layout::ScriptPlacement>>,
}

/// Flags that are carried from input to output sections.
const OUTPUT_FLAG_MASK: u64 =
    SHF_WRITE | SHF_ALLOC | SHF_EXECINSTR | SHF_MERGE | SHF_STRINGS | SHF_TLS | SHF_X86_64_LARGE;

enum Assigned<'a> {
    Rule(u16, u16),
    Orphan(&'a [u8], OrphanClass),
}

/// Assigns output sections to every live input section.
#[must_use]
pub fn place<'a>(
    rules: &RuleSet<'a>,
    files: &[ElfInput<'a>],
    sections: &Sections,
    options: &crate::args::LinkOptions,
) -> Placement<'a> {
    if let Some(script) = rules.script {
        return crate::elf::script_layout::place(script, files, sections, options);
    }
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
                    // Ignored sections are live only when a mode revives
                    // them (`.note.GNU-stack` with --emit-relocs).
                    if !*live {
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
                stmt: NONE,
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
                    stmt: NONE,
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

    let mut placement = Placement {
        outputs,
        out,
        sub,
        keep,
        discarded: Vec::new(),
        script: None,
    };
    placement.compute_flags(files, sections);
    placement
}

/// What [`Placement::compute_flags`] folds over the input sections of one
/// output section (a run of them, in section order).
#[derive(Clone, Copy, Debug)]
struct FlagFold {
    /// The union of the inputs' output flags.
    flags: u64,
    /// The type of the first input that is not `SHT_NOBITS`.
    first_type: Option<u32>,
    /// Whether every input is `SHT_NOBITS`.
    all_nobits: bool,
    /// The intersection of the inputs' flags, masked to MERGE and STRINGS.
    merge_and: u64,
    /// The first input's MERGE and STRINGS bits and merge entry size.
    first_merge: (u64, u64),
    /// Whether an input's MERGE/STRINGS bits or entry size differ from the
    /// first's.
    merge_mismatch: bool,
}

impl FlagFold {
    /// The fold of one input section.
    fn of(header: &crate::elf::read::SectionHeader) -> Self {
        let bits = header.sh_flags & (SHF_MERGE | SHF_STRINGS);
        let entsize = if bits & SHF_MERGE != 0 {
            header.sh_entsize
        } else {
            0
        };
        let nobits = header.sh_type == SHT_NOBITS;
        Self {
            flags: header.sh_flags & OUTPUT_FLAG_MASK,
            first_type: (!nobits).then_some(header.sh_type),
            all_nobits: nobits,
            merge_and: (SHF_MERGE | SHF_STRINGS) & header.sh_flags,
            first_merge: (bits, entsize),
            merge_mismatch: false,
        }
    }

    /// The fold of `self`'s sections followed by `next`'s.
    fn then(self, next: Self) -> Self {
        Self {
            flags: self.flags | next.flags,
            first_type: self.first_type.or(next.first_type),
            all_nobits: self.all_nobits && next.all_nobits,
            merge_and: self.merge_and & next.merge_and,
            first_merge: self.first_merge,
            merge_mismatch: self.merge_mismatch
                || next.merge_mismatch
                || self.first_merge != next.first_merge,
        }
    }
}

impl Placement<'_> {
    /// Computes each output section's type and flags from its live input
    /// sections. Placement does this once; the driver repeats it after
    /// garbage collection, since GNU ld decides flags from the sections that
    /// survive it.
    pub fn compute_flags(&mut self, files: &[ElfInput<'_>], sections: &Sections) {
        // Output types and flags from the inputs, as a fold over the live
        // input sections in section order. The fold is associative, so each
        // file folds its own sections in parallel and the files' results are
        // combined in file order. As in GNU ld, the output keeps SHF_MERGE
        // and SHF_STRINGS only if every input has the same two bits and,
        // when merged, the same entry size.
        let out = &self.out;
        let per_file: Vec<Vec<(u32, FlagFold)>> = files
            .par_iter()
            .enumerate()
            .map(|(file_index, file)| {
                let mut folds: Vec<(u32, FlagFold)> = Vec::new();
                let Some(object) = &file.object else {
                    return folds;
                };
                for (index, section) in object.sections.iter().enumerate() {
                    let Some(id) = sections.id(file_index, u32::try_from(index).unwrap_or(NONE))
                    else {
                        continue;
                    };
                    if !sections.is_live(id) {
                        continue;
                    }
                    let output = out.get(id.index()).copied().unwrap_or(NONE);
                    let fold = FlagFold::of(&section.header);
                    // Files usually list an output's sections together, and
                    // a file touches few outputs: a short list suffices.
                    match folds.iter_mut().rev().find(|(o, _)| *o == output) {
                        Some((_, existing)) => *existing = existing.then(fold),
                        None => folds.push((output, fold)),
                    }
                }
                folds
            })
            .collect();
        let mut folds: Vec<Option<FlagFold>> = vec![None; self.outputs.len()];
        for (output, fold) in per_file.into_iter().flatten() {
            if let Some(slot) = folds.get_mut(output as usize) {
                *slot = Some(match *slot {
                    Some(existing) => existing.then(fold),
                    None => fold,
                });
            }
        }
        for (output, fold) in self.outputs.iter_mut().zip(folds) {
            let (flags, sh_type, all_nobits, merge_bits) = match fold {
                Some(fold) => (
                    fold.flags,
                    fold.first_type,
                    fold.all_nobits,
                    if fold.merge_mismatch {
                        0
                    } else {
                        fold.merge_and
                    },
                ),
                None => (0, None, true, SHF_MERGE | SHF_STRINGS),
            };
            // An output is mergeable only if every input section is.
            let flags = flags & !(SHF_MERGE | SHF_STRINGS) | (flags & merge_bits);
            output.flags = flags;
            output.sh_type = match (sh_type, all_nobits && flags != 0) {
                (_, true) => SHT_NOBITS,
                (Some(t), false) => t,
                (None, false) => SHT_PROGBITS,
            };
        }
    }

    /// The output section of `id`, if any.
    #[must_use]
    pub fn output_of(&self, id: SectionId) -> Option<u32> {
        self.out.get(id.index()).copied().filter(|&o| o != NONE)
    }
}
