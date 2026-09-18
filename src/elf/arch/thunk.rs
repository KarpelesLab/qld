//! Range-extension thunks.
//!
//! An AArch64 `bl`/`b` reaches ±128 MiB. When a call is farther than that,
//! the branch goes to a thunk — `adrp x16, target; add x16, x16, :lo12:;
//! br x16` — which reaches ±4 GiB.
//!
//! Thunks are planned after addresses are assigned, so planning changes the
//! addresses it was planned from: [`crate::elf::layout::layout`] repeats the
//! assignment until the set of thunks stops changing (or the round cap is
//! reached), the same fixpoint gold, lld and mold run. A thunk is placed at
//! the end of the output section holding its callers, and every caller in
//! that section that branches to the same address shares it, so the table
//! stays small; it is sorted by output section and destination, so it does
//! not depend on scheduling.
//!
//! The pool at the end of one output section must be within reach of every
//! caller in it, so an output section holding more than 128 MiB of code
//! still gets "relocation out of range" from the writer. Splitting the pool
//! is the next step if that ever matters.
//!
//! The same pool holds the Cortex-A53 erratum patches
//! ([`super::aarch64_errata`]), after the thunks: 8 bytes each, the moved
//! instruction and a branch back. Their sites depend on addresses too, so
//! they take part in the same fixpoint.

#![deny(clippy::arithmetic_side_effects)]

use crate::arch::aarch64;
use crate::elf::layout::{Layout, LayoutInput};
use crate::elf::object::SectionKind;
use crate::elf::read::Relocations;
use crate::elf::read::consts::aarch64::{R_AARCH64_CALL26, R_AARCH64_JUMP26};
use crate::elf::read::consts::{SHF_ALLOC, SHF_EXECINSTR};
use crate::elf::refs::Def;
use crate::elf::synth::Owner;
use crate::ids::SectionId;
use crate::symbols::SymbolFlags;

use super::Arch;
use super::aarch64_errata::{self, Site};

/// How many times layout may be repeated before giving up on a fixpoint.
pub const MAX_ROUNDS: u32 = 8;

/// One planned thunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Thunk {
    /// The output section (its index in `Placement::outputs`) whose callers
    /// use this thunk, and at the end of which it is placed.
    pub output: u32,
    /// The address the thunk branches to.
    pub target: u64,
    /// Offset of the thunk in its output section.
    pub offset: u64,
}

/// One planned Cortex-A53 erratum patch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Patch {
    /// The instruction it replaces.
    pub site: Site,
    /// Offset of the patch in the site's output section.
    pub offset: u64,
}

/// The thunks of a link, sorted by output section and destination, and
/// the erratum patches, sorted by output section and site.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Thunks {
    /// Every thunk, sorted.
    pub entries: Vec<Thunk>,
    /// Every erratum patch, sorted.
    pub patches: Vec<Patch>,
}

impl Thunks {
    /// Whether no thunk and no patch is needed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.patches.is_empty()
    }

    /// The bytes the thunks and patches of output section `output` occupy.
    #[must_use]
    pub fn size_of(&self, output: u32) -> u64 {
        self.thunk_bytes(output)
            .saturating_add(self.patch_bytes(output))
    }

    /// The bytes the thunks of output section `output` occupy.
    fn thunk_bytes(&self, output: u32) -> u64 {
        let count = self.entries.iter().filter(|t| t.output == output).count();
        u64::try_from(count)
            .unwrap_or(0)
            .saturating_mul(aarch64::THUNK_SIZE)
    }

    /// The bytes the erratum patches of output section `output` occupy.
    #[must_use]
    pub fn patch_bytes(&self, output: u32) -> u64 {
        let count = self
            .patches
            .iter()
            .filter(|p| p.site.output == output)
            .count();
        u64::try_from(count)
            .unwrap_or(0)
            .saturating_mul(aarch64::ERRATUM_PATCH_SIZE)
    }

    /// The offset in its output section of the thunk of `output` that
    /// branches to `target`.
    #[must_use]
    pub fn offset_of(&self, output: u32, target: u64) -> Option<u64> {
        let at = self
            .entries
            .binary_search_by_key(&(output, target), |t| (t.output, t.target))
            .ok()?;
        self.entries.get(at).map(|t| t.offset)
    }

    /// Builds the table from the destinations each output section needs,
    /// starting each section's pool at `pool_start`.
    #[must_use]
    pub fn build(mut needed: Vec<(u32, u64)>, pool_start: &dyn Fn(u32) -> u64) -> Self {
        needed.sort_unstable();
        needed.dedup();
        let mut entries = Vec::with_capacity(needed.len());
        let mut current = None;
        let mut next = 0u64;
        for (output, target) in needed {
            if current != Some(output) {
                current = Some(output);
                next = pool_start(output);
            }
            entries.push(Thunk {
                output,
                target,
                offset: next,
            });
            next = next.saturating_add(aarch64::THUNK_SIZE);
        }
        Self {
            entries,
            patches: Vec::new(),
        }
    }

    /// Adds patches for `sites` (sorted), after the thunks of each output
    /// section's pool, which starts at `pool_start`.
    #[must_use]
    pub fn with_patches(mut self, sites: Vec<Site>, pool_start: &dyn Fn(u32) -> u64) -> Self {
        let mut current = None;
        let mut next = 0u64;
        for site in sites {
            if current != Some(site.output) {
                current = Some(site.output);
                next = pool_start(site.output).saturating_add(self.thunk_bytes(site.output));
            }
            self.patches.push(Patch { site, offset: next });
            next = next.saturating_add(aarch64::ERRATUM_PATCH_SIZE);
        }
        self
    }

    /// The bytes of the thunks of output section `output`, whose contents
    /// start at address `base`, as `(offset, bytes)` pairs.
    #[must_use]
    pub fn render(&self, output: u32, base: u64) -> Vec<(u64, Vec<u8>)> {
        let mut out = Vec::new();
        for thunk in self.entries.iter().filter(|t| t.output == output) {
            let mut bytes = vec![0u8; aarch64::THUNK_SIZE as usize];
            let address = base.wrapping_add(thunk.offset);
            if aarch64::write_thunk(&mut bytes, 0, address, thunk.target).is_ok() {
                out.push((thunk.offset, bytes));
            }
        }
        out
    }
}

