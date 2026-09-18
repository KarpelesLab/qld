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
//! PowerPC64 uses the same machinery: its `bl` reaches ±32 MiB, and its
//! thunks ([`crate::arch::ppc64::thunk`]) also serve calls from code
//! without a TOC pointer to functions that need one. The architecture
//! decides which branches need a thunk and where they lead
//! ([`Arch::branch_thunk`]), so planning and the writer agree.

#![deny(clippy::arithmetic_side_effects)]

use crate::elf::layout::{Layout, LayoutInput};
use crate::elf::object::SectionKind;
use crate::elf::read::Relocations;
use crate::elf::read::consts::{SHF_ALLOC, SHF_EXECINSTR};
use crate::elf::refs::Def;
use crate::elf::synth::Owner;
use crate::symbols::SymbolFlags;

use super::{Arch, Branch};

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

/// The thunks of a link, sorted by output section and destination.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Thunks {
    /// Every thunk, sorted.
    pub entries: Vec<Thunk>,
    /// The architecture whose thunks these are.
    pub arch: Arch,
}

impl Thunks {
    /// Whether no thunk is needed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The bytes the thunks of output section `output` occupy.
    #[must_use]
    pub fn size_of(&self, output: u32) -> u64 {
        let count = self.entries.iter().filter(|t| t.output == output).count();
        u64::try_from(count)
            .unwrap_or(0)
            .saturating_mul(self.arch.thunk_size())
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

    /// Builds the table of `arch` from the destinations each output section
    /// needs, starting each section's pool at `pool_start`.
    #[must_use]
    pub fn build(arch: Arch, mut needed: Vec<(u32, u64)>, pool_start: &dyn Fn(u32) -> u64) -> Self {
        needed.sort_unstable();
        needed.dedup();
        let size = arch.thunk_size();
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
            next = next.saturating_add(size);
        }
        Self { entries, arch }
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

/// A thunk with its final address, for the writer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Placed {
    /// The output section its callers are in.
    pub output: u32,
    /// The address it branches to.
    pub target: u64,
    /// The thunk's own address.
    pub address: u64,
}

/// The address of the PLT entry `owner` is called through, if it has one.
fn plt_address(input: &LayoutInput<'_, '_>, layout: &Layout<'_>, owner: Owner) -> Option<u64> {
    crate::elf::values::plt_address(input.synth, layout, owner)
}

/// The GOT word `owner`'s stub jumps through (0 when there is none, which
/// the writer reports).
fn slot_of(input: &LayoutInput<'_, '_>, layout: &Layout<'_>, owner: Owner) -> u64 {
    crate::elf::values::plt_slot_address(input.synth, layout, owner).unwrap_or(0)
}

/// The address a `bl`/`b` against `symbol` of `file` ends up branching to,
/// as the writer will compute it, whether that is a stub (and the GOT word
/// it jumps through), and the callee's `st_other`.
fn branch_target(
    input: &LayoutInput<'_, '_>,
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

/// Plans the thunks the layout in `layout` needs, given the ones `previous`
/// round planned (whose space `layout` already reserves).
#[must_use]
pub fn plan(input: &LayoutInput<'_, '_>, layout: &Layout<'_>, previous: &Thunks) -> Thunks {
    let arch = input.synth.arch;
    if !arch.needs_thunks() {
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
                if let Some(destination) = arch.branch_thunk(branch) {
                    needed.push((output, destination));
                }
            }
        }
    }
    if needed.is_empty() {
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
    Thunks::build(arch, needed, &pool_start)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arch::aarch64;

    #[test]
    fn thunks_are_shared_and_ordered() {
        let plan = Thunks::build(
            Arch::AArch64,
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
        let plan = Thunks::build(Arch::AArch64, vec![(0, 0x8000_1000)], &|_| 0);
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
