//! Mergeable section adapter (pipeline stage 8, first half).
//!
//! Sections were split into pieces while their objects were parsed
//! ([`crate::passes::merge::split_section`]). After GC, the live ones are
//! grouped by output section, piece kind and alignment, and deduplicated
//! with [`merge_split_sections`]. Each group becomes one block of its output
//! section, placed where its first input section would have gone.

#![deny(clippy::arithmetic_side_effects)]

use hashbrown::HashMap;
use rayon::prelude::*;

use crate::error::Result;
use crate::ids::SectionId;
use crate::passes::merge::{
    MergeGroup, MergeInput, MergeKind, MergedSections, merge_split_sections,
};

use super::inputs::ElfInput;
use super::object::SectionKind;
use super::place::Placement;
use super::sections::{NONE, Sections};

/// The merged sections of a link.
#[derive(Debug)]
pub struct Merged<'s, 'a> {
    /// Group descriptions.
    pub groups: Vec<MergeGroup>,
    /// The output section of each group.
    pub group_output: Vec<u32>,
    /// The first input section of each group, where the group is placed.
    pub group_first: Vec<SectionId>,
    /// For each input section, its index among the merge inputs, or
    /// [`NONE`].
    pub input_of: Vec<u32>,
    /// The merged layout.
    pub merged: MergedSections<'s, 'a>,
}

impl Merged<'_, '_> {
    /// The group of input section `id`, if it is merged.
    #[must_use]
    pub fn group_of(&self, id: SectionId) -> Option<u32> {
        let input = *self.input_of.get(id.index())?;
        if input == NONE {
            return None;
        }
        self.merged.section_group(input as usize)
    }

    /// Output offset (within its group) of input offset `offset` of merged
    /// section `id`.
    #[must_use]
    pub fn offset_in_group(&self, id: SectionId, offset: u64) -> Option<u64> {
        let input = *self.input_of.get(id.index())?;
        if input == NONE {
            return None;
        }
        self.merged.output_offset(input as usize, offset)
    }
}

/// Pieces from which deduplication runs in a pool of `widen` threads, when
/// the caller offers one: it still gains from threads past 16 (clang with
/// debug information, 18 million pieces: 183 ms on 16 threads, 155 on
/// 64), where starting the pool costs a few hundred microseconds. Links
/// without debug information have far fewer pieces (clang: under a
/// million), and more threads only slow their merge down.
const WIDE_MERGE_PIECES: usize = 4 << 20;

/// Deduplicates the live mergeable sections, in a pool of `widen` threads
/// if given and there are at least 4 Mi pieces (see `WIDE_MERGE_PIECES`).
///
/// # Errors
///
/// Returns [`crate::Error::Internal`] if the merge pass rejects its input.
pub fn merge<'s, 'a, F: crate::elf::read::ElfFormat>(
    files: &'s [ElfInput<'a, F>],
    sections: &Sections,
    placement: &Placement<'_>,
    tail_merge: bool,
    widen: Option<usize>,
) -> Result<Merged<'s, 'a>> {
    let mut groups = Vec::new();
    let mut group_output = Vec::new();
    let mut group_first = Vec::new();
    let mut keys: HashMap<(u32, u8, u64, u64), u32, foldhash::fast::FixedState> =
        HashMap::with_hasher(foldhash::fast::FixedState::with_seed(0x6d65_7267));
    let mut inputs: Vec<MergeInput<'s, 'a>> = Vec::new();
    let mut input_of = vec![NONE; sections.len()];

    // The live merge sections, found in parallel from the dense kind and
    // liveness vectors (in section ID order, which is input order).
    let merge_ids: Vec<SectionId> = sections
        .kind
        .par_iter()
        .zip(sections.live.par_iter())
        .enumerate()
        .with_min_len(1 << 16)
        .filter(|&(_, (&kind, &live))| kind == SectionKind::Merge && live)
        .map(|(index, _)| SectionId::new(index))
        .collect();
    for id in merge_ids {
        let Some((file_index, index)) = sections.locate(id) else {
            continue;
        };
        let Some(object) = files.get(file_index).and_then(|f| f.object.as_ref()) else {
            continue;
        };
        let Some(section) = object.section(index) else {
            continue;
        };
        let Some(output) = placement.output_of(id) else {
            continue;
        };
        let Some(split) = object.splits.get(section.split as usize) else {
            continue;
        };
        let (tag, unit) = match split.kind() {
            MergeKind::Strings { char_size } => (0u8, u64::from(char_size)),
            MergeKind::Fixed { entry_size } => (1u8, entry_size),
        };
        let key = (output, tag, unit, split.alignment());
        let next = u32::try_from(groups.len()).unwrap_or(NONE);
        let group = *keys.entry(key).or_insert_with(|| {
            groups.push(MergeGroup {
                kind: split.kind(),
                alignment: split.alignment(),
                tail_merge,
            });
            group_output.push(output);
            group_first.push(id);
            next
        });
        if let Some(slot) = input_of.get_mut(id.index()) {
            *slot = u32::try_from(inputs.len()).unwrap_or(NONE);
        }
        inputs.push(MergeInput { group, split });
    }
    let pieces: usize = inputs.iter().map(|input| input.split.num_pieces()).sum();
    let merged = match widen.filter(|_| pieces >= WIDE_MERGE_PIECES) {
        Some(threads) => rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .map_err(|e| crate::error::Error::Internal(format!("cannot create thread pool: {e}")))?
            .install(|| merge_split_sections(&groups, &inputs, None))?,
        None => merge_split_sections(&groups, &inputs, None)?,
    };
    Ok(Merged {
        groups,
        group_output,
        group_first,
        input_of,
        merged,
    })
}
