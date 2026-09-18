//! Encoding pointer fixups for dyld: chained fixups
//! (`LC_DYLD_CHAINED_FIXUPS`) or rebase and bind opcodes
//! (`LC_DYLD_INFO_ONLY`).
//!
//! # Chained fixups
//!
//! Every pointer is rewritten in place as a `DYLD_CHAINED_PTR_64` rebase
//! (`target:36 high8:8 reserved:7 next:12 bind:1`, the target being the
//! unslid address, as lld writes it) or bind (`ordinal:24 addend:8
//! reserved:19 next:12 bind:1`, the ordinal indexing the imports table).
//! `next` links each fixup to the next one in the same page, in 4-byte
//! strides; `dyld_chained_starts_in_segment` records the first fixup of
//! every page. Addends that do not fit 8 bits switch the imports table to
//! `DYLD_CHAINED_IMPORT_ADDEND` (or `_ADDEND64`), with one entry per
//! symbol and addend.
//!
//! arm64e uses `DYLD_CHAINED_PTR_ARM64E` (to macOS 11, iOS 14) or
//! `DYLD_CHAINED_PTR_ARM64E_USERLAND24` (8-byte strides, 11-bit `next`),
//! as ld64 does: plain rebases (`target:43 high8:8`, the unslid address or
//! the offset from the image base), authenticated rebases (`target:32
//! diversity:16 addrDiv:1 key:2`, an offset), binds (`ordinal:16|24`, a
//! 19-bit addend) and authenticated binds (`ordinal:16|24` and the signing
//! schema; their addends go to the imports table).
//!
//! # Opcodes
//!
//! The legacy form keeps plain pointers in the image (unslid addresses for
//! rebases, zero for binds) and describes them with the opcode streams of
//! `LC_DYLD_INFO_ONLY`. qld binds everything at load time, so the lazy
//! binding stream is empty. A pointer to an exported weak definition (a
//! weak-lookup import) is rebased to the image's own definition and also
//! listed, sorted by symbol name, in the weak binding stream, so that dyld
//! can redirect it to the definition that wins across images.

#![deny(clippy::arithmetic_side_effects)]

use hashbrown::HashMap;

use crate::error::{Error, Result};
use crate::macho::read::consts::{
    BIND_OPCODE_DO_BIND, BIND_OPCODE_DONE, BIND_OPCODE_SET_ADDEND_SLEB,
    BIND_OPCODE_SET_DYLIB_ORDINAL_IMM, BIND_OPCODE_SET_DYLIB_ORDINAL_ULEB,
    BIND_OPCODE_SET_DYLIB_SPECIAL_IMM, BIND_OPCODE_SET_SEGMENT_AND_OFFSET_ULEB,
    BIND_OPCODE_SET_SYMBOL_TRAILING_FLAGS_IMM, BIND_OPCODE_SET_TYPE_IMM,
    BIND_SPECIAL_DYLIB_WEAK_LOOKUP, BIND_SYMBOL_FLAGS_WEAK_IMPORT, BIND_TYPE_POINTER,
    DYLD_CHAINED_IMPORT, DYLD_CHAINED_IMPORT_ADDEND, DYLD_CHAINED_IMPORT_ADDEND64,
    DYLD_CHAINED_PTR_64, DYLD_CHAINED_PTR_ARM64E, DYLD_CHAINED_PTR_ARM64E_USERLAND24,
    DYLD_CHAINED_PTR_START_NONE, REBASE_OPCODE_DO_REBASE_IMM_TIMES, REBASE_OPCODE_DONE,
    REBASE_OPCODE_SET_SEGMENT_AND_OFFSET_ULEB, REBASE_OPCODE_SET_TYPE_IMM, REBASE_TYPE_POINTER,
};

use super::buf::{pad_to, push_sleb, push_uleb, push16, push32, push64, to_u64, to_usize};
use super::layout::Layout;
use super::reloc::{Fixup, FixupKind, PtrAuth};
use super::scan::Import;

