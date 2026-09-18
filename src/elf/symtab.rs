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
    /// String table offset of each name of `hidden`.
    hidden_offsets: Vec<usize>,
    /// String table offset of each name of `globals`.
    global_offsets: Vec<usize>,
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
    /// Section symbols at the start of the table (`--emit-relocs`).
    pub section_symbols: usize,
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

/// Which local symbols of `file` the relocations of its live sections
/// name, by symbol index.
fn referenced_locals(
    refs: &Refs<'_, '_>,
    file_index: usize,
    object: &super::object::ObjectInput<'_>,
) -> Vec<bool> {
    let mut referenced = vec![false; object.first_global];
    for (index, section) in object.sections.iter().enumerate() {
        let Ok(index) = u32::try_from(index) else {
            break;
        };
        if section.relocs == 0 || !refs.sections.is_live_in(file_index, index) {
            continue;
        }
        let Some(Ok(Some(relocations))) = object
            .section(section.relocs)
            .map(|r| object.elf.relocation_section(section.relocs, &r.header))
        else {
            continue;
        };
        if let crate::elf::read::Relocations::Rela(relas) = relocations.relocations {
            for rel in relas.iter() {
                if let Some(slot) = referenced.get_mut(rel.symbol as usize) {
                    *slot = true;
                }
            }
        }
    }
    referenced
}

