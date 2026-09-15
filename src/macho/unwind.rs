//! `__TEXT,__unwind_info`: the two-level lookup table libunwind uses to
//! find a function's unwind encoding, personality and LSDA.
//!
//! The construction follows lld's `UnwindInfoSection`:
//!
//! 1. One entry per function start: every live symbol in a code section
//!    (encoding 0 when the function has no unwind information), overridden
//!    by the `__LD,__compact_unwind` records of live functions, and by the
//!    `__eh_frame` FDEs of functions whose unwind information is DWARF
//!    (their encoding is the DWARF mode plus the FDE's offset in the output
//!    `__eh_frame`).
//! 2. Entries are sorted by address, and runs with the same encoding and
//!    personality and no LSDA are folded into their first entry.
//! 3. Personalities (at most three) are referenced through `__got` slots;
//!    the most frequent encodings (at most 127) go into the common table.
//! 4. Second-level pages of 4 KiB use the compressed format (24-bit function
//!    offsets and 8-bit encoding indexes), or the regular format when a
//!    page would otherwise hold too few entries.
//! 5. A sentinel index entry marks the end of the last function, and the
//!    LSDA index lists the functions that have one.

#![deny(clippy::arithmetic_side_effects)]

use hashbrown::HashMap;

use crate::error::{Error, Result};
use crate::ids::SymbolId;
use crate::macho::read::consts::{
    CPU_TYPE_X86_64, N_SECT, UNWIND_ARM64_MODE_DWARF, UNWIND_HAS_LSDA, UNWIND_MODE_MASK,
    UNWIND_PERSONALITY_MASK, UNWIND_X86_64_MODE_DWARF, UNWIND_X86_64_MODE_STACK_IND,
};
use crate::macho::read::{CompactUnwindEntry, compact_unwind_entries};

use super::addr::Addresses;
use super::buf::{put32, to_u64, to_usize};
use super::layout::{Layout, SectionKind, is_consumed};
use super::reloc::{self, Place, Referent, Resolve, Value};
use super::state::{Link, NONE};

const PAGE_BYTES: usize = 4096;
const PAGE_WORDS: usize = 1024;
const COMMON_ENCODINGS_MAX: usize = 127;
const COMPACT_ENCODINGS_MAX: usize = 256;
const COMPRESSED_OFFSET_MASK: u64 = 0x00ff_ffff;
const REGULAR_ENTRIES_MAX: usize = (PAGE_BYTES - 8) / 8;
const UNWIND_SECOND_LEVEL_REGULAR: u32 = 2;
const UNWIND_SECOND_LEVEL_COMPRESSED: u32 = 3;

/// A personality routine: an imported or defined global symbol.
pub type Personality = SymbolId;

/// One function's unwind information before layout.
#[derive(Clone, Copy, Debug)]
pub struct Entry {
    /// The function (an atom and offset).
    pub function: Place,
    /// Its length.
    pub length: u32,
    /// The compact encoding (without the personality index bits).
    pub encoding: u32,
    /// The personality routine.
    pub personality: Option<Personality>,
    /// The LSDA.
    pub lsda: Option<Place>,
    /// Offset of the function's FDE in the output `__eh_frame`, for DWARF
    /// entries.
    pub fde: Option<u64>,
}

/// The unwind entries of the live functions, before sorting.
#[derive(Clone, Debug, Default)]
pub struct Entries {
    /// Every entry.
    pub entries: Vec<Entry>,
    /// Functions (global atom index, offset) with a compact unwind record
    /// that does not defer to DWARF: their FDEs are dropped.
    pub compact: HashMap<(usize, i64), usize>,
    /// The entry of each function (global atom index, offset).
    pub by_place: HashMap<(usize, i64), usize>,
}

fn dwarf_mode(arm64: bool) -> u32 {
    if arm64 {
        UNWIND_ARM64_MODE_DWARF
    } else {
        UNWIND_X86_64_MODE_DWARF
    }
}

