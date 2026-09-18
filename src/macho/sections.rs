//! Writing section contents: input atoms with their relocations applied,
//! and the synthetic sections (`__stubs`, `__got`, `__thread_ptrs`,
//! `-sectcreate`). Sections are written in parallel; the pointer fixups
//! they need are collected and returned in address order.

#![deny(clippy::arithmetic_side_effects)]

use rayon::prelude::*;

use crate::arch::aarch64;
use crate::error::{Error, Result};
use crate::macho::read::consts::SECTION_TYPE;

use super::addr::Addresses;
use super::buf::{put64, to_usize};
use super::layout::{OutSection, SectionKind};
use super::reloc::{self, Fixup, FixupKind, Resolve, Value};

/// Splits `image` into the file ranges of the sections with contents.
fn split<'i>(sections: &[OutSection], image: &'i mut [u8]) -> Result<Vec<(usize, &'i mut [u8])>> {
    let mut ranges: Vec<(usize, u64, u64)> = sections
        .iter()
        .enumerate()
        .filter(|(_, s)| !s.is_zerofill() && s.size > 0)
        .map(|(i, s)| (i, s.offset, s.size))
        .collect();
    ranges.sort_by_key(|r| r.1);
    let mut out = Vec::with_capacity(ranges.len());
    let mut rest = image;
    let mut consumed = 0u64;
    for (index, offset, size) in ranges {
        let skip = offset
            .checked_sub(consumed)
            .ok_or_else(|| Error::Internal("overlapping output sections".into()))?;
        let (_, tail) = rest.split_at_mut(to_usize(skip).min(rest.len()));
        if to_usize(size) > tail.len() {
            return Err(Error::Internal("section past the end of the output".into()));
        }
        let (slice, tail) = tail.split_at_mut(to_usize(size));
        out.push((index, slice));
        rest = tail;
        consumed = offset.saturating_add(size);
    }
    Ok(out)
}

/// Writes every section with contents into `image` and returns the pointer
/// fixups, sorted by address.
///
/// # Errors
///
/// Relocation errors.
pub fn write(
    addresses: &Addresses<'_, '_>,
    sectcreate: &[Vec<u8>],
    image: &mut [u8],
) -> Result<Vec<Fixup>> {
    let layout = addresses.layout;
    let slices = split(&layout.sections, image)?;
    let results: Vec<Result<Vec<Fixup>>> = slices
        .into_par_iter()
        .map(|(index, out)| {
            let Some(section) = layout.sections.get(index) else {
                return Ok(Vec::new());
            };
            match section.kind {
                SectionKind::Input => write_input(addresses, index, section, out),
                SectionKind::Stubs => write_stubs(addresses, section, out).map(|()| Vec::new()),
                SectionKind::Got => {
                    let mut fixups =
                        write_pointers(addresses, section, &addresses.synthetic.got, out)?;
                    fixups.extend(write_local_pointers(addresses, section, out)?);
                    Ok(fixups)
                }
                SectionKind::ThreadPtrs => {
                    write_pointers(addresses, section, &addresses.synthetic.thread_ptrs, out)
                }
                SectionKind::Sectcreate(i) => {
                    if let Some(data) = sectcreate.get(i)
                        && let Some(slot) = out.get_mut(..data.len())
                    {
                        slot.copy_from_slice(data);
                    }
                    Ok(Vec::new())
                }
                SectionKind::Common | SectionKind::UnwindInfo | SectionKind::EhFrame => {
                    Ok(Vec::new())
                }
            }
        })
        .collect();
    let mut fixups = Vec::new();
    for result in results {
        fixups.extend(result?);
    }
    fixups.sort_by_key(|f| f.address);
    Ok(fixups)
}