fn locate(layout: &Layout, address: u64) -> Result<(usize, u64, u64)> {
    let (segment, offset) = layout.segment_of(address).ok_or_else(|| {
        Error::Internal(format!("fixup at {address:#x} is outside every segment"))
    })?;
    let file = layout
        .file_offset(address)
        .ok_or_else(|| Error::Internal(format!("fixup at {address:#x} has no file contents")))?;
    Ok((segment, offset, file))
}

/// Encodes chained fixups: rewrites every pointer of `fixups` in `image`
/// (the whole output file) and returns the `LC_DYLD_CHAINED_FIXUPS` blob.
///
/// `pointer_format` is `DYLD_CHAINED_PTR_64`, or for arm64e
/// `DYLD_CHAINED_PTR_ARM64E` (switched to `_USERLAND24` past 65535
/// imports, as ld64 does) or `DYLD_CHAINED_PTR_ARM64E_USERLAND24`.
///
/// # Errors
///
/// [`Error::Limit`] for fixups that cannot be chained (misaligned, or a
/// target out of the format's reach) and [`Error::Internal`] for fixups
/// outside the image.
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
pub fn chained(
    layout: &Layout,
    image_base: u64,
    page_size: u64,
    pointer_format: u16,
    fixups: &[Fixup],
    imports: &[Import],
    image: &mut [u8],
) -> Result<Vec<u8>> {
    let arm64e = pointer_format != DYLD_CHAINED_PTR_64;
    // The addend a bind carries in the pointer itself, if it can: 8 bits
    // (`DYLD_CHAINED_PTR_64`), or 19 signed bits for arm64e binds that are
    // not authenticated (authenticated binds have no addend field).
    let inline_room = |fixup: &Fixup, addend: i64| {
        if arm64e {
            if fixup.auth.is_some() {
                addend == 0
            } else {
                (-(1 << 18)..1 << 18).contains(&addend)
            }
        } else {
            (0..=255).contains(&addend)
        }
    };
    // The imports table.
    let wide_addend = fixups
        .iter()
        .any(|f| matches!(f.kind, FixupKind::Bind { addend, .. } if !inline_room(f, addend)));
    let huge_addend = fixups.iter().any(
        |f| matches!(f.kind, FixupKind::Bind { addend, .. } if i32::try_from(addend).is_err()),
    );
    let format = if huge_addend {
        DYLD_CHAINED_IMPORT_ADDEND64
    } else if wide_addend {
        DYLD_CHAINED_IMPORT_ADDEND
    } else {
        DYLD_CHAINED_IMPORT
    };
    // The addend a bind keeps inline, and its key in the table.
    let inline = |fixup: &Fixup, import: u32, addend: i64| -> (Option<i64>, (u32, i64)) {
        if format == DYLD_CHAINED_IMPORT || (arm64e && inline_room(fixup, addend)) {
            (Some(addend), (import, 0))
        } else {
            (None, (import, addend))
        }
    };
    // (import, addend) -> table index.
    let mut table: Vec<(u32, i64)> = Vec::new();
    let mut index_of: HashMap<(u32, i64), u32> = HashMap::new();
    if format == DYLD_CHAINED_IMPORT {
        for index in 0..imports.len() {
            let index =
                u32::try_from(index).map_err(|_| Error::Limit("too many imports".into()))?;
            index_of.insert((index, 0), index);
            table.push((index, 0));
        }
    } else {
        let mut keys: Vec<(u32, i64)> = fixups
            .iter()
            .filter_map(|f| match f.kind {
                FixupKind::Bind { import, addend } => Some(inline(f, import, addend).1),
                FixupKind::Rebase(_) => None,
            })
            .collect();
        keys.sort_unstable();
        keys.dedup();
        for key in keys {
            let index =
                u32::try_from(table.len()).map_err(|_| Error::Limit("too many imports".into()))?;
            index_of.insert(key, index);
            table.push(key);
        }
    }
    let pointer_format = if pointer_format == DYLD_CHAINED_PTR_ARM64E && table.len() > 0xffff {
        DYLD_CHAINED_PTR_ARM64E_USERLAND24
    } else {
        pointer_format
    };
    if table.len() >= 1 << 24 {
        return Err(Error::Limit("more than 2^24 chained imports".into()));
    }
    // Chains step in 4-byte units with 12 bits of `next`, or 8-byte units
    // with 11 bits on arm64e.
    let (stride, max_next) = if arm64e { (8u64, 2047u64) } else { (4, 4095) };

    // Fixups per segment and page, sorted by address.
    let mut sorted: Vec<&Fixup> = fixups.iter().collect();
    sorted.sort_by_key(|f| f.address);
    let mut located = Vec::with_capacity(sorted.len());
    for fixup in &sorted {
        let (segment, offset, file) = locate(layout, fixup.address)?;
        located.push((segment, offset, file, **fixup));
    }
    for (index, &(segment, offset, file, fixup)) in located.iter().enumerate() {
        let next = located
            .get(index.saturating_add(1))
            .filter(|n| {
                n.0 == segment && n.1.checked_div(page_size) == offset.checked_div(page_size)
            })
            .map_or(Ok(0u64), |n| {
                let distance = n.1.saturating_sub(offset);
                match (distance.checked_rem(stride), distance.checked_div(stride)) {
                    (Some(0), Some(next)) if next <= max_next => Ok(next),
                    _ => Err(Error::Limit(format!(
                        "pointers at {:#x} and {:#x} cannot be chained (misaligned)",
                        fixup.address, n.3.address
                    ))),
                }
            })?;
        let ordinal_of = |import: u32, addend: i64| -> Result<(u64, u64)> {
            let (inline_addend, key) = inline(&fixup, import, addend);
            let ordinal = u64::from(*index_of.get(&key).ok_or_else(|| {
                Error::Internal("bind to an import missing from the table".into())
            })?);
            Ok((ordinal, inline_addend.unwrap_or(0) as u64))
        };
        let value = if arm64e {
            encode_arm64e(pointer_format, image_base, fixup, next, |import, addend| {
                ordinal_of(import, addend)
            })?
        } else {
            match fixup.kind {
                FixupKind::Rebase(target) => {
                    let high8 = target >> 56;
                    let low = target & 0x00ff_ffff_ffff_ffff;
                    if low >= 1 << 36 {
                        return Err(Error::Limit(format!(
                            "pointer at {:#x} targets {target:#x}, beyond the 64 GiB chained fixups reach",
                            fixup.address
                        )));
                    }
                    low | (high8 << 36) | (next << 51)
                }
                FixupKind::Bind { import, addend } => {
                    let (ordinal, inline) = ordinal_of(import, addend)?;
                    ordinal | ((inline & 0xff) << 24) | (next << 51) | (1 << 63)
                }
            }
        };
        let at = to_usize(file);
        super::buf::put64(image, at, value)
            .ok_or_else(|| Error::Internal("fixup outside the output".into()))?;
    }

    // dyld_chained_fixups_header, 28 bytes padded to 32.
    let mut blob = vec![0u8; 32];
    let starts_offset = 32u32;
    let segment_count = layout.segments.len();
    let mut image_starts = Vec::new();
    push32(&mut image_starts, u32::try_from(segment_count).unwrap_or(0));
    let offsets_at = image_starts.len();
    image_starts.resize(
        offsets_at.saturating_add(segment_count.saturating_mul(4)),
        0,
    );
    pad_to(&mut image_starts, 8);
    for (segment_index, segment) in layout.segments.iter().enumerate() {
        let pages: Vec<(u64, u64)> = located
            .iter()
            .filter(|f| f.0 == segment_index)
            .map(|f| {
                (
                    f.1.checked_div(page_size).unwrap_or(0),
                    f.1.checked_rem(page_size).unwrap_or(0),
                )
            })
            .collect();
        let Some(&(last_page, _)) = pages.last() else {
            continue;
        };
        let offset = u32::try_from(image_starts.len())
            .map_err(|_| Error::Limit("chained fixups too large".into()))?;
        super::buf::put32(
            &mut image_starts,
            offsets_at.saturating_add(segment_index.saturating_mul(4)),
            offset,
        );
        let page_count = last_page.saturating_add(1);
        let mut starts = vec![DYLD_CHAINED_PTR_START_NONE; to_usize(page_count)];
        for &(page, within) in pages.iter().rev() {
            if let Some(slot) = starts.get_mut(to_usize(page)) {
                *slot = u16::try_from(within).unwrap_or(DYLD_CHAINED_PTR_START_NONE);
            }
        }
        let mut record = Vec::new();
        // sizeof(dyld_chained_starts_in_segment) includes one page start.
        let size = 22usize
            .saturating_add(starts.len().saturating_mul(2))
            .next_multiple_of(8);
        push32(&mut record, u32::try_from(size).unwrap_or(0));
        push16(&mut record, u16::try_from(page_size).unwrap_or(0));
        push16(&mut record, pointer_format);
        push64(&mut record, segment.vmaddr.wrapping_sub(image_base));
        push32(&mut record, 0);
        push16(
            &mut record,
            u16::try_from(page_count).map_err(|_| {
                Error::Limit("a segment with more than 65535 pages of fixups".into())
            })?,
        );
        for start in starts {
            push16(&mut record, start);
        }
        pad_to(&mut record, 8);
        image_starts.extend_from_slice(&record);
    }
    let imports_offset =
        starts_offset.saturating_add(u32::try_from(image_starts.len()).unwrap_or(u32::MAX));
    blob.extend_from_slice(&image_starts);

    let mut symbols = Vec::new();
    let mut name_offsets: HashMap<&[u8], u32> = HashMap::new();
    let mut import_bytes = Vec::new();
    for &(import, addend) in &table {
        let Some(entry) = imports.get(to_usize(u64::from(import))) else {
            return Err(Error::Internal("import index out of range".into()));
        };
        let name_offset = match name_offsets.get(entry.name.as_slice()) {
            Some(&offset) => offset,
            None => {
                let offset = u32::try_from(symbols.len())
                    .map_err(|_| Error::Limit("chained import names too large".into()))?;
                symbols.extend_from_slice(&entry.name);
                symbols.push(0);
                name_offsets.insert(&entry.name, offset);
                offset
            }
        };
        let ordinal = entry.ordinal;
        let weak = u64::from(entry.weak);
        if format == DYLD_CHAINED_IMPORT_ADDEND64 {
            let value = (ordinal as u64 & 0xffff) | (weak << 16) | (u64::from(name_offset) << 32);
            push64(&mut import_bytes, value);
            push64(&mut import_bytes, addend as u64);
        } else {
            if name_offset >= 1 << 23 {
                return Err(Error::Limit("chained import names too large".into()));
            }
            let value = (ordinal as u32 & 0xff) | ((weak as u32) << 8) | (name_offset << 9);
            push32(&mut import_bytes, value);
            if format == DYLD_CHAINED_IMPORT_ADDEND {
                push32(&mut import_bytes, addend as i32 as u32);
            }
        }
    }
    blob.extend_from_slice(&import_bytes);
    let symbols_offset =
        u32::try_from(blob.len()).map_err(|_| Error::Limit("chained fixups too large".into()))?;
    blob.extend_from_slice(&symbols);
    pad_to(&mut blob, 8);

    let header = [
        0u32,
        starts_offset,
        imports_offset,
        symbols_offset,
        u32::try_from(table.len()).unwrap_or(0),
        format,
        0,
    ];
    for (index, value) in header.into_iter().enumerate() {
        super::buf::put32(&mut blob, index.saturating_mul(4), value);
    }
    let _ = to_u64(0);
    Ok(blob)
}

