//! `__TEXT,__eh_frame`: the DWARF call frame information of functions whose
//! unwinding compact unwind cannot describe.
//!
//! Each object's `__eh_frame` is split into its CIEs and FDEs. An FDE is
//! kept when its function is live and has no compact unwind record (or one
//! that defers to DWARF); a CIE is kept when a kept FDE uses it. The kept
//! records are concatenated in input order, and their pointers rewritten:
//!
//! - the FDE's CIE pointer, to the CIE's new position;
//! - `pc_begin` and the LSDA pointer, to the output addresses of their
//!   targets (PC-relative, as Darwin compilers emit them);
//! - the CIE's personality pointer, to the personality's `__got` slot.
//!
//! Targets come from the fields' relocations: a `SUBTRACTOR`/`UNSIGNED` pair
//! for PC-relative fields (whose stored value is relative to the
//! subtracted symbol, so it is adjusted by that symbol's distance from the
//! field), or `X86_64_RELOC_GOT` / `ARM64_RELOC_POINTER_TO_GOT` for the
//! personality. Fields without relocations are object addresses, which the
//! reader decodes.

#![deny(clippy::arithmetic_side_effects)]

use crate::error::{Error, Result};
use crate::ids::SymbolId;
use crate::macho::read::consts::{ARM64_RELOC_POINTER_TO_GOT, N_SECT, X86_64_RELOC_GOT};
use crate::macho::read::eh_frame::DW_EH_PE_PCREL;
use crate::macho::read::{EhFrame, EhFrameKind, EhPointer};

use super::addr::Addresses;
use super::buf::{to_u64, to_usize};
use super::layout::{Layout, SectionKind};
use super::object::LinkObject;
use super::reloc::{self, Place, Referent, Resolve, Value};
use super::state::{Link, SymbolDef};
use super::unwind::{Entries, Entry};

/// What a pointer field of a record refers to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    /// A place in the image.
    Place(Place),
    /// The `__got` slot of a symbol, with the relocation's addend (x86_64
    /// stores the 4 bytes of PC adjustment there).
    Got(SymbolId, i64),
}

/// One record kept in the output.
#[derive(Clone, Copy, Debug)]
struct Piece {
    file: usize,
    record: usize,
    offset: u64,
    /// For an FDE, the output offset of its CIE.
    cie: Option<u64>,
}

/// The planned `__eh_frame` contents.
#[derive(Clone, Debug, Default)]
pub struct EhFramePlan {
    pieces: Vec<Piece>,
    size: u64,
}

impl EhFramePlan {
    /// Size of the section.
    #[must_use]
    pub fn size(&self) -> u64 {
        self.size
    }
}

/// The value of symbol table entry `symbol` of `object`, when defined in it.
fn object_value(
    link: &Link<'_>,
    file: usize,
    object: &LinkObject<'_>,
    referent: Referent,
) -> Option<u64> {
    match referent {
        Referent::Local(symbol) => {
            let entry = object.file.symbols().get(symbol).ok()?;
            (entry.n_type & 0x0e == N_SECT).then_some(entry.n_value)
        }
        Referent::Global(id) => match link.defs.get(id.index())? {
            SymbolDef::Object {
                file: def_file,
                symbol,
            } if usize::try_from(*def_file).ok() == Some(file) => {
                object.file.symbols().get(*symbol).ok().map(|e| e.n_value)
            }
            _ => None,
        },
        Referent::Address { address, .. } => Some(address),
    }
}

/// Resolves pointer field `pointer` of the `__eh_frame` section `section`.
///
/// # Errors
///
/// Malformed relocations.
pub fn pointer_target(
    link: &Link<'_>,
    file: usize,
    object: &LinkObject<'_>,
    section: usize,
    data: &[u8],
    pointer: &EhPointer,
) -> Result<Option<Target>> {
    let arm64 = link.config.is_arm64();
    let Some(header) = object.file.sections().get(section) else {
        return Ok(None);
    };
    let field = header.addr.wrapping_add(to_u64(pointer.offset));
    match pointer.relocation {
        Some(relocation) => {
            let decoded = reloc::decode(link, file, object, section, data, &relocation)?;
            let is_got = (arm64 && decoded.r_type == ARM64_RELOC_POINTER_TO_GOT)
                || (!arm64 && decoded.r_type == X86_64_RELOC_GOT);
            if is_got {
                return match decoded.referent {
                    Referent::Global(id) => Ok(Some(Target::Got(id, decoded.addend))),
                    _ => Err(object
                        .file
                        .source()
                        .malformed(0, "__eh_frame GOT relocation against a local symbol")),
                };
            }
            match decoded.subtrahend {
                Some(subtrahend) => {
                    let base = object_value(link, file, object, subtrahend).ok_or_else(|| {
                        object.file.source().malformed(
                            0,
                            "__eh_frame SUBTRACTOR against a symbol defined elsewhere",
                        )
                    })?;
                    let adjust = field.wrapping_sub(base) as i64;
                    let place = match decoded.referent {
                        Referent::Address { section, address } => reloc::place(
                            link,
                            file,
                            object,
                            Referent::Address {
                                section,
                                address: address.wrapping_add(adjust as u64),
                            },
                            0,
                        )?,
                        referent => reloc::place(
                            link,
                            file,
                            object,
                            referent,
                            decoded.addend.wrapping_add(adjust),
                        )?,
                    };
                    Ok(Some(Target::Place(place)))
                }
                None => Ok(Some(Target::Place(reloc::place(
                    link,
                    file,
                    object,
                    decoded.referent,
                    decoded.addend,
                )?))),
            }
        }
        None => {
            let Some(target) = object.file.section_at_address(pointer.address) else {
                return Ok(None);
            };
            Ok(Some(Target::Place(reloc::place(
                link,
                file,
                object,
                Referent::Address {
                    section: target,
                    address: pointer.address,
                },
                0,
            )?)))
        }
    }
}

