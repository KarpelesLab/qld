//! `__TEXT,__unwind_info` synthesis (placeholder until the unwinding step).

#![deny(clippy::arithmetic_side_effects)]

use crate::error::Result;

use super::addr::Addresses;
use super::reloc::Fixup;
use super::scan::Synthetic;
use super::state::Link;

/// The planned `__unwind_info` contents.
#[derive(Clone, Debug, Default)]
pub struct UnwindPlan {
    size: u64,
}

impl UnwindPlan {
    /// Size of the section.
    #[must_use]
    pub fn size(&self) -> u64 {
        self.size
    }
}

/// Plans `__unwind_info`.
///
/// # Errors
///
/// Malformed compact unwind sections.
pub fn plan(link: &Link<'_>, synthetic: &Synthetic) -> Result<UnwindPlan> {
    let _ = (link, synthetic);
    Ok(UnwindPlan::default())
}

/// Writes `__unwind_info`.
///
/// # Errors
///
/// Addresses out of range.
pub fn write(
    addresses: &Addresses<'_, '_>,
    plan: &UnwindPlan,
    image: &mut [u8],
    fixups: &mut Vec<Fixup>,
) -> Result<()> {
    let _ = (addresses, plan, image, fixups);
    Ok(())
}