/// A thunk or erratum patch with its final address, for the writer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Placed {
    /// The output section its callers are in.
    pub output: u32,
    /// The address it branches to; for a patch, the address of the
    /// instruction it replaces.
    pub target: u64,
    /// The thunk's own address.
    pub address: u64,
    /// For an erratum patch, the input section and offset of the
    /// instruction it replaces; `None` for a range-extension thunk.
    pub patch: Option<(SectionId, u64)>,
}

/// The erratum patches among `placed` (sorted, as `Layout::thunks` is) whose
/// sites are in output section `output` between addresses `start` and
/// `end`.
pub fn patches_in(
    placed: &[Placed],
    output: u32,
    start: u64,
    end: u64,
) -> impl Iterator<Item = &Placed> {
    let from = placed.partition_point(|p| (p.output, p.target) < (output, start));
    placed
        .get(from..)
        .unwrap_or_default()
        .iter()
        .take_while(move |p| p.output == output && p.target < end)
        .filter(|p| p.patch.is_some())
}

/// The address of the PLT entry `owner` is called through, if it has one.
fn plt_address(input: &LayoutInput<'_, '_>, layout: &Layout<'_>, owner: Owner) -> Option<u64> {
    crate::elf::values::plt_address(input.synth, layout, owner)
}

/// The address a `bl`/`b` against `symbol` of `file` ends up branching to,
/// as the writer will compute it.
fn branch_target(
    input: &LayoutInput<'_, '_>,
    layout: &Layout<'_>,
    file: usize,
    symbol: u32,
    addend: i64,
) -> Option<u64> {
    let refs = &input.refs;
    let target = refs.target(file, symbol as usize)?;
    let owner = match target.global {
        Some(id) => Owner::Global(id),
        None => Owner::Local {
            file: u32::try_from(file).unwrap_or(u32::MAX),
            symbol,
        },
    };
    if target.is_ifunc()
        && let Some(stub) = crate::elf::values::iplt_address(input.synth, layout, owner)
    {
        return Some(stub);
    }
    let flags = target
        .global
        .map_or(SymbolFlags::EMPTY, |id| refs.symbols.flags(id));
    if flags.contains(SymbolFlags::NEEDS_PLT | crate::elf::export::PREEMPTIBLE)
        && let Some(plt) = plt_address(input, layout, owner)
    {
        return Some(plt);
    }
    let address = match target.def {
        Def::Section {
            file,
            section,
            value,
        } => {
            let id = refs.sections.id(file, section)?;
            if layout.section_shndx.get(id.index()).copied().unwrap_or(0) == 0 {
                return None;
            }
            layout
                .section_addr
                .get(id.index())
                .copied()?
                .wrapping_add(value)
        }
        Def::Absolute(value) => value,
        // Common, linker-defined and shared-library symbols are not branch
        // targets in code qld links; anything left is resolved to zero and
        // reported by the writer if it really is out of range.
        _ => return None,
    };
    Some(address.wrapping_add_signed(addend))
}