/// Parses the `__eh_frame` of `object`, with its section index.
///
/// # Errors
///
/// Malformed records.
pub fn parse<'a>(object: &LinkObject<'a>) -> Result<Option<(usize, EhFrame<'a>)>> {
    let Some((index, _)) = object.file.find_section(b"__TEXT", b"__eh_frame") else {
        return Ok(None);
    };
    Ok(EhFrame::from_object(&object.file)?.map(|frame| (index, frame)))
}

/// Plans `__eh_frame`, and attaches the kept FDEs to the unwind entries of
/// their functions.
///
/// # Errors
///
/// Malformed `__eh_frame` sections.
pub fn plan(link: &Link<'_>, entries: &mut Entries) -> Result<EhFramePlan> {
    let mut plan = EhFramePlan::default();
    let mut offset = 0u64;
    for file in 0..link.files.len() {
        let Some(object) = link.object(file) else {
            continue;
        };
        let Some((section, frame)) = parse(object)? else {
            continue;
        };
        let data = object.file.section_data(section)?;
        // Which FDEs are kept, and their functions.
        let mut kept: Vec<Option<(Place, u64, Option<Target>)>> = vec![None; frame.records.len()];
        let mut cie_used = vec![false; frame.records.len()];
        for (index, record) in frame.records.iter().enumerate() {
            let EhFrameKind::Fde(fde) = record.kind else {
                continue;
            };
            let Some(Target::Place(function)) =
                pointer_target(link, file, object, section, data, &fde.pc_begin)?
            else {
                continue;
            };
            let Place::Atom {
                atom,
                offset: within,
            } = function
            else {
                continue;
            };
            if !link.live.get(atom).copied().unwrap_or(false)
                || entries.compact.contains_key(&(atom, within))
            {
                continue;
            }
            let lsda = match &fde.lsda {
                Some(pointer) => pointer_target(link, file, object, section, data, pointer)?,
                None => None,
            };
            if let Some(slot) = kept.get_mut(index) {
                *slot = Some((function, fde.pc_range, lsda));
            }
            if let Some(slot) = cie_used.get_mut(fde.cie_index) {
                *slot = true;
            }
        }
        let mut cie_offsets = vec![0u64; frame.records.len()];
        for (index, record) in frame.records.iter().enumerate() {
            let keep = match record.kind {
                EhFrameKind::Cie(_) => cie_used.get(index).copied().unwrap_or(false),
                EhFrameKind::Fde(_) => kept.get(index).is_some_and(Option::is_some),
            };
            if !keep {
                continue;
            }
            let cie = match record.kind {
                EhFrameKind::Cie(_) => {
                    if let Some(slot) = cie_offsets.get_mut(index) {
                        *slot = offset;
                    }
                    None
                }
                EhFrameKind::Fde(fde) => cie_offsets.get(fde.cie_index).copied(),
            };
            plan.pieces.push(Piece {
                file,
                record: index,
                offset,
                cie,
            });
            if let Some(Some((function, length, lsda))) = kept.get(index) {
                let lsda = match lsda {
                    Some(Target::Place(place)) => Some(*place),
                    _ => None,
                };
                attach(entries, *function, *length, lsda, offset);
            }
            offset = offset.saturating_add(to_u64(record.data.len()));
        }
    }
    plan.size = offset;
    Ok(plan)
}

fn attach(entries: &mut Entries, function: Place, length: u64, lsda: Option<Place>, fde: u64) {
    let length = u32::try_from(length).unwrap_or(u32::MAX);
    if let Some(entry) = entries.entries.iter_mut().find(|e| e.function == function) {
        entry.length = length;
        entry.lsda = lsda;
        entry.fde = Some(fde);
        entry.personality = None;
        return;
    }
    entries.entries.push(Entry {
        function,
        length,
        encoding: 0,
        personality: None,
        lsda,
        fde: Some(fde),
    });
}