/// Collects the function starts and compact unwind records of live code.
///
/// # Errors
///
/// Malformed `__compact_unwind` sections.
pub fn collect(link: &Link<'_>) -> Result<Entries> {
    let arm64 = link.config.is_arm64();
    let mut result = Entries::default();
    // (atom, offset) -> entry index.
    let mut by_place: HashMap<(usize, i64), usize> = HashMap::new();
    for file in 0..link.files.len() {
        let Some(object) = link.object(file) else {
            continue;
        };
        // Function starts.
        for symbol in object.file.symbols().iter().flatten() {
            if symbol.n_type & 0x0e != N_SECT {
                continue;
            }
            let Some(section) = object.file.section_by_ordinal(u32::from(symbol.n_sect)) else {
                continue;
            };
            if !is_code(section.segname, section.sectname, section.flags) {
                continue;
            }
            let Some(atom) = object.atoms.symbol_atom(symbol.index) else {
                continue;
            };
            if !link.is_live(file, atom) {
                continue;
            }
            if symbol.is_external() {
                let global = object
                    .global_of_symbol
                    .get(to_usize(u64::from(symbol.index)))
                    .copied()
                    .unwrap_or(NONE);
                if !link.global_wins(file, to_usize(u64::from(global))) {
                    continue;
                }
            }
            let start = object
                .atoms
                .atoms()
                .get(atom)
                .map_or(0, |a| section.addr.saturating_add(a.offset));
            let key = (
                link.atom_id(file, atom),
                symbol.n_value.wrapping_sub(start) as i64,
            );
            by_place.entry(key).or_insert_with(|| {
                result.entries.push(Entry {
                    function: Place::Atom {
                        atom: key.0,
                        offset: key.1,
                    },
                    length: 0,
                    encoding: 0,
                    personality: None,
                    lsda: None,
                    fde: None,
                });
                result.entries.len().saturating_sub(1)
            });
        }

        // Compact unwind records.
        let records = compact_unwind_entries(&object.file)?;
        if records.is_empty() {
            continue;
        }
        let Some((section_index, _)) = object.file.find_section(b"__LD", b"__compact_unwind")
        else {
            continue;
        };
        let data = object.file.section_data(section_index)?;
        for record in &records {
            let Some(function) = record_place(link, file, section_index, data, record)? else {
                continue;
            };
            let Place::Atom { atom, offset } = function else {
                continue;
            };
            if !link.live.get(atom).copied().unwrap_or(false) {
                continue;
            }
            let personality = match record.personality.relocation {
                Some(relocation) => {
                    let decoded =
                        reloc::decode(link, file, object, section_index, data, &relocation)?;
                    match decoded.referent {
                        Referent::Global(id) => Some(id),
                        _ => {
                            return Err(object
                                .file
                                .source()
                                .malformed(0, "compact unwind personality (not a global symbol)"));
                        }
                    }
                }
                None => None,
            };
            let lsda = match record.lsda.relocation {
                Some(relocation) => {
                    let decoded =
                        reloc::decode(link, file, object, section_index, data, &relocation)?;
                    Some(reloc::place(
                        link,
                        file,
                        object,
                        decoded.referent,
                        decoded.addend,
                    )?)
                }
                None => None,
            };
            let entry = Entry {
                function,
                length: record.length,
                encoding: record.encoding & !UNWIND_PERSONALITY_MASK,
                personality,
                lsda,
                fde: None,
            };
            let key = (atom, offset);
            let index = match by_place.get(&key) {
                Some(&index) => {
                    if let Some(slot) = result.entries.get_mut(index) {
                        *slot = entry;
                    }
                    index
                }
                None => {
                    result.entries.push(entry);
                    let index = result.entries.len().saturating_sub(1);
                    by_place.insert(key, index);
                    index
                }
            };
            if record.encoding & UNWIND_MODE_MASK != dwarf_mode(arm64) {
                result.compact.insert(key, index);
            }
        }
    }
    result.by_place = by_place;
    Ok(result)
}