/// Encodes one arm64e chained pointer (`dyld_chained_ptr_arm64e_*`):
/// plain rebases carry the unslid address (`DYLD_CHAINED_PTR_ARM64E`) or
/// the offset from the image base (`_USERLAND24`), authenticated rebases
/// a 32-bit offset and the signing schema, binds a 16- or 24-bit ordinal
/// and either a 19-bit addend or the schema.
fn encode_arm64e(
    pointer_format: u16,
    image_base: u64,
    fixup: Fixup,
    next: u64,
    ordinal_of: impl Fn(u32, i64) -> Result<(u64, u64)>,
) -> Result<u64> {
    let schema = |auth: PtrAuth| {
        (u64::from(auth.diversity) << 32)
            | (u64::from(auth.address_diversity) << 48)
            | (u64::from(auth.key & 3) << 49)
    };
    let out_of_reach = |what: &str| {
        Error::Limit(format!(
            "pointer at {:#x}: {what} out of the reach of arm64e chained fixups",
            fixup.address
        ))
    };
    Ok(match (fixup.kind, fixup.auth) {
        (FixupKind::Rebase(target), None) => {
            let target = if pointer_format == DYLD_CHAINED_PTR_ARM64E {
                target
            } else {
                target.wrapping_sub(image_base)
            };
            let high8 = target >> 56;
            let low = target & 0x00ff_ffff_ffff_ffff;
            if low >= 1 << 43 {
                return Err(out_of_reach("target"));
            }
            low | (high8 << 43) | (next << 51)
        }
        (FixupKind::Rebase(target), Some(auth)) => {
            let offset = target
                .checked_sub(image_base)
                .filter(|&o| o < 1 << 32)
                .ok_or_else(|| out_of_reach("authenticated target"))?;
            offset | schema(auth) | (next << 51) | (1 << 63)
        }
        (FixupKind::Bind { import, addend }, auth) => {
            let (ordinal, inline) = ordinal_of(import, addend)?;
            let ordinal_bits = if pointer_format == DYLD_CHAINED_PTR_ARM64E {
                16
            } else {
                24
            };
            if ordinal >= 1 << ordinal_bits {
                return Err(out_of_reach("import ordinal"));
            }
            match auth {
                Some(auth) => ordinal | schema(auth) | (next << 51) | (1 << 62) | (1 << 63),
                None => ordinal | ((inline & 0x7_ffff) << 32) | (next << 51) | (1 << 62),
            }
        }
    })
}

