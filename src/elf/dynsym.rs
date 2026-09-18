//! The dynamic symbol table and everything built from it: `.dynsym`,
//! `.dynstr`, `.gnu.hash`/`.hash`, `.gnu.version`, `.gnu.version_r`,
//! `.gnu.version_d`, and the `.dynamic` entries.
//!
//! [`plan`] runs after the relocation scan and synthetic section planning,
//! so it knows which symbols dynamic relocations need, and fixes every size:
//! the symbol order, the string table, the version tables and the hash
//! tables are built here, since none of them depends on an address. Symbol
//! values and `.dynamic` values are written after layout ([`write_dynsym`],
//! [`write_dynamic`]).
//!
//! **Which symbols.** Imports: symbols defined by a needed shared library
//! that a regular object refers to, and undefined preemptible symbols that a
//! dynamic relocation needs (or, in a shared object, that a regular object
//! refers to). Exports: symbols [`export`](super::export) marked, copy
//! relocations, and the version definitions of a shared object.
//!
//! **Order.** Imports first, in symbol ID order, then the defined symbols.
//! With `.gnu.hash`, the defined symbols are sorted by hash bucket, as the
//! format requires; imports are not hashed.

#![deny(clippy::arithmetic_side_effects)]

use std::hash::BuildHasher;

use hashbrown::HashTable;
use rayon::prelude::*;

use crate::args::{HashStyle, LinkOptions};
use crate::elf::read::VersionKind;
use crate::elf::read::consts::{
    DF_1_GLOBAL, DF_1_INITFIRST, DF_1_INTERPOSE, DF_1_LOADFLTR, DF_1_NODEFLIB, DF_1_NODELETE,
    DF_1_NODUMP, DF_1_NOOPEN, DF_1_NOW, DF_1_ORIGIN, DF_1_PIE, DF_1_SINGLETON, DF_BIND_NOW,
    DF_ORIGIN, DF_STATIC_TLS, DF_SYMBOLIC, DF_TEXTREL, DT_AUXILIARY, DT_DEBUG, DT_FILTER, DT_FINI,
    DT_FINI_ARRAY, DT_FINI_ARRAYSZ, DT_FLAGS, DT_FLAGS_1, DT_GNU_HASH, DT_HASH, DT_INIT,
    DT_INIT_ARRAY, DT_INIT_ARRAYSZ, DT_JMPREL, DT_NEEDED, DT_NULL, DT_PLTGOT, DT_PLTREL,
    DT_PLTRELSZ, DT_PREINIT_ARRAY, DT_PREINIT_ARRAYSZ, DT_RELA, DT_RELACOUNT, DT_RELAENT,
    DT_RELASZ, DT_RELR, DT_RELRENT, DT_RELRSZ, DT_RPATH, DT_RUNPATH, DT_SONAME, DT_STRSZ,
    DT_STRTAB, DT_SYMBOLIC, DT_SYMENT, DT_SYMTAB, DT_TEXTREL, DT_VERDEF, DT_VERDEFNUM, DT_VERNEED,
    DT_VERNEEDNUM, DT_VERSYM, SHN_ABS, SHN_UNDEF, STB_GLOBAL, STB_WEAK, STT_FUNC, STT_GNU_IFUNC,
    STT_NOTYPE, STT_OBJECT, STT_TLS, STV_DEFAULT, STV_PROTECTED, VER_FLG_BASE, VER_NDX_GLOBAL,
};
use crate::error::{Error, Result};
use crate::ids::SymbolId;
use crate::symbols::{DefinitionKind, SymbolFlags};

use super::dso::{Needed, REF_REGULAR, REF_REGULAR_STRONG};
use super::export::{Exports, Mode, PREEMPTIBLE, merged_visibility};
use super::refs::{Def, LINKER_FILE, Refs};
use super::rules::Synthetic;
use super::scan::REF_LIVE;
use super::scan::ScanResult;
use super::synth::Synth;
use super::values::Addresses;

/// Size of a `.dynsym` entry.
pub const DYNSYM_SIZE: u64 = 24;
/// Size of a `.dynamic` entry.
pub const DYNAMIC_SIZE: u64 = 16;

/// One `.dynsym` entry after the null symbol.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Entry {
    /// A global symbol.
    Symbol(SymbolId),
    /// The symbol a version definition (by index) gets in a shared object.
    Version(u16),
}

