//! Range-extension thunks.
//!
//! An AArch64 `bl`/`b` reaches ±128 MiB. When a call is farther than that,
//! the branch goes to a thunk — `adrp x16, target; add x16, x16, :lo12:;
//! br x16` — which reaches ±4 GiB.
//!
//! Thunks are planned after addresses are assigned, so planning changes the
//! addresses it was planned from: [`crate::elf::layout::layout`] repeats the
//! assignment until the set of thunks stops changing (or the round cap is
//! reached), the same fixpoint gold, lld and mold run. Every caller in one
//! pool's reach that branches to the same address shares one thunk, so the
//! table stays small; it is sorted by output section, pool and destination,
//! so it does not depend on scheduling.
//!
//! **Pools.** A pool must be within reach of the callers that use it, so an
//! output section larger than a branch reaches gets more than one:
//! [`Arch::thunk_pool_spacing`] bytes of content apart, plus one at the
//! end, as lld spreads its thunk sections through the output. A caller uses
//! the pool nearest to it ([`Pool::nearest`]). A section smaller than the
//! spacing has exactly one pool, at its end, which is where every pool was
//! before. Branches with a much shorter reach than the spacing
//! (`R_ARM_THM_JUMP19`, `R_ARM_THM_JUMP8`) can still fail to reach one.
//!
//! A pool's position is fixed in *content* offsets — the offsets the
//! section would have if no pool took any space — so it does not move as
//! the pools grow, and layout and planning agree on it round after round.
//!
//! A pool also holds the Cortex-A53 erratum patches
//! ([`super::aarch64_errata`]), after its thunks: 8 bytes each, the moved
//! instruction and a branch back. Their sites depend on addresses too, so
//! they take part in the same fixpoint.
//!
//! PowerPC64 uses the same machinery: its `bl` reaches ±32 MiB, and its
//! thunks ([`crate::arch::ppc64::thunk`]) also serve calls from code
//! without a TOC pointer to functions that need one or through the PLT,
//! and calls to functions that clobber the TOC pointer. The architecture
//! decides which branches need a thunk and what its key is
//! ([`Arch::branch_thunk`]), so planning and the writer agree.

#![deny(clippy::arithmetic_side_effects)]

use crate::arch::aarch64;
use crate::elf::layout::{Layout, LayoutInput};
use crate::elf::object::SectionKind;
use crate::elf::read::Relocations;
use crate::elf::read::consts::{SHF_ALLOC, SHF_EXECINSTR};
use crate::elf::refs::Def;
use crate::elf::synth::Owner;
use crate::ids::SectionId;
use crate::symbols::SymbolFlags;

use super::aarch64_errata::{self, Site};
use super::{Arch, Branch};

/// How many times layout may be repeated before giving up on a fixpoint.
pub const MAX_ROUNDS: u32 = 8;

/// Where one thunk pool goes in its output section.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Pool {
    /// The output section (its index in `Placement::outputs`).
    pub output: u32,
    /// Its index among that section's pools, in address order.
    pub index: u32,
    /// Its offset in the section with no pool taking any space, which is
    /// what it is placed from and does not change between rounds.
    pub content: u64,
    /// Its address in the layout that recorded it.
    pub address: u64,
}

impl Pool {
    /// The pool of output section `output` nearest to `place`, which is
    /// the one its callers use.
    #[must_use]
    pub fn nearest(pools: &[Self], output: u32, place: u64) -> Option<u32> {
        pools
            .iter()
            .filter(|pool| pool.output == output)
            .min_by_key(|pool| pool.address.abs_diff(place))
            .map(|pool| pool.index)
    }
}

/// One planned thunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Thunk {
    /// The output section (its index in `Placement::outputs`) whose callers
    /// use this thunk.
    pub output: u32,
    /// The pool of that section it is in ([`Pool::index`]).
    pub pool: u32,
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
    /// The pool it is in ([`Pool::index`]).
    pub pool: u32,
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
    /// The architecture whose thunks these are.
    pub arch: Arch,
}

