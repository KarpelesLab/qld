//! The output symbol table (`.symtab`, `.strtab`).
//!
//! [`plan`] decides, before layout, which symbols are written and where
//! their names go, so both sizes are known. [`write_symtab`] and
//! [`write_strtab`] fill the tables after layout, in parallel.
//!
//! Local symbols of live sections come first, file by file, followed by
//! global symbols with hidden or internal visibility (which an executable
//! turns into locals, as GNU ld does), then the other globals by symbol ID.
//! Section symbols and assembler temporaries (`.L*`) are dropped; `-x` drops
//! every local and `-s` the whole table.

#![deny(clippy::arithmetic_side_effects)]

use rayon::prelude::*;

use crate::args::{DiscardMode, LinkOptions, StripMode};
use crate::elf::read::consts::{
    SHN_ABS, SHN_UNDEF, STB_GLOBAL, STB_LOCAL, STB_WEAK, STT_FILE, STT_NOTYPE, STT_OBJECT,
    STT_SECTION, STT_TLS, STV_DEFAULT, STV_HIDDEN, STV_INTERNAL,
};
use crate::elf::read::{RawSymbol, SectionIndex};
use crate::ids::SymbolId;
use crate::symbols::{DefinitionKind, SymbolFlags};

use super::defined::{LinkerSymbols, is_hidden};
use super::dso::{REF_REGULAR, REF_REGULAR_STRONG};
use super::dynsym::shndx_of_address;
use super::export::PREEMPTIBLE;
use super::refs::{Def, Refs};
use super::values::Addresses;

/// Size of one symbol table entry.
pub const SYM_SIZE: usize = 24;

/// Which symbols the output table holds.
#[derive(Debug, Default)]
pub struct SymtabPlan {
    /// Per file: local symbol indices kept.
    pub locals: Vec<Vec<u32>>,
    /// Globals written as locals (hidden visibility).
    pub hidden: Vec<SymbolId>,
    /// Globals.
    pub globals: Vec<SymbolId>,
    /// First entry index of each file's locals (after the null symbol).
    local_base: Vec<usize>,
    /// String table offset where each file's local names start.
    local_names: Vec<usize>,
    /// Index of the first entry after file locals.
    hidden_base: usize,
    /// String table offset where hidden global names start.
    hidden_names: usize,
    /// Index of the first global entry.
    pub first_global: usize,
    /// String table offset where global names start.
    global_names: usize,
    /// Total number of entries, including the null symbol.
    pub count: usize,
    /// String table size.
    pub strtab_size: usize,
}

fn keep_local(name: &[u8], raw: &RawSymbol, discard: DiscardMode) -> bool {
    if raw.kind() == STT_SECTION || name.is_empty() {
        return false;
    }
    match discard {
        DiscardMode::All => false,
        DiscardMode::None => true,
        DiscardMode::Default | DiscardMode::Locals => !name.starts_with(b".L"),
    }
}

fn global_visibility(refs: &Refs<'_, '_>, linker: &LinkerSymbols, id: SymbolId) -> u8 {
    let target = refs.global_target(id, true);
    if let Def::Linker(_) = target.def {
        return match linker.entries.iter().find(|(i, _)| *i == id) {
            Some((_, value)) if is_hidden(*value) => STV_HIDDEN,
            _ => STV_DEFAULT,
        };
    }
    target.raw.map_or(STV_DEFAULT, |raw| raw.visibility())
}

