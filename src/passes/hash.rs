//! Fixed-seed hashing shared by the passes.
//!
//! Hash values only decide which items get compared; every decision is
//! confirmed by exact comparison, and every tie is broken by input order. So
//! results never depend on hash values, but using a fixed seed still keeps
//! run-to-run behavior (and performance) reproducible.

use std::hash::BuildHasher;

use foldhash::fast::{FixedState, FoldHasher};

/// The seed for every hash computed by the passes ("qld_pass").
const SEED: u64 = 0x716c_645f_7061_7373;

/// Returns a fresh hasher with the passes' fixed seed.
#[inline]
pub(crate) fn hasher() -> FoldHasher<'static> {
    FixedState::with_seed(SEED).build_hasher()
}