fn write_pointer(
    out: &mut [u8],
    at: usize,
    pointer: &EhPointer,
    target: u64,
    field: u64,
) -> Result<()> {
    if pointer.encoding & 0x70 != DW_EH_PE_PCREL {
        return Err(Error::Unimplemented(format!(
            "__eh_frame pointer encoding {:#x} (only PC-relative pointers are supported)",
            pointer.encoding
        )));
    }
    let delta = target.wrapping_sub(field);
    match pointer.size {
        4 => {
            let value = i32::try_from(delta as i64).map_err(|_| {
                Error::Limit(format!("__eh_frame pointer at {field:#x} out of range"))
            })?;
            super::buf::put32(out, at, value as u32)
        }
        8 => super::buf::put64(out, at, delta),
        _ => None,
    }
    .ok_or_else(|| Error::Internal("__eh_frame pointer outside the section".into()))
}

/// Writes `__eh_frame` into the image.
///
/// # Errors
///
/// Unsupported pointer encodings and out-of-range values.
pub fn write(addresses: &Addresses<'_, '_>, plan: &EhFramePlan, image: &mut [u8]) -> Result<()> {
    if plan.pieces.is_empty() {
        return Ok(());
    }
    let layout: &Layout = addresses.layout;
    let link = addresses.link;
    let section = layout
        .find(SectionKind::EhFrame)
        .ok_or_else(|| Error::Internal("__eh_frame was not laid out".into()))?;
    let start = to_usize(section.offset);
    let out = image
        .get_mut(start..start.saturating_add(to_usize(section.size)))
        .ok_or_else(|| Error::Internal("__eh_frame outside the image".into()))?;
    let mut current: Option<(usize, usize, EhFrame<'_>)> = None;
    for piece in &plan.pieces {
        let Some(object) = link.object(piece.file) else {
            continue;
        };
        if current.as_ref().map(|c| c.0) != Some(piece.file) {
            let Some((index, frame)) = parse(object)? else {
                continue;
            };
            current = Some((piece.file, index, frame));
        }
        let Some((_, section_index, frame)) = &current else {
            continue;
        };
        let data = object.file.section_data(*section_index)?;
        let Some(record) = frame.records.get(piece.record) else {
            continue;
        };
        let at = to_usize(piece.offset);
        out.get_mut(at..at.saturating_add(record.data.len()))
            .ok_or_else(|| Error::Internal("__eh_frame record outside the section".into()))?
            .copy_from_slice(record.data);
        let field_address = |pointer: &EhPointer| {
            section
                .addr
                .saturating_add(piece.offset)
                .saturating_add(to_u64(pointer.offset.saturating_sub(record.offset)))
        };
        let field_at =
            |pointer: &EhPointer| at.saturating_add(pointer.offset.saturating_sub(record.offset));
        let resolve = |target: Option<Target>, field: u64| -> Result<Option<u64>> {
            Ok(match target {
                Some(Target::Place(place)) => match addresses.value(place, 0)? {
                    Value::Address(address) | Value::Absolute(address) => Some(address),
                    Value::Import(..) => None,
                },
                Some(Target::Got(id, addend)) => {
                    let slot = addresses.got(id).ok_or_else(|| {
                        Error::Internal("__eh_frame personality without a __got slot".into())
                    })?;
                    // The x86_64 GOT relocation is relative to the end of
                    // the field; its addend compensates.
                    let arm64 = link.config.is_arm64();
                    let adjust = if arm64 { 0 } else { addend.wrapping_sub(4) };
                    let _ = field;
                    Some(slot.wrapping_add(adjust as u64))
                }
                None => None,
            })
        };
        match record.kind {
            EhFrameKind::Cie(cie) => {
                if let Some(personality) = &cie.personality {
                    let target = pointer_target(
                        link,
                        piece.file,
                        object,
                        *section_index,
                        data,
                        personality,
                    )?;
                    let field = field_address(personality);
                    if let Some(address) = resolve(target, field)? {
                        write_pointer(out, field_at(personality), personality, address, field)?;
                    }
                }
            }
            EhFrameKind::Fde(fde) => {
                let cie = piece.cie.unwrap_or(0);
                let pointer_field = piece.offset.saturating_add(to_u64(record.length_size));
                let delta = u32::try_from(pointer_field.saturating_sub(cie))
                    .map_err(|_| Error::Limit("__eh_frame CIE pointer out of range".into()))?;
                super::buf::put32(out, at.saturating_add(record.length_size), delta)
                    .ok_or_else(|| Error::Internal("__eh_frame record too short".into()))?;
                for pointer in [Some(&fde.pc_begin), fde.lsda.as_ref()]
                    .into_iter()
                    .flatten()
                {
                    let target =
                        pointer_target(link, piece.file, object, *section_index, data, pointer)?;
                    let field = field_address(pointer);
                    if let Some(address) = resolve(target, field)? {
                        write_pointer(out, field_at(pointer), pointer, address, field)?;
                    }
                }
            }
        }
    }
    Ok(())
}
