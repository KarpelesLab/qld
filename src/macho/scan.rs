//! The relocation scan: which symbols need stubs, `__got` slots and
//! thread-local pointer slots, which are imported, and which dylibs are
//! used.

#![deny(clippy::arithmetic_side_effects)]

use rayon::prelude::*;

use crate::args::darwin::LoadMode;
use crate::error::Result;
use crate::ids::SymbolId;
use crate::macho::read::compact_unwind_entries;
use crate::macho::read::consts::{
    BIND_SPECIAL_DYLIB_FLAT_LOOKUP, BIND_SPECIAL_DYLIB_MAIN_EXECUTABLE,
    BIND_SPECIAL_DYLIB_WEAK_LOOKUP,
};

use super::layout::is_consumed;
use super::reloc::{self, Place, Referent};
use super::state::{Link, NONE, SymbolDef};
use super::symtab::ExportFilter;

/// One imported symbol.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Import {
    /// The symbol.
    pub symbol: SymbolId,
    /// Its name.
    pub name: Vec<u8>,
    /// Library ordinal (1-based), or a negative special ordinal.
    pub ordinal: i32,
    /// Weak import: may be missing at run time.
    pub weak: bool,
}

/// The synthetic tables.
#[derive(Clone, Debug, Default)]
pub struct Synthetic {
    /// Symbols with stubs, in stub order.
    pub stubs: Vec<SymbolId>,
    /// Symbols with `__got` slots, in slot order.
    pub got: Vec<SymbolId>,
    /// Symbols with `__thread_ptrs` slots, in slot order.
    pub thread_ptrs: Vec<SymbolId>,
    /// Stub index of each symbol, or [`NONE`].
    pub stub_index: Vec<u32>,
    /// `__got` index of each symbol, or [`NONE`].
    pub got_index: Vec<u32>,
    /// `__thread_ptrs` index of each symbol, or [`NONE`].
    pub tlv_index: Vec<u32>,
    /// Common symbols, in `__common` order.
    pub commons: Vec<SymbolId>,
    /// Imports.
    pub imports: Vec<Import>,
    /// Import index of each symbol, or [`NONE`].
    pub import_index: Vec<u32>,
    /// Ordinal of each dylib (0 when no load command is written for it).
    pub dylib_ordinals: Vec<i32>,
    /// The dylibs that get load commands, in ordinal order.
    pub loaded_dylibs: Vec<usize>,
    /// Whether every import from each dylib is weak.
    pub dylib_all_weak: Vec<bool>,
    /// Exported weak definitions bound through dyld's weak lookup, by
    /// symbol.
    pub weak_bound: Vec<bool>,
    /// `__got` slots of local symbols (file, symbol table index), which
    /// follow the global symbols' slots: a pointer-to-GOT relocation
    /// cannot be relaxed (an LSDA type table naming a local type info).
    pub local_got: Vec<(u32, u32)>,
    /// Index into [`Synthetic::local_got`] of each local symbol with a slot.
    pub local_got_index: hashbrown::HashMap<(u32, u32), u32>,
}

impl Synthetic {
    /// Number of `__got` slots, globals and locals.
    #[must_use]
    pub fn got_slots(&self) -> usize {
        self.got.len().saturating_add(self.local_got.len())
    }
}

#[derive(Clone, Copy, Default)]
struct Wants {
    got: bool,
    stub: bool,
    tlv: bool,
    used: bool,
}

