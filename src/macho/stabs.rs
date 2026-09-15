//! The STABS debug map (placeholder until the debug map step).

#![deny(clippy::arithmetic_side_effects)]

use super::addr::Addresses;
use super::symtab::Nlist;

/// The debug map entries.
#[must_use]
pub fn build(addresses: &Addresses<'_, '_>) -> Vec<Nlist> {
    let _ = addresses;
    Vec::new()
}