/// The value of a `.dynamic` entry, resolved after layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DynValue {
    /// A constant.
    Value(u64),
    /// The address of a synthetic part.
    Address(Synthetic),
    /// The size of a synthetic part.
    Size(Synthetic),
    /// The address of an output section, by name.
    OutputAddress(&'static [u8]),
    /// The size of an output section, by name.
    OutputSize(&'static [u8]),
    /// The address of a symbol.
    Symbol(SymbolId),
}

/// The planned dynamic symbol table and `.dynamic` section.
#[derive(Debug, Default)]
pub struct DynamicPlan {
    /// Whether the output has dynamic sections at all.
    pub enabled: bool,
    /// The entries after the null symbol; entry `i` has index `i + 1`.
    pub entries: Vec<Entry>,
    /// Index in `entries` of the first hashed (defined) symbol.
    pub first_hashed: usize,
    /// `.dynsym` index of each symbol (0 when not in the table), by ID.
    index: Vec<u32>,
    /// `.dynstr` offset of each entry's name.
    pub names: Vec<u32>,
    /// `.gnu.version` of each entry, including the null entry; empty when
    /// the output has no versions.
    pub versym: Vec<u16>,
    /// `.dynstr` contents.
    pub dynstr: Vec<u8>,
    /// `.gnu.hash` contents (empty if not written).
    pub gnu_hash: Vec<u8>,
    /// `.hash` contents (empty if not written).
    pub sysv_hash: Vec<u8>,
    /// `.gnu.version_r` contents.
    pub verneed: Vec<u8>,
    /// Number of `.gnu.version_r` file entries.
    pub verneed_count: u64,
    /// `.gnu.version_d` contents.
    pub verdef: Vec<u8>,
    /// Number of `.gnu.version_d` entries.
    pub verdef_count: u64,
    /// `.dynamic` entries, `DT_NULL` included.
    pub dynamic: Vec<(i64, DynValue)>,
}

impl DynamicPlan {
    /// The `.dynsym` index of `id`, or 0.
    #[must_use]
    pub fn index_of(&self, id: SymbolId) -> u32 {
        self.index.get(id.index()).copied().unwrap_or(0)
    }

    /// The number of `.dynsym` entries, the null entry included.
    #[must_use]
    pub fn count(&self) -> u64 {
        u64::try_from(self.entries.len())
            .unwrap_or(u64::MAX)
            .saturating_add(1)
    }

    /// Sizes and alignments of the planned sections, for [`Synth`].
    #[must_use]
    pub fn sizes(&self) -> Vec<(Synthetic, u64, u64)> {
        if !self.enabled {
            return Vec::new();
        }
        let len = |v: &[u8]| u64::try_from(v.len()).unwrap_or(u64::MAX);
        vec![
            (
                Synthetic::DynSym,
                self.count().saturating_mul(DYNSYM_SIZE),
                8,
            ),
            (Synthetic::DynStr, len(&self.dynstr), 1),
            (Synthetic::GnuHash, len(&self.gnu_hash), 8),
            (Synthetic::Hash, len(&self.sysv_hash), 4),
            (
                Synthetic::VerSym,
                u64::try_from(self.versym.len())
                    .unwrap_or(u64::MAX)
                    .saturating_mul(2),
                2,
            ),
            (Synthetic::VerNeed, len(&self.verneed), 8),
            (Synthetic::VerDef, len(&self.verdef), 8),
            (
                Synthetic::Dynamic,
                u64::try_from(self.dynamic.len())
                    .unwrap_or(u64::MAX)
                    .saturating_mul(DYNAMIC_SIZE),
                8,
            ),
        ]
    }
}

/// A string table under construction, deduplicating whole strings.
struct StrTab {
    data: Vec<u8>,
    /// Offsets of the strings added one by one, keyed by their hash.
    seen: HashTable<(u64, u32)>,
    /// (hash, offset) of the strings added by [`StrTab::add_all`], sorted.
    bulk: Vec<(u64, u32)>,
    hasher: foldhash::fast::FixedState,
}

/// Strings that one parallel task of [`StrTab::add_all`] copies.
const STRINGS_PER_TASK: usize = 4096;

impl StrTab {
    fn new() -> Self {
        Self {
            data: vec![0],
            seen: HashTable::new(),
            bulk: Vec::new(),
            hasher: foldhash::fast::FixedState::with_seed(0x6479_6e73),
        }
    }

    /// The string at `offset`, without its terminator.
    fn string_at(data: &[u8], offset: u32) -> &[u8] {
        let rest = data.get(offset as usize..).unwrap_or_default();
        let end = crate::elf::read::strtab::find_nul(rest).unwrap_or(rest.len());
        rest.get(..end).unwrap_or_default()
    }

    /// The offset of `text` if it was added already.
    fn find(&self, hash: u64, text: &[u8]) -> Option<u32> {
        let data = &self.data;
        if let Some(&(_, offset)) = self.seen.find(hash, |&(h, offset)| {
            h == hash && Self::string_at(data, offset) == text
        }) {
            return Some(offset);
        }
        let start = self.bulk.partition_point(|&(h, _)| h < hash);
        self.bulk
            .get(start..)?
            .iter()
            .take_while(|&&(h, _)| h == hash)
            .find(|&&(_, offset)| Self::string_at(data, offset) == text)
            .map(|&(_, offset)| offset)
    }

    fn add(&mut self, text: &[u8]) -> Result<u32> {
        if text.is_empty() {
            return Ok(0);
        }
        let hash = self.hasher.hash_one(text);
        if let Some(offset) = self.find(hash, text) {
            return Ok(offset);
        }
        let offset = u32::try_from(self.data.len())
            .map_err(|_| Error::Limit("dynamic string table larger than 4 GiB".into()))?;
        self.data.extend_from_slice(text);
        self.data.push(0);
        self.seen.insert_unique(hash, (hash, offset), |&(h, _)| h);
        Ok(offset)
    }

    /// Adds `texts` in order and returns their offsets: the same offsets
    /// and contents as calling [`add`](Self::add) on each, in parallel.
    ///
    /// Hashing, finding strings already present, and copying run in
    /// parallel. Duplicates within `texts` are found by sorting the new
    /// strings by hash; the offsets are then assigned in order, where each
    /// string's first occurrence takes the next free offset.
    fn add_all(&mut self, texts: &[&[u8]]) -> Result<Vec<u32>> {
        let too_large = || Error::Limit("dynamic string table larger than 4 GiB".into());
        let this = &*self;
        let found: Vec<(u64, Option<u32>)> = texts
            .par_iter()
            .with_min_len(STRINGS_PER_TASK)
            .map(|&text| {
                if text.is_empty() {
                    return (0, Some(0));
                }
                let hash = this.hasher.hash_one(text);
                (hash, this.find(hash, text))
            })
            .collect();
        // New strings by (hash, position); the first of equal contents
        // leads the others.
        let mut order: Vec<u32> = (0..texts.len())
            .filter(|&i| found[i].1.is_none())
            .map(|i| u32::try_from(i).unwrap_or(u32::MAX))
            .collect();
        order.par_sort_unstable_by_key(|&i| (found[i as usize].0, i));
        let mut leader: Vec<u32> = (0..texts.len())
            .map(|i| u32::try_from(i).unwrap_or(u32::MAX))
            .collect();
        for run in order.chunk_by(|&a, &b| found[a as usize].0 == found[b as usize].0) {
            for (at, &i) in run.iter().enumerate() {
                let text = texts[i as usize];
                if let Some(&first) = run[..at]
                    .iter()
                    .find(|&&j| leader[j as usize] == j && texts[j as usize] == text)
                {
                    leader[i as usize] = first;
                }
            }
        }
        drop(order);

        let mut offsets = vec![0u32; texts.len()];
        let mut cursor = self.data.len();
        // Leaders, in order, with their offsets: what to copy.
        let mut copies: Vec<(usize, u32)> = Vec::new();
        for (i, &(_, existing)) in found.iter().enumerate() {
            offsets[i] = match existing {
                Some(offset) => offset,
                None if leader[i] as usize == i => {
                    let offset = u32::try_from(cursor).map_err(|_| too_large())?;
                    cursor = cursor
                        .checked_add(texts[i].len())
                        .and_then(|c| c.checked_add(1))
                        .ok_or_else(too_large)?;
                    copies.push((i, offset));
                    offset
                }
                None => offsets[leader[i] as usize],
            };
        }
        u32::try_from(cursor).map_err(|_| too_large())?;
        let start = self.data.len();
        self.data.resize(cursor, 0);
        {
            // Each task copies a run of consecutive strings into its own
            // slice of the new data.
            // Offsets and lengths are below `cursor`, which fits in u32,
            // so these sums cannot overflow.
            type Task<'t, 'd> = (&'t [(usize, u32)], &'d mut [u8]);
            let mut tasks: Vec<Task<'_, '_>> = Vec::new();
            let mut rest = &mut self.data[start..];
            let mut base = start;
            for chunk in copies.chunks(STRINGS_PER_TASK) {
                let end = chunk.last().map_or(base, |&(i, offset)| {
                    (offset as usize)
                        .saturating_add(texts[i].len())
                        .saturating_add(1)
                });
                let (head, tail) = std::mem::take(&mut rest).split_at_mut(end.saturating_sub(base));
                tasks.push((chunk, head));
                rest = tail;
                base = end;
            }
            tasks.into_par_iter().for_each(|(chunk, out)| {
                let base = chunk.first().map_or(0, |&(_, offset)| offset as usize);
                for &(i, offset) in chunk {
                    let at = (offset as usize).saturating_sub(base);
                    let text = texts[i];
                    if let Some(dest) = out.get_mut(at..at.saturating_add(text.len())) {
                        dest.copy_from_slice(text);
                    }
                }
            });
        }
        self.bulk
            .extend(copies.iter().map(|&(i, offset)| (found[i].0, offset)));
        self.bulk.par_sort_unstable();
        Ok(offsets)
    }
}

/// The GNU hash of a symbol name.
#[must_use]
pub fn gnu_hash(name: &[u8]) -> u32 {
    name.iter().fold(5381u32, |h, &b| {
        h.wrapping_mul(33).wrapping_add(u32::from(b))
    })
}

/// The System V ELF hash of a name.
#[must_use]
pub fn sysv_hash(name: &[u8]) -> u32 {
    let mut h = 0u32;
    for &b in name {
        h = (h << 4).wrapping_add(u32::from(b));
        let g = h & 0xf000_0000;
        if g != 0 {
            h ^= g >> 24;
        }
        h &= !g;
    }
    h
}

/// What the planner needs from the rest of the link.
pub struct PlanInput<'p, 'r, 'a> {
    /// Relocation targets and the symbol table.
    pub refs: &'p Refs<'r, 'a>,
    /// Needed shared objects.
    pub needed: &'p Needed,
    /// The output mode.
    pub mode: Mode,
    /// Options.
    pub options: &'p LinkOptions,
    /// Synthetic sections (GOT, PLT, copies, relocation counts).
    pub synth: &'p Synth,
    /// Exports and versions.
    pub exports: &'p Exports,
    /// The relocation scan.
    pub scan: &'p ScanResult,
    /// Whether each named output section has contents.
    pub has_output: &'p dyn Fn(&[u8]) -> bool,
    /// The `DT_SONAME` of a shared object output, or its file name.
    pub soname: Option<Vec<u8>>,
}