/// Plans the symbol table. Returns an empty plan for `-s`.
#[must_use]
pub fn plan(refs: &Refs<'_, '_>, linker: &LinkerSymbols, options: &LinkOptions) -> SymtabPlan {
    if options.strip == StripMode::All {
        return SymtabPlan::default();
    }
    let discard = options.discard;
    let per_file: Vec<(Vec<u32>, usize)> = refs
        .files
        .par_iter()
        .enumerate()
        .map(|(file_index, file)| {
            let mut kept = Vec::new();
            let mut names = 0usize;
            let Some(object) = &file.object else {
                return (kept, names);
            };
            if refs
                .sections
                .base
                .get(file_index)
                .is_none_or(|&b| b == super::sections::NONE)
            {
                return (kept, names);
            }
            let symbols = object.elf.symbols();
            for index in 1..object.first_global {
                let Some(raw) = symbols.get_raw(index) else {
                    break;
                };
                let Ok(name) = symbols.name(index, &raw) else {
                    continue;
                };
                if !keep_local(name, &raw, discard) {
                    continue;
                }
                let live = match symbols.section(index, &raw) {
                    Ok(SectionIndex::Section(section)) => {
                        refs.sections.is_present_in(file_index, section)
                    }
                    Ok(SectionIndex::Absolute) => true,
                    _ => false,
                };
                if !live && raw.kind() != STT_FILE {
                    continue;
                }
                kept.push(u32::try_from(index).unwrap_or(u32::MAX));
                names = names.saturating_add(name.len()).saturating_add(1);
            }
            (kept, names)
        })
        .collect();

    let symbols = refs.symbols;
    let mut selected: Vec<(SymbolId, bool, usize)> = (0..symbols.len())
        .into_par_iter()
        .filter_map(|index| {
            let id = SymbolId::new(index);
            let kind = symbols.definition_kind(id);
            let emit = match kind {
                DefinitionKind::Regular | DefinitionKind::Weak | DefinitionKind::Common => {
                    let target = refs.global_target(id, true);
                    match target.def {
                        Def::Section { file, section, .. } => {
                            refs.sections.is_present_in(file, section)
                        }
                        _ => true,
                    }
                }
                DefinitionKind::Shared => symbols.flags(id).contains(REF_REGULAR),
                DefinitionKind::Undefined | DefinitionKind::Lazy => {
                    let flags = symbols.flags(id);
                    (flags.contains(SymbolFlags::WEAK_REFERENCED)
                        && !flags.contains(SymbolFlags::REFERENCED))
                        || (flags.contains(REF_REGULAR) && flags.contains(PREEMPTIBLE))
                }
            };
            if !emit {
                return None;
            }
            let visibility = global_visibility(refs, linker, id);
            let hidden = matches!(visibility, STV_HIDDEN | STV_INTERNAL)
                && kind != DefinitionKind::Undefined
                && kind != DefinitionKind::Lazy;
            if hidden && discard == DiscardMode::All {
                return None;
            }
            let len = name_len(symbols.name(id)).saturating_add(1);
            Some((id, hidden, len))
        })
        .collect();
    selected.sort_unstable_by_key(|(id, ..)| *id);

    let mut plan = SymtabPlan::default();
    let mut index = 1usize;
    let mut offset = 1usize;
    for (kept, names) in per_file {
        plan.local_base.push(index);
        plan.local_names.push(offset);
        index = index.saturating_add(kept.len());
        offset = offset.saturating_add(names);
        plan.locals.push(kept);
    }
    plan.hidden_base = index;
    plan.hidden_names = offset;
    for &(id, hidden, len) in &selected {
        if hidden {
            plan.hidden.push(id);
            index = index.saturating_add(1);
            offset = offset.saturating_add(len);
        }
    }
    plan.first_global = index;
    plan.global_names = offset;
    for &(id, hidden, len) in &selected {
        if !hidden {
            plan.globals.push(id);
            index = index.saturating_add(1);
            offset = offset.saturating_add(len);
        }
    }
    plan.count = index;
    plan.strtab_size = offset;
    plan
}

impl SymtabPlan {
    /// `.symtab` size in bytes.
    #[must_use]
    pub fn symtab_size(&self) -> u64 {
        u64::try_from(self.count.saturating_mul(SYM_SIZE)).unwrap_or(u64::MAX)
    }

    /// Whether the table is written at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
}

/// The length of a symbol's name in `.strtab`: `name@version` for
/// versioned symbols.
fn name_len(name: crate::symbols::SymbolName<'_>) -> usize {
    match name.version() {
        Some(version) => name
            .bytes()
            .len()
            .saturating_add(1)
            .saturating_add(version.len()),
        None => name.bytes().len(),
    }
}

/// In executables and shared objects, a TLS symbol's value is its offset in
/// the TLS template.
fn tls_relative(addresses: &Addresses<'_, '_>, kind: u8, value: u64, shndx: u16) -> u64 {
    if kind != STT_TLS || shndx == SHN_ABS || shndx == SHN_UNDEF {
        return value;
    }
    value.wrapping_sub(addresses.layout.tls.map_or(0, |t| t.start))
}

