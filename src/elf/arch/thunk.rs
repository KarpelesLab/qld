//! Range-extension thunks.
//!
//! An AArch64 `bl`/`b` reaches ±128 MiB. When a call is farther than that,
//! the branch goes to a thunk — `adrp x16, target; add x16, x16, :lo12:;
//! br x16` — placed at the end of the calling output section, which reaches
//! ±4 GiB.
//!
//! Thunks are planned after addresses are assigned, so planning changes the
//! addresses it was planned from: [`super::super::layout::layout`] repeats
//! the assignment until the set of thunks stops changing (or the cap is
//! reached), which is the same fixpoint gold, lld and mold run. Callers in
//! one output section share a thunk per destination, so the table stays
//! small and its contents do not depend on scheduling: thunks are sorted by
//! output section and destination address.

use crate::arch::aarch64;

/// One planned thunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Thunk {
    /// The output section (its position in `Layout::sections`) whose
    /// callers use this thunk, and at the end of which it is placed.
    pub section: u32,
    /// The address the thunk branches to.
    pub target: u64,
    /// Offset of the thunk in its output section, assigned by planning.
    pub offset: u64,
}

/// The thunks of a link, sorted by section and target.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Thunks {
    /// Every thunk, sorted.
    pub entries: Vec<Thunk>,
}

impl Thunks {
    /// Whether no thunk is needed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The bytes the thunks of output section `section` occupy.
    #[must_use]
    pub fn size_of(&self, section: u32) -> u64 {
        let count = self.entries.iter().filter(|t| t.section == section).count();
        u64::try_from(count)
            .unwrap_or(0)
            .saturating_mul(aarch64::THUNK_SIZE)
    }

    /// The offset in its output section of the thunk of `section` that
    /// branches to `target`.
    #[must_use]
    pub fn offset_of(&self, section: u32, target: u64) -> Option<u64> {
        let at = self
            .entries
            .binary_search_by_key(&(section, target), |t| (t.section, t.target))
            .ok()?;
        self.entries.get(at).map(|t| t.offset)
    }

    /// Builds the table from the destinations each output section needs,
    /// assigning offsets from `pool_start` in each section.
    #[must_use]
    pub fn plan(mut needed: Vec<(u32, u64)>, pool_start: &dyn Fn(u32) -> u64) -> Self {
        needed.sort_unstable();
        needed.dedup();
        let mut entries = Vec::with_capacity(needed.len());
        let mut section = u32::MAX;
        let mut next = 0u64;
        for (in_section, target) in needed {
            if in_section != section {
                section = in_section;
                next = pool_start(in_section);
            }
            entries.push(Thunk {
                section: in_section,
                target,
                offset: next,
            });
            next = next.saturating_add(aarch64::THUNK_SIZE);
        }
        Self { entries }
    }

    /// The bytes of the thunks of output section `section`, which starts at
    /// address `base`, in offset order.
    #[must_use]
    pub fn render(&self, section: u32, base: u64) -> Vec<(u64, Vec<u8>)> {
        let mut out = Vec::new();
        for thunk in self.entries.iter().filter(|t| t.section == section) {
            let mut bytes = vec![0u8; aarch64::THUNK_SIZE as usize];
            let address = base.wrapping_add(thunk.offset);
            if aarch64::write_thunk(&mut bytes, 0, address, thunk.target).is_ok() {
                out.push((thunk.offset, bytes));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thunks_are_shared_and_ordered() {
        let plan = Thunks::plan(
            vec![(1, 0x9000_0000), (1, 0x8000_0000), (1, 0x9000_0000)],
            &|_| 0x100,
        );
        assert_eq!(plan.entries.len(), 2);
        assert_eq!(plan.size_of(1), 2 * aarch64::THUNK_SIZE);
        assert_eq!(plan.offset_of(1, 0x8000_0000), Some(0x100));
        assert_eq!(
            plan.offset_of(1, 0x9000_0000),
            Some(0x100 + aarch64::THUNK_SIZE)
        );
        assert_eq!(plan.offset_of(2, 0x8000_0000), None);
    }

    #[test]
    fn rendered_thunks_branch_to_their_target() {
        let plan = Thunks::plan(vec![(0, 0x8000_1000)], &|_| 0);
        let rendered = plan.render(0, 0x1000);
        assert_eq!(rendered.len(), 1);
        let (offset, bytes) = &rendered[0];
        assert_eq!(*offset, 0);
        let words: Vec<u32> = bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|w| u32::from_le_bytes(*w))
            .collect();
        assert_eq!(words[2], aarch64::BR_X16);
    }
}