/// The version of a symbol defined by a shared object: `(file, name)`.
/// For a symbol a shared library defines under a non-base version, the
/// library's file index and the version name.
#[must_use]
pub fn import_version<'a>(refs: &Refs<'_, 'a>, id: SymbolId) -> Option<(usize, &'a [u8])> {
    let def = refs.symbols.definition(id);
    if def.kind != DefinitionKind::Shared {
        return None;
    }
    let file = def.file.index();
    let shared = refs.files.get(file)?.shared.as_ref()?;
    let index = *shared.symbols.get(def.index as usize)?;
    let version = shared.elf.symbol_version(index as usize).ok()?;
    let info = version.info?;
    (version.index > VER_NDX_GLOBAL && !info.is_base() && info.kind == VersionKind::Defined)
        .then_some((file, info.name))
}

/// The version glibc requires of objects that use `DT_RELR`.
pub const GLIBC_ABI_DT_RELR: &[u8] = b"GLIBC_ABI_DT_RELR";

/// The needed shared library that defines the `GLIBC_ABI_DT_RELR` version.
fn relr_version_provider(refs: &Refs<'_, '_>, needed: &Needed) -> Option<usize> {
    refs.files.iter().enumerate().find_map(|(index, file)| {
        let shared = file.shared.as_ref()?;
        (needed.is_needed(index)
            && shared
                .elf
                .versions()
                .iter()
                .flatten()
                .any(|v| v.name == GLIBC_ABI_DT_RELR && v.kind == VersionKind::Defined))
        .then_some(index)
    })
}

/// Whether a defined symbol's section made it into the output.
fn present(refs: &Refs<'_, '_>, id: SymbolId) -> bool {
    match refs.global_target(id, true).def {
        Def::Section { file, section, .. } => refs.sections.is_present_in(file, section),
        Def::Undefined { .. } => false,
        _ => true,
    }
}

/// Adds, for each imported weak data symbol, the strong symbol its shared
/// library defines at the same place (glibc's `__environ` for `environ`,
/// `__timezone` for `timezone`), as GNU ld does: it records a weak
/// definition's "real" alias as dynamic whenever the weak one is. `chosen`
/// is in symbol ID order and stays so.
fn with_strong_aliases(
    refs: &Refs<'_, '_>,
    synth: &Synth,
    mut chosen: Vec<(SymbolId, bool)>,
) -> Vec<(SymbolId, bool)> {
    let symbols = refs.symbols;
    // (file, shndx, value, symbol) of each weak data import.
    let mut wanted: Vec<(usize, u16, u64, SymbolId)> = chosen
        .iter()
        .filter(|&&(id, defined)| !defined && synth.copy_of(id).is_none())
        .filter_map(|&(id, _)| {
            let def = symbols.definition(id);
            if def.kind != DefinitionKind::Shared {
                return None;
            }
            let shared = refs.files.get(def.file.index())?.shared.as_ref()?;
            let index = *shared.symbols.get(def.index as usize)?;
            let raw = shared.elf.symbols().get_raw(index as usize)?;
            (raw.binding() == STB_WEAK && !matches!(raw.kind(), STT_FUNC | STT_GNU_IFUNC))
                .then_some((def.file.index(), raw.st_shndx, raw.st_value, id))
        })
        .collect();
    if wanted.is_empty() {
        return chosen;
    }
    wanted.sort_unstable();
    let mut files: Vec<usize> = wanted.iter().map(|&(file, _, _, _)| file).collect();
    files.dedup();
    let mut added: Vec<SymbolId> = Vec::new();
    for file in files {
        let Some(shared) = refs.files.get(file).and_then(|f| f.shared.as_ref()) else {
            continue;
        };
        let ids = refs.resolution.symbol_ids(crate::ids::FileId::new(file));
        // The first strong symbol (by symbol index) at each wanted place.
        let mut found: Vec<(u16, u64)> = Vec::new();
        for (local, (&index, &id)) in shared.symbols.iter().zip(ids).enumerate() {
            if !matches!(
                shared.uses.get(local),
                Some(crate::symbols::SymbolUse::Definition { .. })
            ) || symbols.name(id).version().is_some()
            {
                continue;
            }
            let Some(raw) = shared.elf.symbols().get_raw(index as usize) else {
                continue;
            };
            let place = (raw.st_shndx, raw.st_value);
            if raw.binding() != STB_GLOBAL || found.contains(&place) {
                continue;
            }
            let key = (file, place.0, place.1);
            let from = wanted.partition_point(|&(f, shndx, value, _)| (f, shndx, value) < key);
            let weak: &[(usize, u16, u64, SymbolId)] = wanted.get(from..).unwrap_or_default();
            let count = weak
                .iter()
                .take_while(|&&(f, shndx, value, _)| (f, shndx, value) == key)
                .count();
            let weak = weak.get(..count).unwrap_or_default();
            if weak.is_empty() {
                continue;
            }
            found.push(place);
            let def = symbols.definition(id);
            if def.kind == DefinitionKind::Shared && def.file.index() == file {
                // The alias is referenced as its weak symbol is (GNU ld
                // copies the reference flags), which sets its binding.
                for &(_, _, _, weak) in weak {
                    let flags = symbols.flags(weak) & (REF_REGULAR | REF_REGULAR_STRONG);
                    symbols.set_flags(id, flags);
                }
                added.push(id);
            }
        }
    }
    if added.is_empty() {
        return chosen;
    }
    chosen.extend(added.into_iter().map(|id| (id, false)));
    chosen.sort_unstable_by_key(|&(id, _)| id);
    chosen.dedup_by_key(|&mut (id, _)| id);
    chosen
}