fn put_sym(out: &mut [u8], name: usize, info: u8, other: u8, shndx: u16, value: u64, size: u64) {
    let Some(entry) = out.first_chunk_mut::<SYM_SIZE>() else {
        return;
    };
    let name = u32::try_from(name).unwrap_or(0);
    entry[0..4].copy_from_slice(&name.to_le_bytes());
    entry[4] = info;
    entry[5] = other;
    entry[6..8].copy_from_slice(&shndx.to_le_bytes());
    entry[8..16].copy_from_slice(&value.to_le_bytes());
    entry[16..24].copy_from_slice(&size.to_le_bytes());
}

fn shndx_for(addresses: &Addresses<'_, '_>, file: usize, section: u32) -> u16 {
    let Some(id) = addresses
        .refs
        .sections
        .id(file, section)
        .and_then(|id| addresses.refs.sections.resolve(id))
    else {
        return SHN_UNDEF;
    };
    let index = addresses
        .layout
        .section_shndx
        .get(id.index())
        .copied()
        .unwrap_or(0);
    if index == super::layout::EMPTY_SHNDX {
        return SHN_ABS;
    }
    u16::try_from(index).unwrap_or(crate::elf::read::consts::SHN_XINDEX)
}

/// Writes `.symtab` into `out`.
pub fn write_symtab(
    plan: &SymtabPlan,
    addresses: &Addresses<'_, '_>,
    linker: &LinkerSymbols,
    out: &mut [u8],
) {
    let refs = &addresses.refs;
    let (entries, _) = out.as_chunks_mut::<SYM_SIZE>();
    let (_, rest) = entries.split_at_mut(1.min(entries.len()));
    let local_count = plan.hidden_base.saturating_sub(1);
    let (locals, rest) = rest.split_at_mut(local_count.min(rest.len()));
    let (hidden, globals) = rest.split_at_mut(plan.hidden.len().min(rest.len()));

    // File locals, per file in parallel.
    let mut slices = Vec::with_capacity(plan.locals.len());
    let mut remaining = locals;
    for kept in &plan.locals {
        let n = kept.len().min(remaining.len());
        let (head, tail) = std::mem::take(&mut remaining).split_at_mut(n);
        slices.push(head);
        remaining = tail;
    }
    slices
        .into_par_iter()
        .enumerate()
        .for_each(|(file_index, slice)| {
            let Some(object) = refs.files.get(file_index).and_then(|f| f.object.as_ref()) else {
                return;
            };
            let symbols = object.elf.symbols();
            let mut name_offset = plan.local_names.get(file_index).copied().unwrap_or(0);
            let kept = plan.locals.get(file_index).map_or(&[][..], Vec::as_slice);
            for (entry, &index) in slice.iter_mut().zip(kept) {
                let index = index as usize;
                let Some(raw) = symbols.get_raw(index) else {
                    continue;
                };
                let name_len = symbols.name(index, &raw).map_or(0, <[u8]>::len);
                let (shndx, value) = match symbols.section(index, &raw) {
                    Ok(SectionIndex::Section(section)) => (
                        shndx_for(addresses, file_index, section),
                        addresses
                            .section_offset_address(file_index, section, raw.st_value)
                            .unwrap_or(0),
                    ),
                    _ => (SHN_ABS, raw.st_value),
                };
                let value = tls_relative(addresses, raw.kind(), value, shndx);
                put_sym(
                    entry,
                    name_offset,
                    raw.st_info,
                    raw.st_other,
                    shndx,
                    value,
                    raw.st_size,
                );
                name_offset = name_offset.saturating_add(name_len).saturating_add(1);
            }
        });

    let global_entry =
        |id: SymbolId, local: bool, name_offset: usize, entry: &mut [u8; SYM_SIZE]| {
            let target = refs.global_target(id, true);
            let value = addresses.globals.get(id.index()).copied().unwrap_or(0);
            let (binding, kind, other, shndx, size) = match target.def {
                Def::Section { file, section, .. } => {
                    let raw = target.raw.unwrap_or_default();
                    (
                        raw.binding(),
                        raw.kind(),
                        raw.st_other,
                        shndx_for(addresses, file, section),
                        raw.st_size,
                    )
                }
                Def::Absolute(_) => {
                    let raw = target.raw.unwrap_or_default();
                    (
                        raw.binding(),
                        raw.kind(),
                        raw.st_other,
                        SHN_ABS,
                        raw.st_size,
                    )
                }
                Def::Common(_) => {
                    let def = refs.symbols.definition(id);
                    (
                        STB_GLOBAL,
                        STT_OBJECT,
                        STV_DEFAULT,
                        shndx_of_address(addresses, value),
                        def.aux & !super::resolve::AUX_COMDAT,
                    )
                }
                Def::Linker(_) => {
                    let visibility = global_visibility(refs, linker, id);
                    let absolute = refs.symbols.definition(id).file.index() == 0;
                    (
                        STB_GLOBAL,
                        STT_NOTYPE,
                        visibility,
                        if absolute {
                            SHN_ABS
                        } else {
                            shndx_of_address(addresses, value)
                        },
                        0,
                    )
                }
                Def::Shared(_) => {
                    let raw = target.raw.unwrap_or_default();
                    if addresses.synth.copy_of(id).is_some() {
                        (
                            STB_GLOBAL,
                            raw.kind(),
                            STV_DEFAULT,
                            shndx_of_address(addresses, value),
                            raw.st_size,
                        )
                    } else {
                        (
                            import_binding(refs, id),
                            raw.kind(),
                            STV_DEFAULT,
                            SHN_UNDEF,
                            0,
                        )
                    }
                }
                Def::Undefined { .. } => (
                    import_binding(refs, id),
                    STT_NOTYPE,
                    STV_DEFAULT,
                    SHN_UNDEF,
                    0,
                ),
            };
            let binding = if local { STB_LOCAL } else { binding };
            let value = tls_relative(addresses, kind, value, shndx);
            put_sym(
                entry,
                name_offset,
                (binding << 4) | (kind & 0xf),
                other,
                shndx,
                value,
                size,
            );
        };

    let mut offsets = Vec::with_capacity(plan.hidden.len());
    let mut offset = plan.hidden_names;
    for &id in &plan.hidden {
        offsets.push(offset);
        offset = offset
            .saturating_add(name_len(refs.symbols.name(id)))
            .saturating_add(1);
    }
    hidden
        .par_iter_mut()
        .zip(plan.hidden.par_iter())
        .zip(offsets.par_iter())
        .for_each(|((entry, &id), &name)| global_entry(id, true, name, entry));

    let mut offsets = Vec::with_capacity(plan.globals.len());
    let mut offset = plan.global_names;
    for &id in &plan.globals {
        offsets.push(offset);
        offset = offset
            .saturating_add(name_len(refs.symbols.name(id)))
            .saturating_add(1);
    }
    globals
        .par_iter_mut()
        .zip(plan.globals.par_iter())
        .zip(offsets.par_iter())
        .for_each(|((entry, &id), &name)| global_entry(id, false, name, entry));
}