fn write_input(
    addresses: &Addresses<'_, '_>,
    index: usize,
    section: &OutSection,
    out: &mut [u8],
) -> Result<Vec<Fixup>> {
    let link = addresses.link;
    let layout = addresses.layout;
    let arm64 = link.config.is_arm64();
    let index32 = u32::try_from(index).unwrap_or(u32::MAX);
    let mut fixups = Vec::new();
    if arm64 && section.has_code() {
        super::thunks::write(addresses.thunks, section.addr, out)?;
    }
    for &(file, input) in &section.inputs {
        let file = to_usize(u64::from(file));
        let input = to_usize(u64::from(input));
        let Some(object) = link.object(file) else {
            continue;
        };
        let Some(header) = object.file.sections().get(input) else {
            continue;
        };
        let data = object.file.section_data(input)?;
        let Some(range) = object.atoms.section_range(input) else {
            continue;
        };
        for atom in range {
            let id = link.atom_id(file, atom);
            if layout.atom_section.get(id) != Some(&index32) {
                continue;
            }
            let Some(info) = object.atoms.atoms().get(atom) else {
                continue;
            };
            let offset = to_usize(layout.atom_offset.get(id).copied().unwrap_or(0));
            if let Some(rewrite) = link.objc.rewrite(id) {
                if let Some(slot) = out.get_mut(offset..offset.saturating_add(rewrite.bytes.len()))
                {
                    slot.copy_from_slice(&rewrite.bytes);
                }
                fixups.extend(write_fields(addresses, section, rewrite, offset, out)?);
                continue;
            }
            let source = data
                .get(to_usize(info.offset)..to_usize(info.offset.saturating_add(info.size)))
                .unwrap_or(&[]);
            if let Some(slot) = out.get_mut(offset..offset.saturating_add(source.len())) {
                slot.copy_from_slice(source);
            }
        }
        let Some(relocations) = object.relocations.get(input) else {
            continue;
        };
        for relocation in relocations {
            let id = link.atom_id(file, relocation.atom);
            if layout.atom_section.get(id) != Some(&index32) || link.objc.rewrite(id).is_some() {
                continue;
            }
            let decoded = reloc::decode(link, file, object, input, data, &relocation.relocation)?;
            let Some(info) = object.atoms.atoms().get(relocation.atom) else {
                continue;
            };
            let within = decoded.offset.saturating_sub(info.offset);
            let at_offset = layout
                .atom_offset
                .get(id)
                .copied()
                .unwrap_or(0)
                .saturating_add(within);
            let target = reloc::place(link, file, object, decoded.referent, decoded.addend)?;
            let subtrahend = match decoded.subtrahend {
                Some(referent) => Some(reloc::place(link, file, object, referent, 0)?),
                None => None,
            };
            let place_address = section.addr.saturating_add(at_offset);
            let fixup = reloc::apply(
                arm64,
                &decoded,
                target,
                subtrahend,
                header.flags & SECTION_TYPE,
                addresses,
                out,
                to_usize(at_offset),
                place_address,
            )
            .map_err(|error| match error {
                Error::Limit(message) | Error::Internal(message) => Error::Limit(format!(
                    "{}: {message}",
                    link.files
                        .get(file)
                        .map_or_else(String::new, |f| f.display())
                )),
                other => other,
            })?;
            fixups.extend(fixup);
        }
    }
    Ok(fixups)
}

/// Fills the pointers and offsets of an Objective-C metadata atom the
/// linker rewrote ([`super::objc`]), at `offset` in the section.
fn write_fields(
    addresses: &Addresses<'_, '_>,
    section: &OutSection,
    rewrite: &super::objc::Rewrite,
    offset: usize,
    out: &mut [u8],
) -> Result<Vec<Fixup>> {
    let mut fixups = Vec::new();
    for field in &rewrite.fields {
        let at = offset.saturating_add(to_usize(field.offset));
        let place = section.addr.saturating_add(at as u64);
        let value = addresses.value(field.target.place, field.target.addend)?;
        match field.kind {
            super::objc::FieldKind::Pointer => {
                let (written, kind) = match value {
                    Value::Address(target) => (target, Some(FixupKind::Rebase(target))),
                    Value::Absolute(value) => (value, None),
                    Value::Import(import, addend) => (0, Some(FixupKind::Bind { import, addend })),
                };
                put64(out, at, written)
                    .ok_or_else(|| Error::Internal("pointer outside its section".into()))?;
                if let Some(kind) = kind {
                    fixups.push(Fixup {
                        address: place,
                        kind,
                    });
                }
            }
            super::objc::FieldKind::Relative => {
                let (Value::Address(target) | Value::Absolute(target)) = value else {
                    return Err(Error::Internal(format!(
                        "relative method list entry at {place:#x} refers to an import"
                    )));
                };
                let delta = i32::try_from(target.wrapping_sub(place) as i64).map_err(|_| {
                    Error::Limit(format!(
                        "relative method list entry at {place:#x} out of range of {target:#x}"
                    ))
                })?;
                super::buf::put32(out, at, delta as u32)
                    .ok_or_else(|| Error::Internal("offset outside its section".into()))?;
            }
        }
    }
    Ok(fixups)
}