/// Plans the dynamic symbol table; see the [module documentation](self).
///
/// # Errors
///
/// Returns [`Error::Limit`] when tables exceed their formats.
#[allow(clippy::too_many_lines)]
pub fn plan(input: &PlanInput<'_, '_, '_>) -> Result<DynamicPlan> {
    let refs = input.refs;
    let symbols = refs.symbols;
    let mode = input.mode;
    let options = input.options;
    let mut plan = DynamicPlan::default();
    if !mode.dynamic {
        return Ok(plan);
    }
    plan.enabled = true;
    let needs = SymbolFlags::NEEDS_GOT
        | SymbolFlags::NEEDS_PLT
        | SymbolFlags::NEEDS_TLSGD
        | SymbolFlags::NEEDS_GOTTPOFF
        | SymbolFlags::NEEDS_TLSDESC
        | SymbolFlags::NEEDS_DYNSYM
        | SymbolFlags::NEEDS_COPY_RELOC;
    // (id, defined)
    let chosen: Vec<(SymbolId, bool)> = (0..symbols.len())
        .into_par_iter()
        .filter_map(|index| {
            let id = SymbolId::new(index);
            let flags = symbols.flags(id);
            let kind = symbols.definition_kind(id);
            if flags.contains(SymbolFlags::NEEDS_COPY_RELOC)
                || (kind == DefinitionKind::Shared && input.synth.copy_of(id).is_some())
            {
                return Some((id, true));
            }
            match kind {
                DefinitionKind::Shared => {
                    let referenced = flags.contains(REF_REGULAR)
                        && (!options.gc_sections || flags.contains(REF_LIVE));
                    (referenced || flags.intersects(needs)).then_some((id, false))
                }
                DefinitionKind::Undefined | DefinitionKind::Lazy => {
                    // As for imports, a reference from code --gc-sections
                    // removed does not count (GNU ld leaves it out).
                    let referenced = flags.contains(REF_REGULAR)
                        && (!options.gc_sections || flags.contains(REF_LIVE));
                    let wanted = flags.contains(PREEMPTIBLE)
                        && (flags.intersects(needs) || (mode.shared && referenced));
                    wanted.then_some((id, false))
                }
                DefinitionKind::Regular | DefinitionKind::Weak | DefinitionKind::Common => {
                    (flags.contains(SymbolFlags::EXPORTED) && present(refs, id))
                        .then_some((id, true))
                }
            }
        })
        .collect();
    let chosen = with_strong_aliases(refs, input.synth, chosen);

    let mut imports: Vec<SymbolId> = Vec::new();
    let mut exports: Vec<Entry> = Vec::new();
    for &(id, defined) in &chosen {
        // A canonical PLT entry gives an undefined symbol an address the
        // dynamic linker must find by hash, as GNU ld does.
        if defined || symbols.flags(id).contains(SymbolFlags::NEEDS_CANONICAL_PLT) {
            exports.push(Entry::Symbol(id));
        } else {
            imports.push(id);
        }
    }
    let version_script = input.exports.script.as_ref();
    if let Some(script) = version_script {
        for index in 0..script.defs.len() {
            let version = u16::try_from(index)
                .ok()
                .and_then(|i| i.checked_add(2))
                .ok_or_else(|| Error::Limit("too many symbol versions".into()))?;
            exports.push(Entry::Version(version));
        }
    }

    // Names.
    let entry_name = |entry: Entry| -> &[u8] {
        match entry {
            Entry::Symbol(id) => symbols.name(id).bytes(),
            Entry::Version(version) => version_script
                .and_then(|s| s.defs.get(usize::from(version).saturating_sub(2)))
                .map_or(&[][..], |d| d.name.as_slice()),
        }
    };
    let gnu = options.hash_style != HashStyle::Sysv;
    let sysv = options.hash_style != HashStyle::Gnu;
    let hashed_count = exports.len();
    let nbuckets = if gnu {
        u32::try_from((hashed_count / 4).max(1))
            .map_err(|_| Error::Limit("too many dynamic symbols".into()))?
    } else {
        1
    };
    let mut hashed: Vec<(u32, Entry, u32)> = exports
        .par_iter()
        .map(|&entry| {
            let hash = gnu_hash(entry_name(entry));
            (hash, entry, hash.checked_rem(nbuckets).unwrap_or(0))
        })
        .collect();
    if gnu {
        // Keys are unique, so an unstable sort orders like a stable one.
        hashed.par_sort_unstable_by_key(|&(_, entry, bucket)| {
            let key = match entry {
                Entry::Symbol(id) => (0u8, id.as_u32()),
                Entry::Version(v) => (1u8, u32::from(v)),
            };
            (bucket, key)
        });
    }
    plan.entries = imports.iter().map(|&id| Entry::Symbol(id)).collect();
    plan.first_hashed = plan.entries.len();
    plan.entries
        .extend(hashed.iter().map(|&(_, entry, _)| entry));
    if u32::try_from(plan.entries.len().saturating_add(1)).is_err() {
        return Err(Error::Limit("too many dynamic symbols".into()));
    }
    plan.index = vec![0; symbols.len()];
    for (position, entry) in plan.entries.iter().enumerate() {
        if let Entry::Symbol(id) = entry
            && let Some(slot) = plan.index.get_mut(id.index())
        {
            *slot = u32::try_from(position.saturating_add(1)).unwrap_or(0);
        }
    }

    // The version each entry imports, looked up once and in parallel: the
    // needed versions and `.gnu.version` both walk every entry (150,000 for
    // clang, which exports its symbols).
    let imported: Vec<Option<(usize, &[u8])>> = plan
        .entries
        .par_iter()
        .map(|&entry| match entry {
            Entry::Symbol(id) => import_version(refs, id),
            Entry::Version(_) => None,
        })
        .collect();
    // Versions needed: (file, version name) in file order, then name.
    let mut need_list: Vec<(usize, &[u8])> = imported.iter().flatten().copied().collect();
    need_list.sort_unstable();
    // glibc refuses DT_RELR without this version need, which its libc.so
    // defines for that purpose.
    if input.synth.relr_count() > 0
        && let Some(libc) = relr_version_provider(refs, input.needed)
    {
        need_list.push((libc, GLIBC_ABI_DT_RELR));
        need_list.sort_unstable();
    }
    need_list.dedup();
    let verdef_count = version_script.map_or(0, |s| s.defs.len());
    let has_verdef = verdef_count > 0;
    let first_need_index = if has_verdef {
        u16::try_from(verdef_count)
            .ok()
            .and_then(|n| n.checked_add(2))
            .ok_or_else(|| Error::Limit("too many symbol versions".into()))?
    } else {
        2
    };
    let need_index = |file: usize, name: &[u8]| -> u16 {
        need_list
            .binary_search_by(|probe| probe.0.cmp(&file).then(probe.1.cmp(name)))
            .ok()
            .and_then(|at| u16::try_from(at).ok())
            .and_then(|at| at.checked_add(first_need_index))
            .unwrap_or(0)
    };
    let versioned = has_verdef || !need_list.is_empty();

    // String table: needed, soname, run path, symbol names, versions.
    let mut dynstr = StrTab::new();
    let mut needed_names = Vec::new();
    for (index, file) in refs.files.iter().enumerate() {
        if !input.needed.is_needed(index) {
            continue;
        }
        if let Some(shared) = &file.shared {
            needed_names.push(dynstr.add(&shared.needed_name)?);
        }
    }
    let soname = match (&options.soname, mode.shared) {
        (Some(name), true) => Some(dynstr.add(name.as_bytes())?),
        _ => None,
    };
    let run_path_text: Vec<u8> = options
        .rpaths
        .iter()
        .map(|p| p.as_os_str().as_encoded_bytes())
        .collect::<Vec<_>>()
        .join(&b':');
    let run_path = if options.rpaths.is_empty() {
        None
    } else {
        Some(dynstr.add(&run_path_text)?)
    };
    let auxiliary: Vec<u32> = options
        .auxiliary
        .iter()
        .map(|a| dynstr.add(a.as_bytes()))
        .collect::<Result<_>>()?;
    let filters: Vec<u32> = options
        .filter
        .iter()
        .map(|f| dynstr.add(f.as_bytes()))
        .collect::<Result<_>>()?;
    let texts: Vec<&[u8]> = plan
        .entries
        .par_iter()
        .map(|&entry| entry_name(entry))
        .collect();
    plan.names = dynstr.add_all(&texts)?;

    // `.gnu.version`.
    if versioned {
        plan.versym = Vec::with_capacity(plan.entries.len().saturating_add(1));
        plan.versym.push(0);
        let exports = input.exports;
        plan.versym
            .par_extend(plan.entries.par_iter().zip(&imported).map(
                |(&entry, &imported)| match entry {
                    Entry::Version(version) => version,
                    Entry::Symbol(id) => {
                        let def_kind = symbols.definition_kind(id);
                        if let Some((file, name)) = imported {
                            need_index(file, name)
                        } else if matches!(def_kind, DefinitionKind::Shared)
                            || !matches!(
                                def_kind,
                                DefinitionKind::Regular
                                    | DefinitionKind::Weak
                                    | DefinitionKind::Common
                            )
                        {
                            0
                        } else {
                            match exports.version(id) {
                                0 => VER_NDX_GLOBAL,
                                v => v,
                            }
                        }
                    }
                },
            ));
    }

    // `.gnu.version_r`.
    if !need_list.is_empty() {
        let mut files: Vec<usize> = need_list.iter().map(|&(file, _)| file).collect();
        files.dedup();
        plan.verneed_count = u64::try_from(files.len()).unwrap_or(u64::MAX);
        let mut data = Vec::new();
        for (file_position, &file) in files.iter().enumerate() {
            let versions: Vec<&[u8]> = need_list
                .iter()
                .filter(|(f, _)| *f == file)
                .map(|&(_, name)| name)
                .collect();
            let file_name = refs
                .files
                .get(file)
                .and_then(|f| f.shared.as_ref())
                .map_or(&[][..], |s| s.needed_name.as_slice());
            let count = u16::try_from(versions.len())
                .map_err(|_| Error::Limit("too many needed versions".into()))?;
            let last_file = file_position.saturating_add(1) == files.len();
            let next = if last_file {
                0u32
            } else {
                u32::from(count).saturating_mul(16).saturating_add(16)
            };
            data.extend_from_slice(&1u16.to_le_bytes());
            data.extend_from_slice(&count.to_le_bytes());
            data.extend_from_slice(&dynstr.add(file_name)?.to_le_bytes());
            data.extend_from_slice(&16u32.to_le_bytes());
            data.extend_from_slice(&next.to_le_bytes());
            for (position, &name) in versions.iter().enumerate() {
                let last = position.saturating_add(1) == versions.len();
                data.extend_from_slice(&sysv_hash(name).to_le_bytes());
                data.extend_from_slice(&0u16.to_le_bytes());
                data.extend_from_slice(&need_index(file, name).to_le_bytes());
                data.extend_from_slice(&dynstr.add(name)?.to_le_bytes());
                data.extend_from_slice(&(if last { 0u32 } else { 16 }).to_le_bytes());
            }
        }
        plan.verneed = data;
    }

    // `.gnu.version_d`.
    if let Some(script) = version_script
        && has_verdef
    {
        let base_name: Vec<u8> = input.soname.clone().unwrap_or_default();
        // (flags, index, name, parents)
        type VerDefEntry = (u16, u16, Vec<u8>, Vec<Vec<u8>>);
        let mut defs: Vec<VerDefEntry> = Vec::new();
        defs.push((VER_FLG_BASE, 1, base_name, Vec::new()));
        for (index, def) in script.defs.iter().enumerate() {
            let version = u16::try_from(index)
                .ok()
                .and_then(|i| i.checked_add(2))
                .unwrap_or(u16::MAX);
            defs.push((0, version, def.name.clone(), def.parents.clone()));
        }
        plan.verdef_count = u64::try_from(defs.len()).unwrap_or(u64::MAX);
        let mut data = Vec::new();
        let count = defs.len();
        // Strings are added up front so their offsets are stable.
        let mut offsets: Vec<(u32, Vec<u32>)> = Vec::with_capacity(count);
        for (_, _, name, parents) in &defs {
            let own = dynstr.add(name)?;
            let mut parent_offsets = Vec::with_capacity(parents.len());
            for parent in parents {
                parent_offsets.push(dynstr.add(parent)?);
            }
            offsets.push((own, parent_offsets));
        }
        for (position, ((flags, index, name, parents), (own, parent_offsets))) in
            defs.iter().zip(&offsets).enumerate()
        {
            let aux_count = parents.len().saturating_add(1);
            let cnt = u16::try_from(aux_count)
                .map_err(|_| Error::Limit("too many version parents".into()))?;
            let size = 20u32.saturating_add(u32::from(cnt).saturating_mul(8));
            let last = position.saturating_add(1) == count;
            data.extend_from_slice(&1u16.to_le_bytes());
            data.extend_from_slice(&flags.to_le_bytes());
            data.extend_from_slice(&index.to_le_bytes());
            data.extend_from_slice(&cnt.to_le_bytes());
            data.extend_from_slice(&sysv_hash(name).to_le_bytes());
            data.extend_from_slice(&20u32.to_le_bytes());
            data.extend_from_slice(&(if last { 0 } else { size }).to_le_bytes());
            let names = std::iter::once(*own).chain(parent_offsets.iter().copied());
            for (aux, offset) in names.enumerate() {
                let last_aux = aux.saturating_add(1) == aux_count;
                data.extend_from_slice(&offset.to_le_bytes());
                data.extend_from_slice(&(if last_aux { 0u32 } else { 8 }).to_le_bytes());
            }
        }
        plan.verdef = data;
    }

    // Hash tables.
    let hashed_names: Vec<(u32, &[u8])> = hashed
        .iter()
        .map(|&(hash, entry, _)| (hash, entry_name(entry)))
        .collect();
    if gnu {
        plan.gnu_hash = build_gnu_hash(&hashed_names, nbuckets, plan.first_hashed)?;
    }
    if sysv {
        let names: Vec<&[u8]> = plan.entries.iter().map(|&e| entry_name(e)).collect();
        plan.sysv_hash = build_sysv_hash(&names)?;
    }
    plan.dynstr = dynstr.data;

    plan.dynamic = dynamic_entries(
        input,
        &plan,
        &DynamicStrings {
            needed: needed_names,
            soname,
            run_path,
            auxiliary,
            filters,
        },
    );
    Ok(plan)
}