/// Whether a section holds code, as lld decides for unwind information.
#[must_use]
pub fn is_code(segname: &[u8], sectname: &[u8], flags: u32) -> bool {
    use crate::macho::read::consts::{S_ATTR_PURE_INSTRUCTIONS, SECTION_ATTRIBUTES, SECTION_TYPE};
    let kind = flags & SECTION_TYPE;
    if kind != 0 && kind != 0x6 {
        // Not S_REGULAR or S_COALESCED.
        return false;
    }
    if is_consumed(segname, sectname, flags) {
        return false;
    }
    // User attributes only (the top byte).
    if flags & SECTION_ATTRIBUTES & 0xff00_0000 == S_ATTR_PURE_INSTRUCTIONS {
        return true;
    }
    segname == b"__TEXT" && matches!(sectname, b"__textcoal_nt" | b"__StaticInit")
}

/// Where a compact unwind record's function field points.
fn record_place(
    link: &Link<'_>,
    file: usize,
    section: usize,
    data: &[u8],
    record: &CompactUnwindEntry,
) -> Result<Option<Place>> {
    let Some(object) = link.object(file) else {
        return Ok(None);
    };
    match record.function.relocation {
        Some(relocation) => {
            let decoded = reloc::decode(link, file, object, section, data, &relocation)?;
            reloc::place(link, file, object, decoded.referent, decoded.addend).map(Some)
        }
        None => Ok(None),
    }
}

/// A folded, sorted entry with its final values.
#[derive(Clone, Copy, Debug)]
struct Final {
    address: u64,
    length: u32,
    encoding: u32,
    personality: Option<Personality>,
    lsda: Option<u64>,
}

#[derive(Clone, Debug, Default)]
struct Page {
    first: usize,
    count: usize,
    compressed: bool,
    local_encodings: Vec<u32>,
    local_indexes: HashMap<u32, usize>,
}

/// The finished `__unwind_info` plan: the section size, then the contents.
#[derive(Clone, Debug, Default)]
pub struct UnwindPlan {
    entries: Entries,
    size: u64,
}

impl UnwindPlan {
    /// Size of the section.
    #[must_use]
    pub fn size(&self) -> u64 {
        self.size
    }

    /// The collected entries.
    #[must_use]
    pub fn entries(&self) -> &Entries {
        &self.entries
    }
}

/// Plans `__unwind_info` from `entries`, whose DWARF entries already carry
/// their FDE offsets. The size does not depend on addresses as long as no
/// second-level page spans more than 16 MiB of code; [`build`] reports the
/// exact size.
#[must_use]
pub fn plan(link: &Link<'_>, entries: Entries) -> UnwindPlan {
    let any = entries
        .entries
        .iter()
        .any(|e| e.encoding != 0 || e.fde.is_some());
    // An upper bound: every entry unfolded, in regular pages, plus headers.
    let size = if any {
        let count = entries.entries.len();
        let pages = count.div_ceil(REGULAR_ENTRIES_MAX).max(1);
        let bytes = 28usize
            .saturating_add(COMMON_ENCODINGS_MAX.saturating_mul(4))
            .saturating_add(12)
            .saturating_add(pages.saturating_add(1).saturating_mul(12))
            .saturating_add(count.saturating_mul(8))
            .saturating_add(pages.saturating_mul(PAGE_BYTES));
        to_u64(bytes)
    } else {
        0
    };
    let _ = link;
    UnwindPlan { entries, size }
}

/// Resolves the entries to addresses, sorts and folds them.
fn finalize(
    addresses: &Addresses<'_, '_>,
    plan: &UnwindPlan,
    eh_frame: Option<u64>,
) -> Result<Vec<Final>> {
    let arm64 = addresses.link.config.is_arm64();
    let mut out = Vec::with_capacity(plan.entries.entries.len());
    for entry in &plan.entries.entries {
        let address = match addresses.value(entry.function, 0)? {
            Value::Address(address) => address,
            _ => continue,
        };
        let lsda = match entry.lsda {
            Some(place) => match addresses.value(place, 0)? {
                Value::Address(address) => Some(address),
                _ => None,
            },
            None => None,
        };
        let mut encoding = entry.encoding;
        if let (Some(fde), Some(_)) = (entry.fde, eh_frame) {
            encoding = dwarf_mode(arm64) | u32::try_from(fde & 0x00ff_ffff).unwrap_or(0);
        }
        let _ = UNWIND_HAS_LSDA;
        out.push(Final {
            address,
            length: entry.length,
            encoding,
            personality: if entry.fde.is_some() {
                None
            } else {
                entry.personality
            },
            lsda,
        });
    }
    out.sort_by_key(|e| e.address);
    Ok(out)
}