impl Thunks {
    /// Whether no thunk and no patch is needed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.patches.is_empty()
    }

    /// The bytes the thunks and patches of pool `pool` of output section
    /// `output` occupy, which is what layout reserves for it.
    #[must_use]
    pub fn size_of(&self, output: u32, pool: u32) -> u64 {
        self.thunk_bytes(output, pool)
            .saturating_add(self.patch_bytes(output, pool))
    }

    /// The bytes the thunks of one pool occupy.
    fn thunk_bytes(&self, output: u32, pool: u32) -> u64 {
        let count = self
            .entries
            .iter()
            .filter(|t| (t.output, t.pool) == (output, pool))
            .count();
        u64::try_from(count)
            .unwrap_or(0)
            .saturating_mul(self.arch.thunk_size())
    }

    /// The bytes the erratum patches of one pool occupy.
    #[must_use]
    pub fn patch_bytes(&self, output: u32, pool: u32) -> u64 {
        let count = self
            .patches
            .iter()
            .filter(|p| (p.site.output, p.pool) == (output, pool))
            .count();
        u64::try_from(count)
            .unwrap_or(0)
            .saturating_mul(aarch64::ERRATUM_PATCH_SIZE)
    }

    /// The offset in its output section of the thunk of pool `pool` of
    /// `output` that branches to `target`.
    #[must_use]
    pub fn offset_of(&self, output: u32, pool: u32, target: u64) -> Option<u64> {
        let at = self
            .entries
            .binary_search_by_key(&(output, pool, target), |t| (t.output, t.pool, t.target))
            .ok()?;
        self.entries.get(at).map(|t| t.offset)
    }

    /// Builds the AArch64 table from the destinations each pool needs,
    /// with `content` giving where each pool goes.
    #[must_use]
    pub fn build(needed: Vec<(u32, u32, u64)>, content: &dyn Fn(u32, u32) -> u64) -> Self {
        Self::build_for(Arch::AArch64, needed, content)
    }

    /// Builds the table of `arch` from the `(output, pool, key)` thunk keys
    /// each pool needs ([`Arch::branch_thunk`]). `content` gives a pool's
    /// offset in its output section with no pool taking space
    /// ([`Pool::content`]); the pools before it push it further along.
    #[must_use]
    pub fn build_for(
        arch: Arch,
        mut needed: Vec<(u32, u32, u64)>,
        content: &dyn Fn(u32, u32) -> u64,
    ) -> Self {
        needed.sort_unstable();
        needed.dedup();
        let entries = needed
            .into_iter()
            .map(|(output, pool, target)| Thunk {
                output,
                pool,
                target,
                offset: 0,
            })
            .collect();
        let mut thunks = Self {
            entries,
            patches: Vec::new(),
            arch,
        };
        thunks.assign(content);
        thunks
    }

    /// Adds patches for `sites` (sorted by output section and address),
    /// each in the pool `pool_of` puts it in, after that pool's thunks.
    #[must_use]
    pub fn with_patches(
        mut self,
        sites: Vec<Site>,
        pool_of: &dyn Fn(&Site) -> u32,
        content: &dyn Fn(u32, u32) -> u64,
    ) -> Self {
        self.patches = sites
            .into_iter()
            .map(|site| Patch {
                pool: pool_of(&site),
                site,
                offset: 0,
            })
            .collect();
        self.patches
            .sort_unstable_by_key(|p| (p.site.output, p.pool, p.site.address));
        self.assign(content);
        self
    }

    /// Gives every thunk and patch its offset: pool after pool in each
    /// output section, thunks first, each pool starting where its content
    /// offset falls once the pools before it have been inserted.
    fn assign(&mut self, content: &dyn Fn(u32, u32) -> u64) {
        let size = self.arch.thunk_size();
        let (mut thunk_at, mut patch_at) = (0usize, 0usize);
        let mut output = None;
        // What the pools of this output section have added to its offsets.
        let mut extra = 0u64;
        loop {
            // The next pool holding anything, in order.
            let next_thunk = self.entries.get(thunk_at).map(|t| (t.output, t.pool));
            let next_patch = self.patches.get(patch_at).map(|p| (p.site.output, p.pool));
            let key = match (next_thunk, next_patch) {
                (Some(thunk), Some(patch)) => thunk.min(patch),
                (Some(key), None) | (None, Some(key)) => key,
                (None, None) => break,
            };
            if output != Some(key.0) {
                output = Some(key.0);
                extra = 0;
            }
            let at = content(key.0, key.1).saturating_add(extra);
            // Layout aligns a pool to 4 before reserving it.
            let mut next = at.saturating_add(3) & !3;
            while let Some(thunk) = self.entries.get_mut(thunk_at)
                && (thunk.output, thunk.pool) == key
            {
                thunk.offset = next;
                next = next.saturating_add(size);
                thunk_at = thunk_at.saturating_add(1);
            }
            while let Some(patch) = self.patches.get_mut(patch_at)
                && (patch.site.output, patch.pool) == key
            {
                patch.offset = next;
                next = next.saturating_add(aarch64::ERRATUM_PATCH_SIZE);
                patch_at = patch_at.saturating_add(1);
            }
            extra = extra.saturating_add(next.saturating_sub(at));
        }
    }

    /// The bytes of the thunks of output section `output`, whose contents
    /// start at address `base`, as `(offset, bytes)` pairs.
    #[must_use]
    pub fn render(&self, output: u32, base: u64) -> Vec<(u64, Vec<u8>)> {
        let mut out = Vec::new();
        let size = usize::try_from(self.arch.thunk_size()).unwrap_or(0);
        for thunk in self.entries.iter().filter(|t| t.output == output) {
            let mut bytes = vec![0u8; size];
            let address = base.wrapping_add(thunk.offset);
            if self
                .arch
                .write_thunk(&mut bytes, 0, address, thunk.target)
                .is_ok()
            {
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
fn plt_address<F: crate::elf::read::ElfFormat>(
    input: &LayoutInput<'_, '_, F>,
    layout: &Layout<'_>,
    owner: Owner,
) -> Option<u64> {
    crate::elf::values::plt_address(input.synth, layout, owner)
}

/// The GOT word `owner`'s stub jumps through (0 when there is none, which
/// the writer reports).
fn slot_of<F: crate::elf::read::ElfFormat>(
    input: &LayoutInput<'_, '_, F>,
    layout: &Layout<'_>,
    owner: Owner,
) -> u64 {
    crate::elf::values::plt_slot_address(input.synth, layout, owner).unwrap_or(0)
}

/// The address a `bl`/`b` against `symbol` of `file` ends up branching to,
/// as the writer will compute it, whether that is a stub (and the GOT word
/// it jumps through), and the callee's `st_other`.
fn branch_target<F: crate::elf::read::ElfFormat>(
    input: &LayoutInput<'_, '_, F>,
    layout: &Layout<'_>,
    file: usize,
    symbol: u32,
    addend: i64,
) -> Option<(u64, Option<u64>, u8)> {
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
        return Some((stub, Some(slot_of(input, layout, owner)), 0));
    }
    let flags = target
        .global
        .map_or(SymbolFlags::EMPTY, |id| refs.symbols.flags(id));
    if flags.contains(SymbolFlags::NEEDS_PLT | crate::elf::export::PREEMPTIBLE)
        && let Some(plt) = plt_address(input, layout, owner)
    {
        return Some((plt, Some(slot_of(input, layout, owner)), 0));
    }
    let st_other = target.raw.map_or(0, |raw| raw.st_other);
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
        // Symbol 0: the addend is an absolute address (what an assembler
        // makes of a branch to an absolute symbol it resolved itself).
        Def::Undefined { .. } if symbol == 0 => 0,
        // Common, linker-defined and shared-library symbols are not branch
        // targets in code qld links; anything left is resolved to zero and
        // reported by the writer if it really is out of range.
        _ => return None,
    };
    Some((address.wrapping_add_signed(addend), None, st_other))
}

/// Plans the thunks the layout in `layout` needs; `layout` says where the
/// pools it reserved space for are ([`Pool`]).
#[must_use]
pub fn plan<F: crate::elf::read::ElfFormat>(
    input: &LayoutInput<'_, '_, F>,
    layout: &Layout<'_>,
) -> Thunks {
    let arch = input.synth.arch;
    if !arch.needs_thunks() {
        return Thunks::default();
    }
    // Arm thunks also interwork, so their planning knows the instruction
    // set of both ends.
    if arch == Arch::Arm {
        return super::arm::thunks::plan(input, layout);
    }
    let refs = &input.refs;
    let mut needed: Vec<(u32, u32, u64)> = Vec::new();
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
                if !arch.is_thunk_branch(rel.r_type) {
                    continue;
                }
                let place = base.wrapping_add(rel.offset);
                let Some((target, stub_slot, st_other)) =
                    branch_target(input, layout, file_index, rel.symbol, rel.addend)
                else {
                    continue;
                };
                let branch = Branch {
                    r_type: rel.r_type,
                    place,
                    target,
                    st_other,
                    via_stub: stub_slot.is_some(),
                    slot: stub_slot,
                };
                if let Some(key) = arch.branch_thunk(branch) {
                    let pool = Pool::nearest(&layout.pools, output, place).unwrap_or(0);
                    needed.push((output, pool, key));
                }
            }
        }
    }
    let sites = if arch == Arch::AArch64 {
        aarch64_errata::scan(refs, layout, input.options)
    } else {
        Vec::new()
    };
    if needed.is_empty() && sites.is_empty() {
        return Thunks::default();
    }
    let content = pool_content(layout);
    let pool_of =
        |site: &Site| Pool::nearest(&layout.pools, site.output, site.address).unwrap_or(0);
    Thunks::build_for(arch, needed, &content).with_patches(sites, &pool_of, &content)
}