/// The opcode streams of `LC_DYLD_INFO_ONLY`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Opcodes {
    /// Rebase opcodes.
    pub rebase: Vec<u8>,
    /// Bind opcodes.
    pub bind: Vec<u8>,
    /// Weak binding opcodes.
    pub weak_bind: Vec<u8>,
}

/// The rebase, bind and weak binding opcode streams of
/// `LC_DYLD_INFO_ONLY`. Also writes the plain pointer values into `image`.
///
/// `weak_targets` gives, for each weak-lookup import, the address of the
/// image's own definition.
///
/// # Errors
///
/// [`Error::Internal`] for fixups outside the image.
pub fn opcodes(
    layout: &Layout,
    fixups: &[Fixup],
    imports: &[Import],
    weak_targets: &[Option<u64>],
    image: &mut [u8],
) -> Result<Opcodes> {
    let mut sorted: Vec<&Fixup> = fixups.iter().collect();
    sorted.sort_by_key(|f| f.address);
    let mut rebase = Vec::new();
    let mut bind = Vec::new();
    let mut rebase_started = false;
    let mut current_bind: Option<(i32, u32, i64)> = None;
    // (name, segment, offset, addend) of each weak binding.
    let mut weak: Vec<(&[u8], u8, u64, i64)> = Vec::new();
    for fixup in sorted {
        let (segment, offset, file) = locate(layout, fixup.address)?;
        let segment = u8::try_from(segment)
            .ok()
            .filter(|&s| s < 16)
            .ok_or_else(|| Error::Limit("fixup in segment 16 or later".into()))?;
        let mut kind = fixup.kind;
        if let FixupKind::Bind { import, addend } = kind {
            let entry = imports
                .get(to_usize(u64::from(import)))
                .ok_or_else(|| Error::Internal("import index out of range".into()))?;
            if entry.ordinal == BIND_SPECIAL_DYLIB_WEAK_LOOKUP {
                let target = weak_targets
                    .get(to_usize(u64::from(import)))
                    .copied()
                    .flatten()
                    .ok_or_else(|| {
                        Error::Internal(format!(
                            "weak binding of {} without a definition",
                            String::from_utf8_lossy(&entry.name)
                        ))
                    })?;
                weak.push((&entry.name, segment, offset, addend));
                kind = FixupKind::Rebase(target.wrapping_add(addend as u64));
            }
        }
        match kind {
            FixupKind::Rebase(target) => {
                super::buf::put64(image, to_usize(file), target)
                    .ok_or_else(|| Error::Internal("fixup outside the output".into()))?;
                if !rebase_started {
                    rebase.push(REBASE_OPCODE_SET_TYPE_IMM | REBASE_TYPE_POINTER);
                    rebase_started = true;
                }
                rebase.push(REBASE_OPCODE_SET_SEGMENT_AND_OFFSET_ULEB | segment);
                push_uleb(&mut rebase, offset);
                rebase.push(REBASE_OPCODE_DO_REBASE_IMM_TIMES | 1);
            }
            FixupKind::Bind { import, addend } => {
                super::buf::put64(image, to_usize(file), 0)
                    .ok_or_else(|| Error::Internal("fixup outside the output".into()))?;
                let entry = imports
                    .get(to_usize(u64::from(import)))
                    .ok_or_else(|| Error::Internal("import index out of range".into()))?;
                let same_symbol = current_bind.is_some_and(|(_, i, _)| i == import);
                if current_bind.map(|(o, _, _)| o) != Some(entry.ordinal) {
                    match entry.ordinal {
                        ordinal @ 0..=15 => {
                            bind.push(BIND_OPCODE_SET_DYLIB_ORDINAL_IMM | ordinal as u8);
                        }
                        ordinal if ordinal > 15 => {
                            bind.push(BIND_OPCODE_SET_DYLIB_ORDINAL_ULEB);
                            push_uleb(&mut bind, ordinal as u64);
                        }
                        special => {
                            bind.push(BIND_OPCODE_SET_DYLIB_SPECIAL_IMM | (special as u8 & 0x0f));
                        }
                    }
                }
                if !same_symbol {
                    let flags = if entry.weak {
                        BIND_SYMBOL_FLAGS_WEAK_IMPORT
                    } else {
                        0
                    };
                    bind.push(BIND_OPCODE_SET_SYMBOL_TRAILING_FLAGS_IMM | flags);
                    bind.extend_from_slice(&entry.name);
                    bind.push(0);
                    if current_bind.is_none() {
                        bind.push(BIND_OPCODE_SET_TYPE_IMM | BIND_TYPE_POINTER);
                    }
                }
                if current_bind.map(|(_, _, a)| a) != Some(addend) {
                    bind.push(BIND_OPCODE_SET_ADDEND_SLEB);
                    push_sleb(&mut bind, addend);
                }
                bind.push(BIND_OPCODE_SET_SEGMENT_AND_OFFSET_ULEB | segment);
                push_uleb(&mut bind, offset);
                bind.push(BIND_OPCODE_DO_BIND);
                current_bind = Some((entry.ordinal, import, addend));
            }
        }
    }
    if rebase_started {
        rebase.push(REBASE_OPCODE_DONE);
        pad_to(&mut rebase, 8);
    }
    if current_bind.is_some() {
        bind.push(BIND_OPCODE_DONE);
        pad_to(&mut bind, 8);
    }
    Ok(Opcodes {
        rebase,
        bind,
        weak_bind: weak_bind_opcodes(weak),
    })
}