/// Plans the thunks the layout in `layout` needs, given the ones `previous`
/// round planned (whose space `layout` already reserves).
#[must_use]
pub fn plan(input: &LayoutInput<'_, '_>, layout: &Layout<'_>, previous: &Thunks) -> Thunks {
    if input.synth.arch != Arch::AArch64 {
        return Thunks::default();
    }
    let refs = &input.refs;
    let mut needed: Vec<(u32, u64)> = Vec::new();
    for (file_index, file) in refs.files.iter().enumerate() {
        let Some(object) = &file.object else {
            continue;
        };
        for (section_index, section) in object.sections.iter().enumerate() {
            let section_index = u32::try_from(section_index).unwrap_or(u32::MAX);
            let flags = section.header.sh_flags;
            if section.relocs == 0
                || flags & (SHF_ALLOC | SHF_EXECINSTR) != (SHF_ALLOC | SHF_EXECINSTR)
                || section.kind == SectionKind::Ignored
                || !refs.sections.is_live_in(file_index, section_index)
            {
                continue;
            }
            let Some(id) = refs.sections.id(file_index, section_index) else {
                continue;
            };
            let shndx = layout.section_shndx.get(id.index()).copied().unwrap_or(0);
            let Some(out) = shndx
                .checked_sub(1)
                .and_then(|p| layout.sections.get(p as usize))
            else {
                continue;
            };
            let output = out.output;
            let base = layout.section_addr.get(id.index()).copied().unwrap_or(0);
            let Some(Ok(Some(relocations))) = object
                .section(section.relocs)
                .map(|r| object.elf.relocation_section(section.relocs, &r.header))
            else {
                continue;
            };
            let Relocations::Rela(relas) = relocations.relocations else {
                continue;
            };
            for rel in relas.iter() {
                if !matches!(rel.r_type, R_AARCH64_CALL26 | R_AARCH64_JUMP26) {
                    continue;
                }
                let place = base.wrapping_add(rel.offset);
                let Some(target) = branch_target(input, layout, file_index, rel.symbol, rel.addend)
                else {
                    continue;
                };
                if !aarch64::branch_in_range(place, target) {
                    needed.push((output, target));
                }
            }
        }
    }
    let sites = aarch64_errata::scan(refs, layout, input.options);
    if needed.is_empty() && sites.is_empty() {
        return Thunks::default();
    }
    let pool_start = |output: u32| -> u64 {
        let size = layout
            .sections
            .iter()
            .find(|s| s.output == output)
            .map_or(0, |s| s.size);
        let base = size.saturating_sub(previous.size_of(output));
        base.saturating_add(3) & !3
    };
    Thunks::build(needed, &pool_start).with_patches(sites, &pool_start)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thunks_are_shared_and_ordered() {
        let plan = Thunks::build(
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
    fn patches_follow_the_thunks_of_their_pool() {
        let site = |output, address| Site {
            output,
            address,
            section: SectionId::new(0),
            offset: address,
        };
        let plan = Thunks::build(vec![(1, 0x8000_0000)], &|_| 0x100)
            .with_patches(vec![site(1, 0x10), site(1, 0x20), site(2, 0x30)], &|_| {
                0x100
            });
        assert_eq!(plan.patches[0].offset, 0x100 + aarch64::THUNK_SIZE);
        assert_eq!(
            plan.patches[1].offset,
            0x100 + aarch64::THUNK_SIZE + aarch64::ERRATUM_PATCH_SIZE
        );
        assert_eq!(plan.patches[2].offset, 0x100);
        assert_eq!(
            plan.size_of(1),
            aarch64::THUNK_SIZE + 2 * aarch64::ERRATUM_PATCH_SIZE
        );
        assert_eq!(plan.patch_bytes(2), aarch64::ERRATUM_PATCH_SIZE);
        assert!(!plan.is_empty());
    }

    #[test]
    fn patches_are_not_thunks() {
        let placed = [
            Placed {
                output: 1,
                target: 0x40,
                address: 0x200,
                patch: None,
            },
            Placed {
                output: 1,
                target: 0x40,
                address: 0x210,
                patch: Some((SectionId::new(3), 0x40)),
            },
        ];
        let found: Vec<u64> = patches_in(&placed, 1, 0, 0x100)
            .map(|p| p.address)
            .collect();
        assert_eq!(found, [0x210]);
        assert_eq!(patches_in(&placed, 1, 0x41, 0x100).count(), 0);
        assert_eq!(patches_in(&placed, 2, 0, u64::MAX).count(), 0);
    }

    #[test]
    fn rendered_thunks_branch_to_their_target() {
        let plan = Thunks::build(vec![(0, 0x8000_1000)], &|_| 0);
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
        // adrp x16, page(0x80001000) - page(0x1000); add x16, x16, #0
        assert_eq!(words[0] & 0x1f, 16);
        assert_eq!(words[1], 0x9100_0210);
    }
}