/// Whether an import survives `--gc-sections`: live code refers to it, or
/// it needs a GOT, PLT or copy (from a relocation of live code).
fn live_import(flags: SymbolFlags) -> bool {
    flags.contains(super::scan::REF_LIVE)
        || flags.intersects(
            SymbolFlags::NEEDS_GOT
                | SymbolFlags::NEEDS_PLT
                | SymbolFlags::NEEDS_COPY_RELOC
                | SymbolFlags::NEEDS_DYNSYM,
        )
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
            // With --emit-relocs, locals that relocations name stay symbols.
            let referenced = if options.emit_relocs {
                referenced_locals(refs, file_index, object)
            } else {
                Vec::new()
            };
            for index in 1..object.first_global {
                let Some(raw) = symbols.get_raw(index) else {
                    break;
                };
                let Ok(name) = symbols.name(index, &raw) else {
                    continue;
                };
                let wanted = keep_local(name, &raw, discard)
                    || (referenced.get(index).copied().unwrap_or(false)
                        && raw.kind() != STT_SECTION
                        && !name.is_empty());
                if !wanted {
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
                DefinitionKind::Shared => {
                    let flags = symbols.flags(id);
                    flags.contains(REF_REGULAR) && (!options.gc_sections || live_import(flags))
                }
                DefinitionKind::Undefined | DefinitionKind::Lazy => {
                    let flags = symbols.flags(id);
                    ((flags.contains(SymbolFlags::WEAK_REFERENCED)
                        && !flags.contains(SymbolFlags::REFERENCED))
                        || (flags.contains(REF_REGULAR) && flags.contains(PREEMPTIBLE)))
                        && (!options.gc_sections || live_import(flags))
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
            let len = global_name_len(refs, id).saturating_add(1);
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
            plan.hidden_offsets.push(offset);
            index = index.saturating_add(1);
            offset = offset.saturating_add(len);
        }
    }
    plan.first_global = index;
    plan.global_names = offset;
    for &(id, hidden, len) in &selected {
        if !hidden {
            plan.globals.push(id);
            plan.global_offsets.push(offset);
            index = index.saturating_add(1);
            offset = offset.saturating_add(len);
        }
    }
    plan.count = index;
    plan.strtab_size = offset;
    plan
}

impl SymtabPlan {
    /// Puts `count` section symbols (one per output section, in header
    /// order) after the null symbol, moving every other symbol up.
    pub fn add_section_symbols(&mut self, count: usize) {
        if self.is_empty() || count == 0 {
            return;
        }
        for base in &mut self.local_base {
            *base = base.saturating_add(count);
        }
        self.hidden_base = self.hidden_base.saturating_add(count);
        self.first_global = self.first_global.saturating_add(count);
        self.count = self.count.saturating_add(count);
        self.section_symbols = self.section_symbols.saturating_add(count);
    }

    /// The table index of local symbol `symbol` of `file`, if it is kept.
    #[must_use]
    pub fn local_index(&self, file: usize, symbol: u32) -> Option<usize> {
        let kept = self.locals.get(file)?;
        let at = kept.binary_search(&symbol).ok()?;
        self.local_base.get(file)?.checked_add(at)
    }

    /// The table index of global symbol `id`, if it is in the table.
    #[must_use]
    pub fn global_index(&self, id: SymbolId) -> Option<usize> {
        if let Ok(at) = self.hidden.binary_search(&id) {
            return self.hidden_base.checked_add(at);
        }
        let at = self.globals.binary_search(&id).ok()?;
        self.first_global.checked_add(at)
    }

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
/// The version a symbol imported from a shared library is written with
/// (`name@VERSION`, as GNU ld does), unless its name already has one.
fn import_suffix<'a>(refs: &Refs<'_, 'a>, id: SymbolId) -> Option<&'a [u8]> {
    if refs.symbols.definition_kind(id) != DefinitionKind::Shared
        || refs.symbols.name(id).version().is_some()
    {
        return None;
    }
    super::dynsym::import_version(refs, id).map(|(_, version)| version)
}

/// The length of a global symbol's name in `.strtab`.
fn global_name_len(refs: &Refs<'_, '_>, id: SymbolId) -> usize {
    let len = name_len(refs.symbols.name(id));
    match import_suffix(refs, id) {
        Some(version) => len.saturating_add(1).saturating_add(version.len()),
        None => len,
    }
}

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

/// The output section header index of input section `section` of `file`
/// (after ICF folding): `SHN_ABS` when its output section was dropped as
/// empty, `SHN_UNDEF` when it is not in the output.
pub(crate) fn shndx_for(addresses: &Addresses<'_, '_>, file: usize, section: u32) -> u16 {
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
    let (head, rest) =
        entries.split_at_mut(plan.section_symbols.saturating_add(1).min(entries.len()));
    for (position, (entry, section)) in head
        .iter_mut()
        .skip(1)
        .zip(&addresses.layout.sections)
        .enumerate()
    {
        let shndx = u16::try_from(position.saturating_add(1))
            .unwrap_or(crate::elf::read::consts::SHN_XINDEX);
        put_sym(
            entry,
            0,
            (STB_LOCAL << 4) | STT_SECTION,
            0,
            shndx,
            section.addr,
            0,
        );
    }
    let local_count = plan
        .hidden_base
        .saturating_sub(1)
        .saturating_sub(plan.section_symbols);
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
                let (shndx, value, size) = match symbols.section(index, &raw) {
                    Ok(SectionIndex::Section(section)) => (
                        shndx_for(addresses, file_index, section),
                        addresses
                            .section_offset_address(file_index, section, raw.st_value)
                            .unwrap_or(0),
                        addresses.symbol_size(file_index, section, raw.st_value, raw.st_size),
                    ),
                    _ => (SHN_ABS, raw.st_value, raw.st_size),
                };
                let value = tls_relative(addresses, raw.kind(), value, shndx);
                put_sym(
                    entry,
                    name_offset,
                    raw.st_info,
                    raw.st_other,
                    shndx,
                    value,
                    size,
                );
                name_offset = name_offset.saturating_add(name_len).saturating_add(1);
            }
        });

    let global_entry =
        |id: SymbolId, local: bool, name_offset: usize, entry: &mut [u8; SYM_SIZE]| {
            let target = refs.global_target(id, true);
            let value = addresses.globals.get(id.index()).copied().unwrap_or(0);
            let (binding, kind, other, shndx, size) = match target.def {
                Def::Section {
                    file,
                    section,
                    value,
                } => {
                    let raw = target.raw.unwrap_or_default();
                    (
                        raw.binding(),
                        raw.kind(),
                        raw.st_other,
                        shndx_for(addresses, file, section),
                        addresses.symbol_size(file, section, value, raw.st_size),
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
                    let absolute = refs.symbols.flags(id).contains(super::defined::ABSOLUTE);
                    (
                        STB_GLOBAL,
                        STT_NOTYPE,
                        visibility,
                        if absolute {
                            SHN_ABS
                        } else {
                            super::defined::linker_shndx(addresses, linker, id)
                                .unwrap_or_else(|| shndx_of_address(addresses, value))
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

    // Name offsets were computed by the plan, from the same lengths.
    hidden
        .par_iter_mut()
        .zip(plan.hidden.par_iter())
        .zip(plan.hidden_offsets.par_iter())
        .for_each(|((entry, &id), &name)| global_entry(id, true, name, entry));
    globals
        .par_iter_mut()
        .zip(plan.globals.par_iter())
        .zip(plan.global_offsets.par_iter())
        .for_each(|((entry, &id), &name)| global_entry(id, false, name, entry));
}

/// Writes `.strtab` into `out`: each file's local names, then the hidden
/// and global names, from the offsets the plan computed, in parallel.
pub fn write_strtab(plan: &SymtabPlan, refs: &Refs<'_, '_>, out: &mut [u8]) {
    /// Globals whose names one task writes.
    const NAMES_PER_TASK: usize = 4096;
    enum Task<'p> {
        /// Local names of a file.
        Locals(usize),
        /// Consecutive hidden or global names.
        Globals(&'p [SymbolId]),
    }
    // Every task writes into its own slice of `out`, which starts at its
    // first name's offset; the plan's offsets increase in this order.
    let mut starts: Vec<(usize, Task<'_>)> = Vec::new();
    for (file_index, &start) in plan.local_names.iter().enumerate() {
        starts.push((start, Task::Locals(file_index)));
    }
    for (ids, offsets) in [
        (&plan.hidden, &plan.hidden_offsets),
        (&plan.globals, &plan.global_offsets),
    ] {
        for (chunk, chunk_offsets) in ids
            .chunks(NAMES_PER_TASK)
            .zip(offsets.chunks(NAMES_PER_TASK))
        {
            if let Some(&start) = chunk_offsets.first() {
                starts.push((start, Task::Globals(chunk)));
            }
        }
    }
    let mut tasks: Vec<(Task<'_>, &mut [u8])> = Vec::with_capacity(starts.len());
    let mut rest: &mut [u8] = out;
    let mut consumed = 0usize;
    let mut starts = starts.into_iter().peekable();
    while let Some((start, task)) = starts.next() {
        let end = starts
            .peek()
            .map_or(consumed.saturating_add(rest.len()), |&(next, _)| next);
        // Skip up to `start` (the null byte, or nothing), then take this
        // task's bytes; out-of-range offsets give empty slices.
        let skip = start.saturating_sub(consumed).min(rest.len());
        let (_, tail) = std::mem::take(&mut rest).split_at_mut(skip);
        let take = end.saturating_sub(start).min(tail.len());
        let (slice, tail) = tail.split_at_mut(take);
        tasks.push((task, slice));
        rest = tail;
        consumed = consumed.saturating_add(skip).saturating_add(take);
    }
    tasks.into_par_iter().for_each(|(task, slice)| match task {
        Task::Locals(file_index) => {
            let (Some(object), Some(kept)) = (
                refs.files.get(file_index).and_then(|f| f.object.as_ref()),
                plan.locals.get(file_index),
            ) else {
                return;
            };
            let symbols = object.elf.symbols();
            let mut cursor = 0usize;
            for &index in kept {
                let name = symbols
                    .get_raw(index as usize)
                    .and_then(|raw| symbols.name(index as usize, &raw).ok())
                    .unwrap_or_default();
                cursor = put_name(slice, cursor, name);
            }
        }
        Task::Globals(ids) => {
            let mut cursor = 0usize;
            for &id in ids {
                let name = refs.symbols.name(id);
                let version = name.version().or_else(|| import_suffix(refs, id));
                cursor = match version {
                    Some(version) => {
                        let end = cursor.saturating_add(name.bytes().len());
                        if let Some(dest) = slice.get_mut(cursor..end) {
                            dest.copy_from_slice(name.bytes());
                        }
                        if let Some(at) = slice.get_mut(end) {
                            *at = b'@';
                        }
                        put_name(slice, end.saturating_add(1), version)
                    }
                    None => put_name(slice, cursor, name.bytes()),
                };
            }
        }
    });
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
