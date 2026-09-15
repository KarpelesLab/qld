//! arm64 range-extension thunks.
//!
//! A `bl` reaches ±128 MiB. When a code section is larger than that,
//! branches to far targets go through a thunk (`adrp x16; add x16; br x16`,
//! from [`crate::arch::aarch64`], which reaches ±4 GiB) placed in an island
//! near the caller. Islands are reserved in each large code section every
//! [`ISLAND_SPACING`] bytes, so every caller has one within reach; each
//! holds one thunk per far target branched to by its nearest callers.
//!
//! Planning iterates: reserve the islands, lay out, find the branches that
//! are out of range, size the islands for their thunks, and lay out again
//! until the thunk sets stop growing. Islands only grow, so this converges
//! quickly.

#![deny(clippy::arithmetic_side_effects)]

use std::collections::BTreeSet;

use hashbrown::HashMap;

use crate::arch::aarch64;
use crate::error::{Error, Result};
use crate::macho::read::consts::ARM64_RELOC_BRANCH26;

use super::addr::Addresses;
use super::buf::to_u64;
use super::layout::{Layout, SectionKind, atom_location};
use super::reloc::{self, Referent, Resolve, Value};
use super::scan::Synthetic;
use super::state::Link;

/// Code between islands. Leaves room for the islands themselves within the
/// 128 MiB branch range.
pub const ISLAND_SPACING: u64 = 100 << 20;

/// Code spans below this need no thunks.
const THRESHOLD: u64 = 120 << 20;

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

/// Plans the thunks of an arm64 link whose layout and addresses are
/// assigned, adjusting the layout for the islands.
///
/// # Errors
///
/// [`Error::Limit`] when the plan does not converge.
pub fn plan(
    link: &Link<'_>,
    layout: &mut Layout,
    synthetic: &Synthetic,
    header_size: u64,
) -> Result<Thunks> {
    if !link.config.is_arm64() {
        return Ok(Thunks::default());
    }
    let code: Vec<usize> = layout
        .sections
        .iter()
        .enumerate()
        .filter(|(_, s)| s.has_code() && matches!(s.kind, SectionKind::Input | SectionKind::Stubs))
        .map(|(i, _)| i)
        .collect();
    let start = code
        .iter()
        .filter_map(|&i| layout.sections.get(i))
        .map(|s| s.addr)
        .min()
        .unwrap_or(0);
    let end = code
        .iter()
        .filter_map(|&i| layout.sections.get(i))
        .map(|s| s.end())
        .max()
        .unwrap_or(0);
    if end.saturating_sub(start) < THRESHOLD {
        return Ok(Thunks::default());
    }

    // Island positions: after the member that crosses each spacing
    // boundary, in every input code section.
    let mut positions: Vec<(usize, Vec<usize>)> = Vec::new();
    for &section in &code {
        let Some(out) = layout.sections.get(section) else {
            continue;
        };
        if out.kind != SectionKind::Input {
            continue;
        }
        let members = layout.members.get(section).cloned().unwrap_or_default();
        let mut after = Vec::new();
        let mut last = 0u64;
        for (position, &atom) in members.iter().enumerate() {
            let offset = layout.atom_offset.get(atom).copied().unwrap_or(0);
            let (file, local) = atom_location(link, atom);
            let size = link
                .object(file)
                .and_then(|o| o.atoms.atoms().get(local))
                .map_or(0, |a| a.size);
            // An island goes before the atom that would take the code past
            // the spacing, so that the callers before it keep one in reach
            // even when the atom itself is huge.
            if position > 0 && offset.saturating_add(size).saturating_sub(last) >= ISLAND_SPACING {
                after.push(position.saturating_sub(1));
                last = offset;
            }
        }
        if !after.is_empty() {
            positions.push((section, after));
        }
    }

    // Thunk targets per island, in island order.
    let mut targets: Vec<BTreeSet<Destination>> = Vec::new();
    for _round in 0..8 {
        let mut index = 0usize;
        for (section, after) in &positions {
            let islands: Vec<(usize, u64)> = after
                .iter()
                .map(|&position| {
                    let size = targets
                        .get(index)
                        .map_or(0, |t| to_u64(t.len()).saturating_mul(aarch64::THUNK_SIZE));
                    index = index.saturating_add(1);
                    (position, size)
                })
                .collect();
            layout.place_islands(link, *section, &islands)?;
        }
        layout.assign_addresses(link, header_size)?;
        let island_addresses: Vec<u64> = layout
            .islands
            .iter()
            .map(|i| {
                layout
                    .sections
                    .get(i.section)
                    .map_or(0, |s| s.addr.saturating_add(i.offset))
            })
            .collect();
        let empty = Thunks::default();
        let addresses = Addresses {
            link,
            layout,
            synthetic,
            thunks: &empty,
        };
        let mut wanted: Vec<BTreeSet<Destination>> = vec![BTreeSet::new(); island_addresses.len()];
        for &section in &code {
            let members = layout.members.get(section).cloned().unwrap_or_default();
            for atom in members {
                far_branches(&addresses, atom, &island_addresses, &mut wanted)?;
            }
        }
        if wanted.len() == targets.len() && wanted.iter().zip(&targets).all(|(a, b)| a.is_subset(b))
        {
            let mut thunks = Thunks::default();
            for (island, set) in island_addresses.iter().zip(&targets) {
                for (slot, &target) in set.iter().enumerate() {
                    let address =
                        island.saturating_add(to_u64(slot).saturating_mul(aarch64::THUNK_SIZE));
                    thunks.add(address, destination_address(&addresses, target)?);
                }
            }
            return Ok(thunks);
        }
        // Keep what earlier rounds needed, so island sizes only grow.
        for (index, set) in wanted.iter_mut().enumerate() {
            if let Some(previous) = targets.get(index) {
                set.extend(previous.iter().copied());
            }
        }
        targets = wanted;
    }
    Err(Error::Limit(
        "range-extension thunk placement did not converge".into(),
    ))
}