/// The weak binding stream: entries sorted by symbol name, as dyld walks
/// the streams of every image in step.
fn weak_bind_opcodes(mut entries: Vec<(&[u8], u8, u64, i64)>) -> Vec<u8> {
    let mut out = Vec::new();
    if entries.is_empty() {
        return out;
    }
    entries.sort_unstable();
    out.push(BIND_OPCODE_SET_TYPE_IMM | BIND_TYPE_POINTER);
    let mut current: Option<(&[u8], i64)> = None;
    for (name, segment, offset, addend) in entries {
        if current.map(|(n, _)| n) != Some(name) {
            out.push(BIND_OPCODE_SET_SYMBOL_TRAILING_FLAGS_IMM);
            out.extend_from_slice(name);
            out.push(0);
        }
        if current.map(|(_, a)| a) != Some(addend) {
            out.push(BIND_OPCODE_SET_ADDEND_SLEB);
            push_sleb(&mut out, addend);
        }
        out.push(BIND_OPCODE_SET_SEGMENT_AND_OFFSET_ULEB | segment);
        push_uleb(&mut out, offset);
        out.push(BIND_OPCODE_DO_BIND);
        current = Some((name, addend));
    }
    out.push(BIND_OPCODE_DONE);
    pad_to(&mut out, 8);
    out
}
