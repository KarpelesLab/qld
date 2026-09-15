//! `__TEXT,__eh_frame` output (placeholder until the unwinding step).

#![deny(clippy::arithmetic_side_effects)]

use crate::error::Result;

use super::addr::Addresses;
use super::state::Link;

/// The planned `__eh_frame` contents.
#[derive(Clone, Debug, Default)]
pub struct EhFramePlan {
    size: u64,
}

impl EhFramePlan {
    /// Size of the section.
    #[must_use]
    pub fn size(&self) -> u64 {
        self.size
    }
}

/// Plans `__eh_frame`.
///
/// # Errors
///
/// Malformed `__eh_frame` sections.
pub fn plan(link: &Link<'_>) -> Result<EhFramePlan> {
    let _ = link;
    Ok(EhFramePlan::default())
}

/// Writes `__eh_frame`.
///
/// # Errors
///
/// Addresses out of range.
pub fn write(addresses: &Addresses<'_, '_>, plan: &EhFramePlan, image: &mut [u8]) -> Result<()> {
    let _ = (addresses, plan, image);
    Ok(())
}