/// String offsets `.dynamic` needs.
struct DynamicStrings {
    needed: Vec<u32>,
    soname: Option<u32>,
    run_path: Option<u32>,
    auxiliary: Vec<u32>,
    filters: Vec<u32>,
}

fn build_gnu_hash(hashed: &[(u32, &[u8])], nbuckets: u32, symoffset: usize) -> Result<Vec<u8>> {
    let too_big = || Error::Limit("GNU hash table too large".into());
    let count = hashed.len();
    let bits = count.saturating_mul(12);
    let mask_words = (bits / 64).max(1).next_power_of_two();
    let mask_words32 = u32::try_from(mask_words).map_err(|_| too_big())?;
    let shift = 26u32;
    let mut bloom = vec![0u64; mask_words];
    let mut buckets = vec![0u32; nbuckets as usize];
    let mut chains = vec![0u32; count];
    let symoffset32 = u32::try_from(symoffset.saturating_add(1)).map_err(|_| too_big())?;
    for (position, &(hash, _)) in hashed.iter().enumerate() {
        let word = (hash >> 6).checked_rem(mask_words32).unwrap_or(0);
        if let Some(slot) = bloom.get_mut(word as usize) {
            *slot |= (1u64 << (hash % 64)) | (1u64 << ((hash >> shift) % 64));
        }
        let bucket = hash.checked_rem(nbuckets).unwrap_or(0);
        let dynsym_index = u32::try_from(position)
            .ok()
            .and_then(|p| p.checked_add(symoffset32))
            .ok_or_else(too_big)?;
        if let Some(slot) = buckets.get_mut(bucket as usize)
            && *slot == 0
        {
            *slot = dynsym_index;
        }
        let last = hashed
            .get(position.saturating_add(1))
            .is_none_or(|&(next, _)| next.checked_rem(nbuckets).unwrap_or(0) != bucket);
        if let Some(slot) = chains.get_mut(position) {
            *slot = if last { hash | 1 } else { hash & !1 };
        }
    }
    let mut data = Vec::with_capacity(
        16usize
            .saturating_add(mask_words.saturating_mul(8))
            .saturating_add((nbuckets as usize).saturating_mul(4))
            .saturating_add(count.saturating_mul(4)),
    );
    data.extend_from_slice(&nbuckets.to_le_bytes());
    data.extend_from_slice(&symoffset32.to_le_bytes());
    data.extend_from_slice(&mask_words32.to_le_bytes());
    data.extend_from_slice(&shift.to_le_bytes());
    for word in bloom {
        data.extend_from_slice(&word.to_le_bytes());
    }
    for bucket in buckets {
        data.extend_from_slice(&bucket.to_le_bytes());
    }
    for chain in chains {
        data.extend_from_slice(&chain.to_le_bytes());
    }
    Ok(data)
}