/// Where each pool of `layout` goes, ignoring what the pools take: the
/// offsets [`Thunks::build_for`] places from, which do not change between
/// rounds.
pub fn pool_content(layout: &Layout<'_>) -> impl Fn(u32, u32) -> u64 {
    move |output: u32, pool: u32| -> u64 {
        layout
            .pools
            .iter()
            .find(|p| (p.output, p.index) == (output, pool))
            .map_or(0, |p| p.content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thunks_are_shared_and_ordered() {
        let plan = Thunks::build(
            vec![
                (1, 0, 0x9000_0000),
                (1, 0, 0x8000_0000),
                (1, 0, 0x9000_0000),
            ],
            &|_, _| 0x100,
        );
        assert_eq!(plan.entries.len(), 2);
        assert_eq!(plan.size_of(1, 0), 2 * aarch64::THUNK_SIZE);
        assert_eq!(plan.offset_of(1, 0, 0x8000_0000), Some(0x100));
        assert_eq!(
            plan.offset_of(1, 0, 0x9000_0000),
            Some(0x100 + aarch64::THUNK_SIZE)
        );
        assert_eq!(plan.offset_of(2, 0, 0x8000_0000), None);
    }

    #[test]
    fn a_pool_starts_where_the_ones_before_it_left_off() {
        // Two pools of one output section, 0x1000 bytes of content apart:
        // the second one moves by what the first one took.
        let plan = Thunks::build(
            vec![
                (1, 0, 0x8000_0000),
                (1, 1, 0x9000_0000),
                (2, 0, 0xa000_0000),
            ],
            &|_, pool| u64::from(pool) * 0x1000,
        );
        assert_eq!(plan.offset_of(1, 0, 0x8000_0000), Some(0));
        assert_eq!(
            plan.offset_of(1, 1, 0x9000_0000),
            Some(0x1000 + aarch64::THUNK_SIZE)
        );
        // Another output section starts its own accounting.
        assert_eq!(plan.offset_of(2, 0, 0xa000_0000), Some(0));
    }

    #[test]
    fn the_nearest_pool_is_the_one_used() {
        let pool = |index, address| Pool {
            output: 1,
            index,
            content: 0,
            address,
        };
        let pools = [pool(0, 0x1000), pool(1, 0x9000)];
        assert_eq!(Pool::nearest(&pools, 1, 0x2000), Some(0));
        assert_eq!(Pool::nearest(&pools, 1, 0x8000), Some(1));
        assert_eq!(Pool::nearest(&pools, 2, 0x2000), None);
    }

    #[test]
    fn patches_follow_the_thunks_of_their_pool() {
        let site = |output, address| Site {
            output,
            address,
            section: SectionId::new(0),
            offset: address,
        };
        let plan = Thunks::build(vec![(1, 0, 0x8000_0000)], &|_, _| 0x100).with_patches(
            vec![site(1, 0x10), site(1, 0x20), site(2, 0x30)],
            &|_| 0,
            &|_, _| 0x100,
        );
        assert_eq!(plan.patches[0].offset, 0x100 + aarch64::THUNK_SIZE);
        assert_eq!(
            plan.patches[1].offset,
            0x100 + aarch64::THUNK_SIZE + aarch64::ERRATUM_PATCH_SIZE
        );
        assert_eq!(plan.patches[2].offset, 0x100);
        assert_eq!(
            plan.size_of(1, 0),
            aarch64::THUNK_SIZE + 2 * aarch64::ERRATUM_PATCH_SIZE
        );
        assert_eq!(plan.patch_bytes(2, 0), aarch64::ERRATUM_PATCH_SIZE);
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
        let plan = Thunks::build(vec![(0, 0, 0x8000_1000)], &|_, _| 0);
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