fn write_stubs(addresses: &Addresses<'_, '_>, section: &OutSection, out: &mut [u8]) -> Result<()> {
    let arm64 = addresses.link.config.is_arm64();
    let size = addresses.link.config.stub_size();
    for (index, &id) in addresses.synthetic.stubs.iter().enumerate() {
        let stub = section
            .addr
            .saturating_add(size.saturating_mul(u64::try_from(index).unwrap_or(0)));
        let got = addresses
            .got(id)
            .ok_or_else(|| Error::Internal("stub without a GOT slot".into()))?;
        let at = to_usize(stub.saturating_sub(section.addr));
        if arm64 {
            let pages = ((got & !0xfff) as i64).wrapping_sub((stub & !0xfff) as i64);
            let adrp = aarch64::Field::Adrp21
                .encode(aarch64::adrp(16), pages)
                .map_err(|_| Error::Limit("stub out of range of its GOT slot".into()))?;
            let ldr = 0xf940_0210 | (u32::try_from((got & 0xfff) >> 3).unwrap_or(0) << 10);
            for (i, insn) in [adrp, ldr, aarch64::BR_X16].into_iter().enumerate() {
                aarch64::write_insn(out, at.saturating_add(i.saturating_mul(4)), insn)
                    .ok_or_else(|| Error::Internal("stub outside __stubs".into()))?;
            }
        } else {
            let delta = got.wrapping_sub(stub.saturating_add(6)) as i64;
            let delta = i32::try_from(delta)
                .map_err(|_| Error::Limit("stub out of range of its GOT slot".into()))?;
            let slot = out
                .get_mut(at..at.saturating_add(6))
                .ok_or_else(|| Error::Internal("stub outside __stubs".into()))?;
            slot[0] = 0xff;
            slot[1] = 0x25;
            slot[2..6].copy_from_slice(&delta.to_le_bytes());
        }
    }
    Ok(())
}

/// The `__got` slots of local symbols, after the global ones.
fn write_local_pointers(
    addresses: &Addresses<'_, '_>,
    section: &OutSection,
    out: &mut [u8],
) -> Result<Vec<Fixup>> {
    let synthetic = addresses.synthetic;
    let mut fixups = Vec::with_capacity(synthetic.local_got.len());
    for (index, &(file, symbol)) in synthetic.local_got.iter().enumerate() {
        let slot = synthetic.got.len().saturating_add(index);
        let offset = to_usize(8u64.saturating_mul(u64::try_from(slot).unwrap_or(0)));
        let address = section.addr.saturating_add(offset as u64);
        let value = addresses
            .object_symbol(to_usize(u64::from(file)), symbol)
            .ok_or_else(|| {
                Error::Internal("__got slot of a local symbol not in the output".into())
            })?;
        let (written, rebase) = match value {
            Value::Address(target) => (target, true),
            Value::Absolute(value) => (value, false),
            Value::Import(..) => (0, false),
        };
        put64(out, offset, written)
            .ok_or_else(|| Error::Internal("pointer outside its section".into()))?;
        if rebase {
            fixups.push(Fixup {
                address,
                kind: FixupKind::Rebase(written),
            });
        }
    }
    Ok(fixups)
}

fn write_pointers(
    addresses: &Addresses<'_, '_>,
    section: &OutSection,
    symbols: &[crate::ids::SymbolId],
    out: &mut [u8],
) -> Result<Vec<Fixup>> {
    let mut fixups = Vec::with_capacity(symbols.len());
    for (index, &id) in symbols.iter().enumerate() {
        let offset = to_usize(8u64.saturating_mul(u64::try_from(index).unwrap_or(0)));
        let address = section.addr.saturating_add(offset as u64);
        let value = addresses.symbol(id).ok_or_else(|| {
            Error::Internal(format!(
                "pointer to undefined symbol {}",
                String::from_utf8_lossy(addresses.link.symbols.name(id).bytes())
            ))
        })?;
        let (written, kind) = match value {
            Value::Address(target) => (target, Some(FixupKind::Rebase(target))),
            Value::Absolute(value) => (value, None),
            Value::Import(import, addend) => (0, Some(FixupKind::Bind { import, addend })),
        };
        put64(out, offset, written)
            .ok_or_else(|| Error::Internal("pointer outside its section".into()))?;
        if let Some(kind) = kind {
            fixups.push(Fixup { address, kind });
        }
    }
    Ok(fixups)
}