/// A branch destination that does not move with the layout: a stub
/// (`(0, symbol, addend)`), an atom and offset (`(1, atom, offset)`), or an
/// absolute address (`(2, address, 0)`).
type Destination = (u8, u64, i64);

fn destination_address(addresses: &Addresses<'_, '_>, destination: Destination) -> Result<u64> {
    let (kind, a, b) = destination;
    let address = match kind {
        0 => addresses
            .stub(crate::ids::SymbolId::new(
                usize::try_from(a).unwrap_or(usize::MAX),
            ))
            .map(|stub| stub.wrapping_add(b as u64)),
        1 => addresses
            .layout
            .atom_address(usize::try_from(a).unwrap_or(usize::MAX))
            .map(|base| base.wrapping_add(b as u64)),
        _ => Some(a),
    };
    address.ok_or_else(|| Error::Internal("thunk destination vanished from the layout".into()))
}

/// Adds the far targets of atom `atom`'s branches to the nearest island.
fn far_branches(
    addresses: &Addresses<'_, '_>,
    atom: usize,
    islands: &[u64],
    wanted: &mut [BTreeSet<Destination>],
) -> Result<()> {
    let link = addresses.link;
    let (file, local) = atom_location(link, atom);
    let Some(object) = link.object(file) else {
        return Ok(());
    };
    let Some(info) = object.atoms.atoms().get(local) else {
        return Ok(());
    };
    let section = usize::try_from(info.section).unwrap_or(usize::MAX);
    let Some(relocations) = object.relocations.get(section) else {
        return Ok(());
    };
    let data = object.file.section_data(section)?;
    let Some(base) = addresses.layout.atom_address(atom) else {
        return Ok(());
    };
    for relocation in relocations {
        if relocation.atom != local
            || relocation.relocation.relocation.r_type != ARM64_RELOC_BRANCH26
        {
            continue;
        }
        let decoded = reloc::decode(link, file, object, section, data, &relocation.relocation)?;
        let place = base.saturating_add(decoded.offset.saturating_sub(info.offset));
        let stub = match decoded.referent {
            Referent::Global(id) => addresses.stub(id).map(|stub| (stub, to_u64(id.index()))),
            _ => None,
        };
        let (destination, key) = match stub {
            Some((stub, id)) => (
                stub.wrapping_add(decoded.addend as u64),
                (0, id, decoded.addend),
            ),
            None => {
                let target = reloc::place(link, file, object, decoded.referent, decoded.addend)?;
                let key = match target {
                    reloc::Place::Atom { atom, offset } => (1, to_u64(atom), offset),
                    reloc::Place::Absolute(value) => (2, value, 0),
                    reloc::Place::Symbol(_) => (2, 0, 0),
                };
                match addresses.value(target, decoded.addend)? {
                    Value::Address(address) | Value::Absolute(address) => {
                        (address, if key.0 == 2 { (2, address, 0) } else { key })
                    }
                    Value::Import(..) => continue,
                }
            }
        };
        if aarch64::branch_in_range(place, destination) {
            continue;
        }
        let nearest = islands
            .iter()
            .enumerate()
            .filter(|&(_, &island)| aarch64::branch_in_range(place, island))
            .min_by_key(|&(_, &island)| island.abs_diff(place))
            .map(|(index, _)| index);
        let Some(index) = nearest else {
            return Err(Error::Limit(format!(
                "branch at {place:#x} to {destination:#x} has no thunk island in range"
            )));
        };
        if let Some(set) = wanted.get_mut(index) {
            set.insert(key);
        }
    }
    Ok(())
}

/// Writes the thunks that fall in output section `section` (at `address`,
/// `out` being its bytes).
///
/// # Errors
///
/// [`Error::Limit`] for a target beyond ±4 GiB.
pub fn write(thunks: &Thunks, address: u64, out: &mut [u8]) -> Result<()> {
    let end = address.saturating_add(to_u64(out.len()));
    for &(thunk, target) in &thunks.thunks {
        if thunk < address || thunk >= end {
            continue;
        }
        aarch64::write_thunk(out, thunk.saturating_sub(address), thunk, target).map_err(|_| {
            Error::Limit(format!(
                "range-extension thunk at {thunk:#x} cannot reach {target:#x}"
            ))
        })?;
    }
    Ok(())
}