/// Writes `.strtab` into `out`.
pub fn write_strtab(plan: &SymtabPlan, refs: &Refs<'_, '_>, out: &mut [u8]) {
    let mut cursor = 1usize;
    for (file_index, kept) in plan.locals.iter().enumerate() {
        let Some(object) = refs.files.get(file_index).and_then(|f| f.object.as_ref()) else {
            continue;
        };
        let symbols = object.elf.symbols();
        for &index in kept {
            let name = symbols
                .get_raw(index as usize)
                .and_then(|raw| symbols.name(index as usize, &raw).ok())
                .unwrap_or_default();
            cursor = put_name(out, cursor, name);
        }
    }
    for &id in plan.hidden.iter().chain(&plan.globals) {
        let name = refs.symbols.name(id);
        match name.version() {
            Some(version) => {
                let end = cursor.saturating_add(name.bytes().len());
                if let Some(dest) = out.get_mut(cursor..end) {
                    dest.copy_from_slice(name.bytes());
                }
                if let Some(at) = out.get_mut(end) {
                    *at = b'@';
                }
                cursor = put_name(out, end.saturating_add(1), version);
            }
            None => cursor = put_name(out, cursor, name.bytes()),
        }
    }
}

/// The binding of an undefined or imported symbol: weak when every
/// reference from a regular object is weak.
fn import_binding(refs: &Refs<'_, '_>, id: SymbolId) -> u8 {
    if refs.symbols.flags(id).contains(REF_REGULAR_STRONG) {
        STB_GLOBAL
    } else {
        STB_WEAK
    }
}

fn put_name(out: &mut [u8], at: usize, name: &[u8]) -> usize {
    let end = at.saturating_add(name.len());
    if let Some(dest) = out.get_mut(at..end) {
        dest.copy_from_slice(name);
    }
    end.saturating_add(1)
}