fn build_sysv_hash(names: &[&[u8]]) -> Result<Vec<u8>> {
    let too_big = || Error::Limit("hash table too large".into());
    let nchain = u32::try_from(names.len().saturating_add(1)).map_err(|_| too_big())?;
    let nbucket = nchain.max(1);
    let mut buckets = vec![0u32; nbucket as usize];
    let mut chains = vec![0u32; nchain as usize];
    for (position, name) in names.iter().enumerate() {
        let index = u32::try_from(position.saturating_add(1)).map_err(|_| too_big())?;
        let bucket = sysv_hash(name).checked_rem(nbucket).unwrap_or(0) as usize;
        if let (Some(head), Some(chain)) = (buckets.get_mut(bucket), chains.get_mut(index as usize))
        {
            *chain = *head;
            *head = index;
        }
    }
    let mut data = Vec::with_capacity(
        8usize.saturating_add(
            (nbucket as usize)
                .saturating_add(nchain as usize)
                .saturating_mul(4),
        ),
    );
    data.extend_from_slice(&nbucket.to_le_bytes());
    data.extend_from_slice(&nchain.to_le_bytes());
    for value in buckets.into_iter().chain(chains) {
        data.extend_from_slice(&value.to_le_bytes());
    }
    Ok(data)
}

/// The symbol `name` if a regular object defines it and it is in the
/// output.
fn defined_symbol(refs: &Refs<'_, '_>, name: &[u8]) -> Option<SymbolId> {
    let id = refs
        .symbols
        .lookup(&crate::symbols::SymbolName::new(name))?;
    matches!(
        refs.symbols.definition_kind(id),
        DefinitionKind::Regular | DefinitionKind::Weak
    )
    .then_some(id)
    .filter(|&id| present(refs, id))
}