/// Scans the live atoms' relocations.
///
/// # Errors
///
/// Malformed relocations.
pub fn scan(
    link: &Link<'_>,
    options: &crate::args::LinkOptions,
    filter: &ExportFilter,
) -> Result<Synthetic> {
    let count = link.symbols.len();
    let arm64 = link.config.is_arm64();
    let weak_bound = weak_bound(link, filter);
    type FileWants = (Vec<(SymbolId, Wants)>, Vec<u32>);
    let per_file: Vec<Result<FileWants>> = (0..link.files.len())
        .into_par_iter()
        .map(|file| {
            let mut out = Vec::new();
            let mut local_slots = Vec::new();
            let Some(object) = link.object(file) else {
                return Ok((out, local_slots));
            };
            for (section_index, relocations) in object.relocations.iter().enumerate() {
                let Some(section) = object.file.sections().get(section_index) else {
                    continue;
                };
                let data = object.file.section_data(section_index)?;
                if section.is(b"__TEXT", b"__eh_frame") {
                    // CIE personalities are reached through `__got`.
                    for relocation in relocations {
                        let decoded = reloc::decode(
                            link,
                            file,
                            object,
                            section_index,
                            data,
                            &relocation.relocation,
                        )?;
                        // A dead-stripped personality belongs only to CIEs
                        // that no kept FDE uses.
                        if let Referent::Global(id) = decoded.referent
                            && reloc::needs(arm64, decoded.r_type).pointer
                            && link
                                .symbol_atom(id)
                                .is_none_or(|atom| link.live.get(atom).copied().unwrap_or(false))
                        {
                            out.push((
                                id,
                                Wants {
                                    got: true,
                                    used: true,
                                    ..Wants::default()
                                },
                            ));
                        }
                    }
                    continue;
                }
                if is_consumed(section.segname, section.sectname, section.flags) {
                    continue;
                }
                for relocation in relocations {
                    if !link.is_live(file, relocation.atom) {
                        continue;
                    }
                    let decoded = reloc::decode(
                        link,
                        file,
                        object,
                        section_index,
                        data,
                        &relocation.relocation,
                    )?;
                    for referent in [Some(decoded.referent), decoded.subtrahend]
                        .into_iter()
                        .flatten()
                    {
                        let needs = reloc::needs(arm64, decoded.r_type);
                        let id = match referent {
                            Referent::Global(id) => id,
                            Referent::Local(symbol) if needs.pointer => {
                                local_slots.push(symbol);
                                continue;
                            }
                            _ => continue,
                        };
                        let imported = link.is_imported(id)
                            || weak_bound.get(id.index()).copied().unwrap_or(false);
                        out.push((
                            id,
                            Wants {
                                got: needs.got && (needs.pointer || imported),
                                tlv: needs.tlv && imported,
                                stub: needs.stub && imported,
                                used: true,
                            },
                        ));
                    }
                }
            }
            // Personalities of the live functions' compact unwind entries
            // are reached through `__got`.
            for entry in compact_unwind_entries(&object.file)? {
                let Some(function) = entry.function.relocation else {
                    continue;
                };
                let Some(personality) = entry.personality.relocation else {
                    continue;
                };
                let Some((section_index, _)) =
                    object.file.find_section(b"__LD", b"__compact_unwind")
                else {
                    continue;
                };
                let data = object.file.section_data(section_index)?;
                let function = reloc::decode(link, file, object, section_index, data, &function)?;
                let live =
                    match reloc::place(link, file, object, function.referent, function.addend)? {
                        Place::Atom { atom, .. } => link.live.get(atom).copied().unwrap_or(false),
                        _ => false,
                    };
                if !live {
                    continue;
                }
                let personality =
                    reloc::decode(link, file, object, section_index, data, &personality)?;
                if let Referent::Global(id) = personality.referent {
                    out.push((
                        id,
                        Wants {
                            got: true,
                            used: true,
                            ..Wants::default()
                        },
                    ));
                }
            }
            Ok((out, local_slots))
        })
        .collect();

    let mut wants = vec![Wants::default(); count];
    let mut local_got: Vec<(u32, u32)> = Vec::new();
    let mut local_got_index = hashbrown::HashMap::new();
    for (file, result) in per_file.into_iter().enumerate() {
        let (list, local_slots) = result?;
        for symbol in local_slots {
            let key = (u32::try_from(file).unwrap_or(NONE), symbol);
            if let hashbrown::hash_map::Entry::Vacant(entry) = local_got_index.entry(key) {
                entry.insert(u32::try_from(local_got.len()).unwrap_or(NONE));
                local_got.push(key);
            }
        }
        for (id, want) in list {
            if let Some(slot) = wants.get_mut(id.index()) {
                slot.got |= want.got;
                slot.stub |= want.stub;
                slot.tlv |= want.tlv;
                slot.used |= want.used;
            }
        }
    }

    let mut synthetic = Synthetic {
        stub_index: vec![NONE; count],
        got_index: vec![NONE; count],
        tlv_index: vec![NONE; count],
        import_index: vec![NONE; count],
        local_got,
        local_got_index,
        ..Synthetic::default()
    };
    let to_u32 = |n: usize| u32::try_from(n).unwrap_or(NONE);
    for (index, want) in wants.iter().enumerate() {
        let id = SymbolId::new(index);
        let def = link.defs.get(index);
        if want.stub {
            synthetic.stub_index[index] = to_u32(synthetic.stubs.len());
            synthetic.stubs.push(id);
        }
        // A thread-local variable reached through TLVP needs its pointer
        // slot; everything else through GOT.
        if want.tlv {
            synthetic.tlv_index[index] = to_u32(synthetic.thread_ptrs.len());
            synthetic.thread_ptrs.push(id);
        }
        // Stubs load their target from `__got`.
        if want.got || want.stub {
            synthetic.got_index[index] = to_u32(synthetic.got.len());
            synthetic.got.push(id);
        }
        if matches!(def, Some(SymbolDef::Common { .. })) {
            synthetic.commons.push(id);
        }
    }

    // Dylib ordinals: every dylib linked explicitly keeps its load command
    // unless -dead_strip_dylibs drops it for being unused.
    let dylib_count = link.dylibs.len();
    let mut used = vec![false; dylib_count];
    let mut strong = vec![false; dylib_count];
    for (index, want) in wants.iter().enumerate() {
        if !want.used {
            continue;
        }
        if let Some(SymbolDef::Dylib { dylib, .. }) = link.defs.get(index) {
            let dylib = usize::try_from(*dylib).unwrap_or(usize::MAX);
            if let Some(slot) = used.get_mut(dylib) {
                *slot = true;
            }
            if link.strong_ref.get(index).copied().unwrap_or(false)
                && let Some(slot) = strong.get_mut(dylib)
            {
                *slot = true;
            }
        }
    }
    synthetic.dylib_ordinals = vec![0; dylib_count];
    synthetic.dylib_all_weak = vec![false; dylib_count];
    let mut next = 1i32;
    for (index, dylib) in link.dylibs.iter().enumerate() {
        // The executable a bundle is loaded into gets no load command:
        // imports from it use the main-executable ordinal.
        if dylib.bundle_loader {
            continue;
        }
        let keep = dylib.mode == LoadMode::Needed
            || used.get(index).copied().unwrap_or(false)
            || (!options.darwin.dead_strip_dylibs && !dylib.implicit);
        if !keep {
            continue;
        }
        synthetic.dylib_ordinals[index] = next;
        synthetic.dylib_all_weak[index] = used.get(index).copied().unwrap_or(false)
            && !strong.get(index).copied().unwrap_or(false);
        synthetic.loaded_dylibs.push(index);
        next = next.saturating_add(1);
    }

    // Imports, in symbol order.
    for (index, want) in wants.iter().enumerate() {
        if !want.used {
            continue;
        }
        let id = SymbolId::new(index);
        let (ordinal, weak) = match link.defs.get(index) {
            Some(SymbolDef::Dylib { dylib, .. }) => {
                let dylib = usize::try_from(*dylib).unwrap_or(usize::MAX);
                let ordinal = if link.config.flat_namespace {
                    BIND_SPECIAL_DYLIB_FLAT_LOOKUP
                } else if link.dylibs.get(dylib).is_some_and(|d| d.bundle_loader) {
                    BIND_SPECIAL_DYLIB_MAIN_EXECUTABLE
                } else {
                    synthetic.dylib_ordinals.get(dylib).copied().unwrap_or(0)
                };
                let weak_dylib = link
                    .dylibs
                    .get(dylib)
                    .is_some_and(|d| d.mode == LoadMode::Weak);
                let weak_ref = !link.strong_ref.get(index).copied().unwrap_or(true);
                (ordinal, weak_dylib || weak_ref)
            }
            Some(SymbolDef::DynamicLookup) => (
                BIND_SPECIAL_DYLIB_FLAT_LOOKUP,
                !link.strong_ref.get(index).copied().unwrap_or(true),
            ),
            Some(SymbolDef::Object { .. }) if weak_bound.get(index).copied().unwrap_or(false) => {
                (BIND_SPECIAL_DYLIB_WEAK_LOOKUP, false)
            }
            _ => continue,
        };
        synthetic.import_index[index] = to_u32(synthetic.imports.len());
        synthetic.imports.push(Import {
            symbol: id,
            name: link.symbols.name(id).bytes().to_vec(),
            ordinal,
            weak,
        });
    }
    synthetic.weak_bound = weak_bound;
    Ok(synthetic)
}

/// The exported weak definitions, which references bind through dyld's
/// weak lookup so that one definition wins across images (as in ld64 and
/// lld). Chained fixups bind them to a weak-lookup import; the legacy
/// form rebases them to the local definition and lists them in the weak
/// binding stream ([`fixups::opcodes`](super::fixups::opcodes)).
fn weak_bound(link: &Link<'_>, filter: &ExportFilter) -> Vec<bool> {
    let mut out = vec![false; link.symbols.len()];
    for (index, slot) in out.iter_mut().enumerate() {
        let Some(SymbolDef::Object { file, symbol }) = link.defs.get(index) else {
            continue;
        };
        let file = usize::try_from(*file).unwrap_or(usize::MAX);
        let Some(object) = link.object(file) else {
            continue;
        };
        let Ok(entry) = object.file.symbols().get(*symbol) else {
            continue;
        };
        *slot = entry.is_weak_def()
            && !link.is_hidden(SymbolId::new(index), file, &entry)
            && filter.exports(entry.name);
    }
    out
}
