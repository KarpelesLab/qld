//! Global input section numbering and per-section state.
//!
//! Every section of every live object gets a dense [`SectionId`]: the
//! sections of file `f` are `base[f]..base[f] + count`. Files are numbered in
//! input order, so section IDs are too, which is what the passes' tie-breaks
//! expect. Per-section state lives in flat vectors indexed by `SectionId`;
//! because each file's sections are contiguous, a vector can be split into
//! one disjoint slice per file and updated in parallel.

#![deny(clippy::arithmetic_side_effects)]

use rayon::prelude::*;

use crate::error::{Error, Result};
use crate::ids::SectionId;
use crate::symbols::Resolution;

use super::inputs::ElfInput;
use super::object::SectionKind;

/// Sentinel for "no section" in `u32` section-indexed tables.
pub const NONE: u32 = u32::MAX;

/// Dense numbering of the input sections of live objects.
#[derive(Debug)]
pub struct Sections {
    /// First section ID of each file, or [`NONE`] if the file has no parsed
    /// object (dead archive members, the internal file).
    pub base: Vec<u32>,
    /// Number of sections of each file.
    pub count: Vec<u32>,
    /// The file each section belongs to.
    pub owner: Vec<u32>,
    /// Whether each section is part of the output: not ignored, not in a
    /// discarded COMDAT group, not garbage collected, not folded by ICF.
    pub live: Vec<bool>,
    /// For sections folded by ICF, the section they fold into ([`NONE`]
    /// otherwise). Empty when ICF did not run.
    pub fold_into: Vec<u32>,
}

impl Sections {
    /// Numbers the sections of the live objects in `files`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Limit`] if there are more than `u32::MAX` sections.
    pub fn new(files: &[ElfInput<'_>], resolution: &Resolution<'_>) -> Result<Self> {
        let mut base = Vec::with_capacity(files.len());
        let mut count = Vec::with_capacity(files.len());
        let mut total = 0u32;
        let too_many = || Error::Limit("more than 4 billion input sections".into());
        for (index, file) in files.iter().enumerate() {
            let live = resolution.is_live(crate::ids::FileId::new(index));
            match (&file.object, live) {
                (Some(object), true) => {
                    let n = u32::try_from(object.sections.len()).map_err(|_| too_many())?;
                    base.push(total);
                    count.push(n);
                    total = total.checked_add(n).ok_or_else(too_many)?;
                }
                _ => {
                    base.push(NONE);
                    count.push(0);
                }
            }
        }
        if total == NONE {
            return Err(too_many());
        }
        let total = total as usize;
        let mut owner = vec![0u32; total];
        let mut live = vec![false; total];
        {
            let owners = split_per_file(&count, &mut owner);
            let lives = split_per_file(&count, &mut live);
            owners.into_par_iter().zip(lives).enumerate().for_each(
                |(file_index, (owner_slice, live_slice))| {
                    let Some(object) = files.get(file_index).and_then(|f| f.object.as_ref()) else {
                        return;
                    };
                    let file_u32 = u32::try_from(file_index).unwrap_or(NONE);
                    owner_slice.fill(file_u32);
                    for (slot, section) in live_slice.iter_mut().zip(&object.sections) {
                        *slot = section.kind != SectionKind::Ignored;
                    }
                },
            );
        }
        Ok(Self {
            base,
            count,
            owner,
            live,
            fold_into: Vec::new(),
        })
    }

    /// Installs the ICF result: folded sections stop being live, and
    /// [`Sections::resolve`] redirects them.
    pub fn apply_folding(&mut self, fold_into: Vec<u32>) {
        for (live, &rep) in self.live.iter_mut().zip(&fold_into) {
            if rep != NONE {
                *live = false;
            }
        }
        self.fold_into = fold_into;
    }

    /// The section whose output location `id` has: `id` itself if live, the
    /// kept section if `id` was folded, `None` if it is not in the output.
    #[must_use]
    pub fn resolve(&self, id: SectionId) -> Option<SectionId> {
        if self.is_live(id) {
            return Some(id);
        }
        let rep = *self.fold_into.get(id.index())?;
        (rep != NONE).then(|| SectionId::from_u32(rep))
    }

    /// Whether section `index` of `file` is live or folded into a live one.
    #[must_use]
    pub fn is_present_in(&self, file: usize, index: u32) -> bool {
        self.id(file, index)
            .is_some_and(|id| self.resolve(id).is_some())
    }

    /// Total number of sections.
    #[must_use]
    pub fn len(&self) -> usize {
        self.owner.len()
    }

    /// Whether there are no sections.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.owner.is_empty()
    }

    /// The ID of section `index` of file `file`.
    #[must_use]
    pub fn id(&self, file: usize, index: u32) -> Option<SectionId> {
        let base = *self.base.get(file)?;
        let count = *self.count.get(file)?;
        if base == NONE || index >= count {
            return None;
        }
        Some(SectionId::from_u32(base.checked_add(index)?))
    }

    /// The file and section index of `id`.
    #[must_use]
    pub fn locate(&self, id: SectionId) -> Option<(usize, u32)> {
        let file = *self.owner.get(id.index())?;
        let base = *self.base.get(file as usize)?;
        Some((file as usize, id.as_u32().checked_sub(base)?))
    }

    /// Whether `id` is live.
    #[must_use]
    pub fn is_live(&self, id: SectionId) -> bool {
        self.live.get(id.index()).copied().unwrap_or(false)
    }

    /// Whether section `index` of `file` is live.
    #[must_use]
    pub fn is_live_in(&self, file: usize, index: u32) -> bool {
        self.id(file, index).is_some_and(|id| self.is_live(id))
    }
}

/// Splits `data` (indexed by section ID) into one slice per file.
pub fn split_per_file<'d, T>(count: &[u32], data: &'d mut [T]) -> Vec<&'d mut [T]> {
    let mut out = Vec::with_capacity(count.len());
    let mut rest = data;
    for &n in count {
        let n = (n as usize).min(rest.len());
        let (head, tail) = std::mem::take(&mut rest).split_at_mut(n);
        out.push(head);
        rest = tail;
    }
    out
}