fn dynamic_entries(
    input: &PlanInput<'_, '_, '_>,
    plan: &DynamicPlan,
    strings: &DynamicStrings,
) -> Vec<(i64, DynValue)> {
    use DynValue::{Address, OutputAddress, OutputSize, Size, Value};
    let options = input.options;
    let mode = input.mode;
    let synth = input.synth;
    let refs = input.refs;
    let mut entries = Vec::new();
    for &needed in &strings.needed {
        entries.push((DT_NEEDED, Value(u64::from(needed))));
    }
    for &aux in &strings.auxiliary {
        entries.push((DT_AUXILIARY, Value(u64::from(aux))));
    }
    for &filter in &strings.filters {
        entries.push((DT_FILTER, Value(u64::from(filter))));
    }
    if let Some(soname) = strings.soname {
        entries.push((DT_SONAME, Value(u64::from(soname))));
    }
    if let Some(run_path) = strings.run_path {
        let tag = if options.new_dtags == Some(false) {
            DT_RPATH
        } else {
            DT_RUNPATH
        };
        entries.push((tag, Value(u64::from(run_path))));
    }
    let init = options.init.as_deref().unwrap_or("_init");
    if let Some(id) = defined_symbol(refs, init.as_bytes()) {
        entries.push((DT_INIT, DynValue::Symbol(id)));
    }
    let fini = options.fini.as_deref().unwrap_or("_fini");
    if let Some(id) = defined_symbol(refs, fini.as_bytes()) {
        entries.push((DT_FINI, DynValue::Symbol(id)));
    }
    if mode.executable() && (input.has_output)(b".preinit_array") {
        entries.push((DT_PREINIT_ARRAY, OutputAddress(b".preinit_array")));
        entries.push((DT_PREINIT_ARRAYSZ, OutputSize(b".preinit_array")));
    }
    if (input.has_output)(b".init_array") {
        entries.push((DT_INIT_ARRAY, OutputAddress(b".init_array")));
        entries.push((DT_INIT_ARRAYSZ, OutputSize(b".init_array")));
    }
    if (input.has_output)(b".fini_array") {
        entries.push((DT_FINI_ARRAY, OutputAddress(b".fini_array")));
        entries.push((DT_FINI_ARRAYSZ, OutputSize(b".fini_array")));
    }
    if !plan.gnu_hash.is_empty() {
        entries.push((DT_GNU_HASH, Address(Synthetic::GnuHash)));
    }
    if !plan.sysv_hash.is_empty() {
        entries.push((DT_HASH, Address(Synthetic::Hash)));
    }
    entries.push((DT_STRTAB, Address(Synthetic::DynStr)));
    entries.push((DT_SYMTAB, Address(Synthetic::DynSym)));
    entries.push((DT_STRSZ, Size(Synthetic::DynStr)));
    entries.push((DT_SYMENT, Value(DYNSYM_SIZE)));
    if mode.executable() {
        entries.push((DT_DEBUG, Value(0)));
    }
    if synth.got_plt_reserved > 0 {
        entries.push((DT_PLTGOT, Address(Synthetic::GotPlt)));
    }
    if synth.plt_entries() > 0 {
        entries.push((DT_PLTRELSZ, Size(Synthetic::RelaPlt)));
        entries.push((DT_PLTREL, Value(crate::elf::read::consts::DT_RELA as u64)));
        entries.push((DT_JMPREL, Address(Synthetic::RelaPlt)));
    }
    if synth.rela_dyn_count() > 0 {
        entries.push((DT_RELA, Address(Synthetic::RelaDyn)));
        entries.push((DT_RELASZ, Size(Synthetic::RelaDyn)));
        entries.push((DT_RELAENT, Value(24)));
    }
    if synth.relr_count() > 0 {
        entries.push((DT_RELR, Address(Synthetic::RelrDyn)));
        entries.push((DT_RELRSZ, Size(Synthetic::RelrDyn)));
        entries.push((DT_RELRENT, Value(8)));
    }
    let text = input.scan.text_relocs();
    if text {
        entries.push((DT_TEXTREL, Value(0)));
    }
    let symbolic = options.symbolic == crate::args::SymbolicMode::All && mode.shared;
    if symbolic {
        entries.push((DT_SYMBOLIC, Value(0)));
    }
    let z = &options.dynamic_flags;
    let mut flags = 0u64;
    if z.origin {
        flags |= DF_ORIGIN;
    }
    if symbolic {
        flags |= DF_SYMBOLIC;
    }
    if text {
        flags |= DF_TEXTREL;
    }
    if options.bind_now {
        flags |= DF_BIND_NOW;
    }
    if mode.shared && input.scan.static_tls() {
        flags |= DF_STATIC_TLS;
    }
    if flags != 0 {
        entries.push((DT_FLAGS, Value(flags)));
    }
    let mut flags_1 = 0u64;
    for (on, bit) in [
        (options.bind_now, DF_1_NOW),
        (mode.pic && mode.executable(), DF_1_PIE),
        (z.nodelete, DF_1_NODELETE),
        (z.nodlopen, DF_1_NOOPEN),
        (z.nodump, DF_1_NODUMP),
        (z.initfirst, DF_1_INITFIRST),
        (z.interpose, DF_1_INTERPOSE),
        (z.global, DF_1_GLOBAL),
        (z.nodefaultlib, DF_1_NODEFLIB),
        (z.loadfltr, DF_1_LOADFLTR),
        (z.origin, DF_1_ORIGIN),
        (z.singleton, DF_1_SINGLETON),
    ] {
        if on {
            flags_1 |= bit;
        }
    }
    if flags_1 != 0 {
        entries.push((DT_FLAGS_1, Value(flags_1)));
    }
    if plan.verdef_count > 0 {
        entries.push((DT_VERDEF, Address(Synthetic::VerDef)));
        entries.push((DT_VERDEFNUM, Value(plan.verdef_count)));
    }
    if plan.verneed_count > 0 {
        entries.push((DT_VERNEED, Address(Synthetic::VerNeed)));
        entries.push((DT_VERNEEDNUM, Value(plan.verneed_count)));
    }
    if !plan.versym.is_empty() {
        entries.push((DT_VERSYM, Address(Synthetic::VerSym)));
    }
    let relative = synth.relative_count();
    if options.combine_relocs && relative > 0 {
        entries.push((DT_RELACOUNT, Value(relative)));
    }
    for _ in 0..options.spare_dynamic_tags.unwrap_or(0).min(64) {
        entries.push((DT_NULL, Value(0)));
    }
    entries.push((DT_NULL, Value(0)));
    entries
}

fn put_sym(out: &mut [u8], name: u32, info: u8, other: u8, shndx: u16, value: u64, size: u64) {
    let Some(entry) = out.first_chunk_mut::<24>() else {
        return;
    };
    entry[0..4].copy_from_slice(&name.to_le_bytes());
    entry[4] = info;
    entry[5] = other;
    entry[6..8].copy_from_slice(&shndx.to_le_bytes());
    entry[8..16].copy_from_slice(&value.to_le_bytes());
    entry[16..24].copy_from_slice(&size.to_le_bytes());
}

/// The output section header index holding `address`, or `SHN_ABS`.
pub fn shndx_of_address(addresses: &Addresses<'_, '_>, address: u64) -> u16 {
    let sections = &addresses.layout.sections;
    sections
        .iter()
        .position(|s| s.is_alloc() && s.addr <= address && address < s.addr.saturating_add(s.size))
        .or_else(|| {
            // Past the end of a section (`_end`): the last section before it.
            sections
                .iter()
                .rposition(|s| s.is_alloc() && s.addr <= address && s.addr != 0)
        })
        .and_then(|p| u16::try_from(p.saturating_add(1)).ok())
        .unwrap_or(SHN_ABS)
}

