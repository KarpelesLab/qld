//! arm64 range-extension thunks.
//!
//! A `bl` reaches ±128 MiB. When the code is larger than that, branches to
//! far targets go through a thunk (`adrp x16; add x16; br x16`, from
//! [`crate::arch::aarch64`]) placed in an island within reach. Islands are
//! appended to `__text` every [`ISLAND_SPACING`] bytes of code; each holds
//! a thunk for every far target branched to from the code before it.

#![deny(clippy::arithmetic_side_effects)]

use hashbrown::HashMap;

use crate::arch::aarch64;

/// Code between islands. Leaves room for the islands themselves within the
/// 128 MiB branch range.
pub const ISLAND_SPACING: u64 = 100 << 20;

/// The thunks of a link: for each target, the addresses of its thunks.
#[derive(Clone, Debug, Default)]
pub struct Thunks {
    by_target: HashMap<u64, Vec<u64>>,
    /// Every thunk as (address, target), for writing.
    pub thunks: Vec<(u64, u64)>,
}

impl Thunks {
    /// Records a thunk at `address` branching to `target`.
    pub fn add(&mut self, address: u64, target: u64) {
        self.by_target.entry(target).or_default().push(address);
        self.thunks.push((address, target));
    }

    /// Whether there are no thunks.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.thunks.is_empty()
    }

    /// A thunk to `target` within branch range of `from`.
    #[must_use]
    pub fn find(&self, from: u64, target: u64) -> Option<u64> {
        self.by_target
            .get(&target)?
            .iter()
            .copied()
            .find(|&thunk| aarch64::branch_in_range(from, thunk))
    }
}