fn can_fold(arm64: bool, encoding: u32) -> bool {
    arm64 || encoding & UNWIND_MODE_MASK != UNWIND_X86_64_MODE_STACK_IND
}

/// Builds the section contents. `eh_frame` is the address of the output
/// `__eh_frame`, when there is one.
///
/// # Errors
///
/// More than three personalities, or a personality without a `__got` slot.
#[allow(clippy::too_many_lines)]
pub fn build(addresses: &Addresses<'_, '_>, plan: &UnwindPlan) -> Result<Vec<u8>> {
    let layout: &Layout = addresses.layout;
    let eh_frame = layout.find(SectionKind::EhFrame).map(|s| s.addr);
    let arm64 = addresses.link.config.is_arm64();
    let base = addresses.header_address();
    let mut entries = finalize(addresses, plan, eh_frame)?;
    if entries.is_empty() {
        return Ok(Vec::new());
    }
    let end_boundary = entries
        .last()
        .map_or(0, |e| e.address.saturating_add(u64::from(e.length)));

    // Fold.
    let mut folded: Vec<Final> = Vec::with_capacity(entries.len());
    for entry in entries.drain(..) {
        if let Some(last) = folded.last()
            && last.encoding == entry.encoding
            && last.lsda.is_none()
            && entry.lsda.is_none()
            && last.personality == entry.personality
            && can_fold(arm64, entry.encoding)
        {
            continue;
        }
        folded.push(entry);
    }

    // Personalities.
    let mut personalities: Vec<Personality> = Vec::new();
    for entry in &mut folded {
        let Some(personality) = entry.personality else {
            continue;
        };
        let index = match personalities.iter().position(|&p| p == personality) {
            Some(index) => index,
            None => {
                personalities.push(personality);
                personalities.len().saturating_sub(1)
            }
        };
        if index >= 3 {
            return Err(Error::Limit(
                "more than three personality routines in __unwind_info".into(),
            ));
        }
        let bits = u32::try_from(index.saturating_add(1)).unwrap_or(0) << 28;
        entry.encoding = (entry.encoding & !UNWIND_PERSONALITY_MASK) | bits;
    }

    // Common encodings, by descending frequency then descending value.
    let mut frequency: HashMap<u32, usize> = HashMap::new();
    for entry in &folded {
        let count = frequency.entry(entry.encoding).or_insert(0);
        *count = count.saturating_add(1);
    }
    let mut common: Vec<(u32, usize)> = frequency.into_iter().collect();
    common.sort_by(|a, b| b.1.cmp(&a.1).then(b.0.cmp(&a.0)));
    common.truncate(COMMON_ENCODINGS_MAX);
    let common_index: HashMap<u32, usize> = common
        .iter()
        .enumerate()
        .map(|(i, &(encoding, _))| (encoding, i))
        .collect();

    // Pages.
    let mut pages: Vec<Page> = Vec::new();
    let mut i = 0usize;
    while i < folded.len() {
        let mut page = Page {
            first: i,
            ..Page::default()
        };
        let first_address = folded.get(i).map_or(0, |e| e.address);
        let limit = first_address.saturating_add(COMPRESSED_OFFSET_MASK);
        let mut next_index = common.len();
        let mut words = PAGE_WORDS.saturating_sub(3);
        while words >= 1 && i < folded.len() {
            let Some(entry) = folded.get(i) else {
                break;
            };
            if entry.address >= limit {
                break;
            }
            if common_index.contains_key(&entry.encoding)
                || page.local_indexes.contains_key(&entry.encoding)
            {
                i = i.saturating_add(1);
                words = words.saturating_sub(1);
            } else if words >= 2 && next_index < COMPACT_ENCODINGS_MAX {
                page.local_encodings.push(entry.encoding);
                page.local_indexes.insert(entry.encoding, next_index);
                next_index = next_index.saturating_add(1);
                i = i.saturating_add(1);
                words = words.saturating_sub(2);
            } else {
                break;
            }
        }
        page.count = i.saturating_sub(page.first);
        if i < folded.len() && page.count < REGULAR_ENTRIES_MAX {
            page.compressed = false;
            page.count = REGULAR_ENTRIES_MAX.min(folded.len().saturating_sub(page.first));
            i = page.first.saturating_add(page.count);
        } else {
            page.compressed = true;
        }
        pages.push(page);
    }

    let lsda_entries: Vec<usize> = (0..folded.len())
        .filter(|&i| folded.get(i).is_some_and(|e| e.lsda.is_some()))
        .collect();
    let lsda_before = |entry: usize| lsda_entries.partition_point(|&i| i < entry);

    let header = 28usize;
    let common_offset = header;
    let personality_offset = common_offset.saturating_add(common.len().saturating_mul(4));
    let index_offset = personality_offset.saturating_add(personalities.len().saturating_mul(4));
    let index_count = pages.len().saturating_add(1);
    let lsda_offset = index_offset.saturating_add(index_count.saturating_mul(12));
    let pages_offset = lsda_offset.saturating_add(lsda_entries.len().saturating_mul(8));
    let size = pages_offset.saturating_add(pages.len().saturating_mul(PAGE_BYTES));
    let mut out = vec![0u8; size];
    let u32_of = |v: usize| u32::try_from(v).unwrap_or(u32::MAX);
    let offset32 = |address: u64| u32::try_from(address.saturating_sub(base)).unwrap_or(u32::MAX);

    let put = |out: &mut Vec<u8>, at: usize, value: u32| -> Result<()> {
        put32(out, at, value).ok_or_else(|| Error::Internal("__unwind_info overflow".into()))
    };
    put(&mut out, 0, 1)?;
    put(&mut out, 4, u32_of(common_offset))?;
    put(&mut out, 8, u32_of(common.len()))?;
    put(&mut out, 12, u32_of(personality_offset))?;
    put(&mut out, 16, u32_of(personalities.len()))?;
    put(&mut out, 20, u32_of(index_offset))?;
    put(&mut out, 24, u32_of(index_count))?;
    for (i, &(encoding, _)) in common.iter().enumerate() {
        put(
            &mut out,
            common_offset.saturating_add(i.saturating_mul(4)),
            encoding,
        )?;
    }
    for (i, &personality) in personalities.iter().enumerate() {
        let slot = addresses
            .got(personality)
            .ok_or_else(|| Error::Internal("personality routine without a __got slot".into()))?;
        put(
            &mut out,
            personality_offset.saturating_add(i.saturating_mul(4)),
            offset32(slot),
        )?;
    }
    for (p, page) in pages.iter().enumerate() {
        let at = index_offset.saturating_add(p.saturating_mul(12));
        let first = folded.get(page.first).map_or(0, |e| e.address);
        put(&mut out, at, offset32(first))?;
        put(
            &mut out,
            at.saturating_add(4),
            u32_of(pages_offset.saturating_add(p.saturating_mul(PAGE_BYTES))),
        )?;
        put(
            &mut out,
            at.saturating_add(8),
            u32_of(lsda_offset.saturating_add(lsda_before(page.first).saturating_mul(8))),
        )?;
    }
    let sentinel = index_offset.saturating_add(pages.len().saturating_mul(12));
    put(&mut out, sentinel, offset32(end_boundary))?;
    put(&mut out, sentinel.saturating_add(4), 0)?;
    put(
        &mut out,
        sentinel.saturating_add(8),
        u32_of(lsda_offset.saturating_add(lsda_entries.len().saturating_mul(8))),
    )?;
    for (n, &i) in lsda_entries.iter().enumerate() {
        let Some(entry) = folded.get(i) else {
            continue;
        };
        let at = lsda_offset.saturating_add(n.saturating_mul(8));
        put(&mut out, at, offset32(entry.address))?;
        put(
            &mut out,
            at.saturating_add(4),
            offset32(entry.lsda.unwrap_or(base)),
        )?;
    }
    for (p, page) in pages.iter().enumerate() {
        let at = pages_offset.saturating_add(p.saturating_mul(PAGE_BYTES));
        let entries = folded
            .get(page.first..page.first.saturating_add(page.count))
            .unwrap_or(&[]);
        if page.compressed {
            let base_address = entries.first().map_or(0, |e| e.address);
            put(&mut out, at, UNWIND_SECOND_LEVEL_COMPRESSED)?;
            let entry_offset = 12u32;
            let count = u32_of(entries.len());
            let encodings_offset = entry_offset.saturating_add(count.saturating_mul(4));
            let head = u32::from(u16::try_from(entry_offset).unwrap_or(0))
                | (u32::from(u16::try_from(count).unwrap_or(0)) << 16);
            put(&mut out, at.saturating_add(4), head)?;
            let tail = u32::from(u16::try_from(encodings_offset).unwrap_or(0))
                | (u32::from(u16::try_from(page.local_encodings.len()).unwrap_or(0)) << 16);
            put(&mut out, at.saturating_add(8), tail)?;
            for (k, entry) in entries.iter().enumerate() {
                let index = common_index
                    .get(&entry.encoding)
                    .or_else(|| page.local_indexes.get(&entry.encoding))
                    .copied()
                    .unwrap_or(0);
                let delta = entry.address.saturating_sub(base_address) & COMPRESSED_OFFSET_MASK;
                let value =
                    (u32::try_from(index).unwrap_or(0) << 24) | u32::try_from(delta).unwrap_or(0);
                put(
                    &mut out,
                    at.saturating_add(12).saturating_add(k.saturating_mul(4)),
                    value,
                )?;
            }
            for (k, &encoding) in page.local_encodings.iter().enumerate() {
                put(
                    &mut out,
                    at.saturating_add(to_usize(u64::from(encodings_offset)))
                        .saturating_add(k.saturating_mul(4)),
                    encoding,
                )?;
            }
        } else {
            put(&mut out, at, UNWIND_SECOND_LEVEL_REGULAR)?;
            let head = 8u32 | (u32::from(u16::try_from(entries.len()).unwrap_or(0)) << 16);
            put(&mut out, at.saturating_add(4), head)?;
            for (k, entry) in entries.iter().enumerate() {
                let slot = at.saturating_add(8).saturating_add(k.saturating_mul(8));
                put(&mut out, slot, offset32(entry.address))?;
                put(&mut out, slot.saturating_add(4), entry.encoding)?;
            }
        }
    }
    // Trim the last page to what it uses, as lld does not: keep full pages
    // so that offsets stay simple.
    let _ = CPU_TYPE_X86_64;
    Ok(out)
}

/// Writes `contents` into the image at the `__unwind_info` section.
///
/// # Errors
///
/// [`Error::Internal`] when the section is missing or too small.
pub fn write(layout: &Layout, contents: &[u8], image: &mut [u8]) -> Result<()> {
    if contents.is_empty() {
        return Ok(());
    }
    let section = layout
        .find(SectionKind::UnwindInfo)
        .ok_or_else(|| Error::Internal("__unwind_info was not laid out".into()))?;
    if to_u64(contents.len()) > section.size {
        return Err(Error::Internal("__unwind_info grew after layout".into()));
    }
    let start = to_usize(section.offset);
    image
        .get_mut(start..start.saturating_add(contents.len()))
        .ok_or_else(|| Error::Internal("__unwind_info outside the image".into()))?
        .copy_from_slice(contents);
    Ok(())
}