/// Writes `.dynsym`.
pub fn write_dynsym(plan: &DynamicPlan, addresses: &Addresses<'_, '_>, out: &mut [u8]) {
    let refs = &addresses.refs;
    let symbols = refs.symbols;
    let (entries, _) = out.as_chunks_mut::<24>();
    let Some((null, rest)) = entries.split_first_mut() else {
        return;
    };
    null.fill(0);
    rest.par_iter_mut()
        .zip(plan.entries.par_iter())
        .zip(plan.names.par_iter())
        .for_each(|((slot, &entry), &name)| {
            let id = match entry {
                Entry::Version(_) => {
                    put_sym(
                        slot,
                        name,
                        (STB_GLOBAL << 4) | STT_OBJECT,
                        STV_DEFAULT,
                        SHN_ABS,
                        0,
                        0,
                    );
                    return;
                }
                Entry::Symbol(id) => id,
            };
            let flags = symbols.flags(id);
            let value = addresses.globals.get(id.index()).copied().unwrap_or(0);
            let visibility = match merged_visibility(flags) {
                STV_PROTECTED => STV_PROTECTED,
                _ => STV_DEFAULT,
            };
            let target = refs.global_target(id, true);
            let raw = target.raw.unwrap_or_default();
            let copy = addresses.synth.copy_of(id).is_some();
            let import = match target.def {
                Def::Shared(_) => !copy,
                Def::Undefined { .. } => true,
                _ => false,
            };
            if import {
                let binding = if flags.contains(REF_REGULAR_STRONG) {
                    STB_GLOBAL
                } else {
                    STB_WEAK
                };
                // An imported IFUNC is a plain function to its users.
                let kind = match (target.def, raw.kind()) {
                    (Def::Shared(_), STT_GNU_IFUNC) => STT_FUNC,
                    (Def::Shared(_), kind) => kind,
                    _ => STT_NOTYPE,
                };
                let canonical = flags.contains(SymbolFlags::NEEDS_CANONICAL_PLT);
                put_sym(
                    slot,
                    name,
                    (binding << 4) | kind,
                    STV_DEFAULT,
                    SHN_UNDEF,
                    if canonical { value } else { 0 },
                    0,
                );
                return;
            }
            let (binding, kind, size, shndx) = match target.def {
                // A copy keeps the library definition's binding, as in GNU ld.
                _ if copy => (
                    raw.binding(),
                    raw.kind(),
                    raw.st_size,
                    shndx_of_address(addresses, value),
                ),
                // By section rather than by address: a symbol in a
                // non-allocated section (rustc's `rust_metadata_*` in
                // `.rustc`) has no address but keeps its section, as in GNU ld.
                Def::Section {
                    file,
                    section,
                    value: offset,
                } => (
                    raw.binding(),
                    raw.kind(),
                    addresses.symbol_size(file, section, offset, raw.st_size),
                    match super::symtab::shndx_for(addresses, file, section) {
                        SHN_UNDEF | SHN_ABS => shndx_of_address(addresses, value),
                        shndx => shndx,
                    },
                ),
                Def::Absolute(_) => (raw.binding(), raw.kind(), raw.st_size, SHN_ABS),
                Def::Common(_) => (
                    STB_GLOBAL,
                    STT_OBJECT,
                    symbols.definition(id).aux & !super::resolve::AUX_COMDAT,
                    shndx_of_address(addresses, value),
                ),
                Def::Linker(_) => {
                    let absolute = flags.contains(super::defined::ABSOLUTE)
                        && symbols.definition(id).file != LINKER_FILE;
                    (
                        STB_GLOBAL,
                        STT_NOTYPE,
                        0,
                        if absolute {
                            SHN_ABS
                        } else {
                            shndx_of_address(addresses, value)
                        },
                    )
                }
                Def::Shared(_) | Def::Undefined { .. } => (STB_GLOBAL, STT_NOTYPE, 0, SHN_UNDEF),
            };
            // A TLS symbol's value is its offset in the TLS template.
            let value = if kind == STT_TLS {
                value.wrapping_sub(addresses.layout.tls.map_or(0, |t| t.start))
            } else {
                value
            };
            put_sym(
                slot,
                name,
                (binding << 4) | kind,
                visibility,
                shndx,
                value,
                size,
            );
        });
}

/// Writes `.dynamic`.
pub fn write_dynamic(plan: &DynamicPlan, addresses: &Addresses<'_, '_>, out: &mut [u8]) {
    let layout = addresses.layout;
    let output = |name: &[u8]| layout.by_name(name).map_or((0, 0), |s| (s.addr, s.size));
    for ((tag, value), slot) in plan
        .dynamic
        .iter()
        .zip(out.as_chunks_mut::<16>().0.iter_mut())
    {
        let value = match *value {
            DynValue::Value(v) => v,
            DynValue::Address(kind) => layout.synthetic(kind).map_or(0, |(addr, ..)| addr),
            DynValue::Size(kind) => layout.synthetic(kind).map_or(0, |(_, _, size)| size),
            DynValue::OutputAddress(name) => output(name).0,
            DynValue::OutputSize(name) => output(name).1,
            DynValue::Symbol(id) => addresses.globals.get(id.index()).copied().unwrap_or(0),
        };
        slot[0..8].copy_from_slice(&tag.to_le_bytes());
        slot[8..16].copy_from_slice(&value.to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_all_matches_adding_one_by_one() {
        // Duplicates within the batch, strings present before it, empty
        // strings, and enough strings for several parallel tasks.
        let mut texts: Vec<Vec<u8>> = Vec::new();
        for i in 0..20_000u32 {
            texts.push(format!("sym{}", i % 7_001).into_bytes());
            if i % 97 == 0 {
                texts.push(Vec::new());
                texts.push(b"libc.so.6".to_vec());
            }
        }
        let refs: Vec<&[u8]> = texts.iter().map(Vec::as_slice).collect();
        let mut one = StrTab::new();
        let mut all = StrTab::new();
        for table in [&mut one, &mut all] {
            table.add(b"libc.so.6").unwrap();
            table.add(b"sym5").unwrap();
        }
        let expected: Vec<u32> = refs.iter().map(|t| one.add(t).unwrap()).collect();
        let offsets = all.add_all(&refs).unwrap();
        assert_eq!(offsets, expected);
        assert_eq!(all.data, one.data);
        // Later single additions still find the batch's strings.
        assert_eq!(all.add(b"sym42").unwrap(), one.add(b"sym42").unwrap());
        assert_eq!(all.add(b"new").unwrap(), one.add(b"new").unwrap());
        assert_eq!(all.data, one.data);
    }
}
