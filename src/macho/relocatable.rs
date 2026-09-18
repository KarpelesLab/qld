//! Relocatable output (`-r`): the live objects of a link merged into one
//! `MH_OBJECT` for a later link, as ld64 produces it.
//!
//! - Nothing is dead-stripped and undefined symbols stay undefined. Weak
//!   definitions are coalesced (one copy is kept), and literals (C strings,
//!   4-, 8- and 16-byte literals) are merged as in a final link.
//! - Input sections are concatenated by segment and section name, without
//!   the renames of a final link. Every section goes into one unnamed
//!   segment at address 0: `__TEXT` first (`__text` leading, `__eh_frame`
//!   last), then the data segments, other segments, `__LD`, and the
//!   zero-fill sections at the end, as the assemblers order them.
//! - Symbols: the locals of every live object (temporary `l`/`ltmp` labels
//!   included, since relocations name them), then the external
//!   definitions sorted by name, then the undefined and common symbols
//!   sorted by name. Private externs become local symbols unless
//!   `-keep_private_externs` is given.
//! - Relocations keep their form: an external relocation names the same
//!   symbol in the output's table, and a section relocation names the
//!   output section its target moved to. The stored values move with what
//!   they locate: a section relocation's field is adjusted by how far its
//!   target (and, when PC-relative, the field itself) moved, and an
//!   external relocation's addend by how far its target moved relative to
//!   its symbol, which is non-zero only when the addend reaches into
//!   another atom that moved differently. arm64 addends live in
//!   `ARM64_RELOC_ADDEND` entries, which are rewritten (or added) instead.
//! - `__compact_unwind` records of functions that were coalesced away are
//!   dropped. `__eh_frame` is rebuilt from the FDEs of the kept functions
//!   and the CIEs they use: function and LSDA pointers become PC-relative
//!   values without relocations (as x86_64 assemblers write them), and
//!   personality pointers keep their GOT relocations.
//! - `LC_LINKER_OPTION` commands of the inputs are carried over, without
//!   duplicates, for the final link to act on. `MH_SUBSECTIONS_VIA_SYMBOLS`
//!   is set when every input has it.
//!
//! Not carried over (see `docs/compatibility.md`): DWARF sections
//! (`S_ATTR_DEBUG`), whose offsets into each other would need rewriting,
//! STABS, `LC_DATA_IN_CODE` and `LC_LINKER_OPTIMIZATION_HINT`.

#![deny(clippy::arithmetic_side_effects)]

use hashbrown::HashMap;

use crate::args::LinkOptions;
use crate::diag::{Diagnostic, DiagnosticSink};
use crate::error::{Error, Result};
use crate::ids::{FileId, SymbolId};
use crate::macho::read::consts::{
    ARM64_RELOC_ADDEND, ARM64_RELOC_BRANCH26, ARM64_RELOC_UNSIGNED, CPU_SUBTYPE_ARM64_ALL,
    CPU_SUBTYPE_X86_64_ALL, CPU_TYPE_ARM64, CPU_TYPE_X86_64, LC_BUILD_VERSION, LC_DYSYMTAB,
    LC_LINKER_OPTION, LC_SEGMENT_64, LC_SYMTAB, MH_MAGIC_64, MH_OBJECT, MH_SUBSECTIONS_VIA_SYMBOLS,
    N_ABS, N_EXT, N_INDR, N_PEXT, N_SECT, N_TYPE, N_UNDF, N_WEAK_DEF, N_WEAK_REF, S_ATTR_DEBUG,
    X86_64_RELOC_UNSIGNED,
};
use crate::macho::read::{AtomKind, AtomRelocation, Relocation, Section};
use crate::symbols::SymbolUse;

use super::addr::Addresses;
use super::buf::{align_up, get32, get64, pad_to, push_name16, push32, push64, put32, put64};
use super::buf::{to_u64, to_usize};
use super::layout::{self, Layout, Member, OutSection, OutSegment, SectionKind};
use super::object::LinkObject;
use super::reloc::{self, Place, Referent};
use super::scan::Synthetic;
use super::state::{Link, NONE, SymbolDef};
use super::thunks::Thunks;

/// Size of an `nlist_64`.
const NLIST_SIZE: usize = 16;

/// Size of a `relocation_info`.
const RELOCATION_SIZE: u64 = 8;

/// Links the resolved `link` into a relocatable object.
///
/// # Errors
///
/// [`Error::Unimplemented`] for what `-r` does not support yet (export
/// lists, `N_INDR` symbols, relocations into coalesced code), and
/// relocation errors.
pub fn write(
    link: &Link<'_>,
    options: &LinkOptions,
    diagnostics: &dyn DiagnosticSink,
) -> Result<Vec<u8>> {
    let darwin = &options.darwin;
    let dropped_debug = (0..link.files.len())
        .filter_map(|file| link.object(file))
        .any(|object| {
            object
                .file
                .sections()
                .iter()
                .any(|s| !carried(s) && s.is_debug())
        });
    if dropped_debug {
        diagnostics.emit(Diagnostic::warning(
            "-r does not carry DWARF debug sections over to the output yet",
        ));
    }
    if !darwin.exported_symbols.is_empty()
        || !darwin.exported_symbols_lists.is_empty()
        || !darwin.unexported_symbols.is_empty()
        || !darwin.unexported_symbols_lists.is_empty()
        || darwin.no_exported_symbols
    {
        return Err(Error::Unimplemented(
            "export lists with -r (relocatable Mach-O output)".into(),
        ));
    }
    let arm64 = link.config.is_arm64();
    let (mut layout, canonical) = plan(link)?;
    let eh_frame = super::eh_frame::plan(link, &mut super::unwind::Entries::default())?;
    if eh_frame.size() > 0 {
        layout.sections.push(OutSection {
            segname: b"__TEXT".to_vec(),
            sectname: b"__eh_frame".to_vec(),
            flags: eh_frame_flags(link),
            align: 3,
            addr: 0,
            size: eh_frame.size(),
            offset: 0,
            kind: SectionKind::EhFrame,
            input_order: layout.sections.len(),
            inputs: Vec::new(),
            reserved1: 0,
            reserved2: 0,
            segment: 0,
        });
    }
    let content_end = arrange(&mut layout)?;
    if layout.sections.len() > 255 {
        return Err(Error::Limit(
            "more than 255 output sections in a relocatable object".into(),
        ));
    }

    let symbols = Symbols::build(link, &layout, darwin.keep_private_externs)?;
    let linker_options = linker_options(link)?;

    // Load commands.
    let nsects = to_u64(layout.sections.len());
    let options_size: u64 = linker_options
        .iter()
        .map(|strings| linker_option_size(strings))
        .fold(0u64, u64::saturating_add);
    let ncmds = 4usize.saturating_add(linker_options.len());
    let sizeofcmds = 72u64
        .saturating_add(nsects.saturating_mul(80))
        .saturating_add(24)
        .saturating_add(options_size)
        .saturating_add(24)
        .saturating_add(80);
    let header_size = 32u64.saturating_add(sizeofcmds);
    for section in &mut layout.sections {
        if !section.is_zerofill() {
            section.offset = header_size.saturating_add(section.addr);
        }
    }

    // Section contents and their relocations.
    let mut image = vec![0u8; to_usize(header_size.saturating_add(content_end))];
    let addresses = Addresses {
        link,
        layout: &layout,
        synthetic: &Synthetic::default(),
        thunks: &Thunks::default(),
    };
    let context = Context {
        link,
        layout: &layout,
        symbols: &symbols,
        canonical: &canonical,
        arm64,
    };
    let mut relocations: Vec<Vec<u8>> = Vec::with_capacity(layout.sections.len());
    for section in &layout.sections {
        let mut groups: Vec<Group> = Vec::new();
        if !section.is_zerofill() && section.size > 0 {
            let start = to_usize(section.offset);
            let out = image
                .get_mut(start..start.saturating_add(to_usize(section.size)))
                .ok_or_else(|| Error::Internal("section past the end of the output".into()))?;
            match section.kind {
                SectionKind::EhFrame => {
                    for personality in super::eh_frame::write_relocatable(
                        &addresses,
                        &eh_frame,
                        section.addr,
                        out,
                    )? {
                        let symbolnum = symbols.global_index(personality.symbol)?;
                        let address = field_address(personality.offset)?;
                        let relocation = personality.relocation;
                        groups.push((
                            address,
                            vec![pack(
                                address,
                                symbolnum,
                                relocation.pcrel,
                                relocation.length,
                                true,
                                relocation.r_type,
                            )],
                        ));
                    }
                }
                _ => context.write_section(section, out, &mut groups)?,
            }
        }
        // Descending addresses, as the assemblers write them; each group
        // keeps its ADDEND or SUBTRACTOR entry first.
        groups.sort_by_key(|group| std::cmp::Reverse(group.0));
        let mut bytes = Vec::new();
        for (_, entries) in groups {
            for entry in entries {
                bytes.extend_from_slice(&entry);
            }
        }
        relocations.push(bytes);
    }

    // Relocations, the symbol table and the string table follow the
    // section contents.
    let mut at = align_up(to_u64(image.len()), 8);
    let mut reloc_offsets = Vec::with_capacity(relocations.len());
    for bytes in &relocations {
        reloc_offsets.push(if bytes.is_empty() { 0 } else { at });
        at = at.saturating_add(to_u64(bytes.len()));
    }
    let symoff = align_up(at, 8);
    let (nlist, strings) = symbols.encode();
    let stroff = symoff.saturating_add(to_u64(nlist.len()));
    let end = stroff.saturating_add(to_u64(strings.len()));
    image.resize(to_usize(end), 0);
    for (bytes, &offset) in relocations.iter().zip(&reloc_offsets) {
        copy(&mut image, offset, bytes)?;
    }
    copy(&mut image, symoff, &nlist)?;
    copy(&mut image, stroff, &strings)?;

    // The header.
    let mut header = Vec::with_capacity(to_usize(header_size));
    let (cpu_type, cpu_subtype) = if arm64 {
        (CPU_TYPE_ARM64, CPU_SUBTYPE_ARM64_ALL)
    } else {
        (CPU_TYPE_X86_64, CPU_SUBTYPE_X86_64_ALL)
    };
    let subsections = (0..link.files.len())
        .filter_map(|file| link.object(file))
        .all(|object| object.file.subsections_via_symbols());
    push32(&mut header, MH_MAGIC_64);
    push32(&mut header, cpu_type);
    push32(&mut header, cpu_subtype);
    push32(&mut header, MH_OBJECT);
    push32(&mut header, u32::try_from(ncmds).unwrap_or(u32::MAX));
    push32(&mut header, u32_of(sizeofcmds)?);
    push32(
        &mut header,
        if subsections {
            MH_SUBSECTIONS_VIA_SYMBOLS
        } else {
            0
        },
    );
    push32(&mut header, 0);

    let vmsize = layout
        .sections
        .iter()
        .map(OutSection::end)
        .max()
        .unwrap_or(0);
    push32(&mut header, LC_SEGMENT_64);
    push32(
        &mut header,
        u32_of(72u64.saturating_add(nsects.saturating_mul(80)))?,
    );
    push_name16(&mut header, b"");
    push64(&mut header, 0);
    push64(&mut header, vmsize);
    push64(&mut header, header_size);
    push64(&mut header, content_end);
    push32(&mut header, 7);
    push32(&mut header, 7);
    push32(&mut header, u32_of(nsects)?);
    push32(&mut header, 0);
    for (index, section) in layout.sections.iter().enumerate() {
        let count = relocations
            .get(index)
            .map_or(0, |r| to_u64(r.len()) / RELOCATION_SIZE);
        push_name16(&mut header, &section.sectname);
        push_name16(&mut header, &section.segname);
        push64(&mut header, section.addr);
        push64(&mut header, section.size);
        push32(&mut header, u32_of(section.offset)?);
        push32(&mut header, section.align);
        push32(
            &mut header,
            u32_of(reloc_offsets.get(index).copied().unwrap_or(0))?,
        );
        push32(&mut header, u32_of(count)?);
        push32(&mut header, section.flags);
        push32(&mut header, 0);
        push32(&mut header, 0);
        push32(&mut header, 0);
    }
    let platform = &link.config.platform;
    push32(&mut header, LC_BUILD_VERSION);
    push32(&mut header, 24);
    push32(&mut header, platform.platform);
    push32(&mut header, platform.min.0);
    push32(&mut header, platform.sdk.0);
    push32(&mut header, 0);
    for strings in &linker_options {
        let size = linker_option_size(strings);
        let start = header.len();
        push32(&mut header, LC_LINKER_OPTION);
        push32(&mut header, u32_of(size)?);
        push32(
            &mut header,
            u32::try_from(strings.len()).unwrap_or(u32::MAX),
        );
        for string in strings {
            header.extend_from_slice(string);
            header.push(0);
        }
        header.resize(start.saturating_add(to_usize(size)), 0);
    }
    push32(&mut header, LC_SYMTAB);
    push32(&mut header, 24);
    push32(&mut header, u32_of(symoff)?);
    push32(&mut header, symbols.count()?);
    push32(&mut header, u32_of(stroff)?);
    push32(&mut header, u32_of(to_u64(strings.len()))?);
    push32(&mut header, LC_DYSYMTAB);
    push32(&mut header, 80);
    let (locals, extdefs, undefs) = symbols.groups()?;
    for value in [
        0,
        locals,
        locals,
        extdefs,
        locals.saturating_add(extdefs),
        undefs,
    ] {
        push32(&mut header, value);
    }
    for _ in 0..12 {
        push32(&mut header, 0);
    }
    if to_u64(header.len()) != header_size {
        return Err(Error::Internal(format!(
            "relocatable header takes {} bytes, {header_size} planned",
            header.len()
        )));
    }
    copy(&mut image, 0, &header)?;
    Ok(image)
}

fn u32_of(value: u64) -> Result<u32> {
    u32::try_from(value).map_err(|_| Error::Limit("relocatable object larger than 4 GiB".into()))
}

fn field_address(offset: u64) -> Result<u32> {
    u32::try_from(offset)
        .ok()
        .filter(|&a| a < 0x8000_0000)
        .ok_or_else(|| Error::Limit("section larger than 2 GiB in a relocatable object".into()))
}

fn copy(image: &mut [u8], offset: u64, bytes: &[u8]) -> Result<()> {
    let start = to_usize(offset);
    image
        .get_mut(start..start.saturating_add(bytes.len()))
        .ok_or_else(|| Error::Internal("relocatable object too short".into()))?
        .copy_from_slice(bytes);
    Ok(())
}

/// The flags of the inputs' `__eh_frame` (the first one's), or the ones
/// assemblers give it.
fn eh_frame_flags(link: &Link<'_>) -> u32 {
    (0..link.files.len())
        .filter_map(|file| link.object(file))
        .find_map(|object| {
            object
                .file
                .find_section(b"__TEXT", b"__eh_frame")
                .map(|(_, section)| section.flags)
        })
        .unwrap_or(0x6800_000b)
}

/// Whether an input section is copied into the relocatable output:
/// everything but debug sections, `__eh_frame` (rebuilt) and bitcode.
fn carried(section: &Section<'_>) -> bool {
    if section.is(b"__LD", b"__compact_unwind") {
        return true;
    }
    section.flags & S_ATTR_DEBUG == 0
        && !section.is(b"__TEXT", b"__eh_frame")
        && section.segname != b"__LLVM"
}

/// For the `__compact_unwind` section of `object`, whether each of its
/// atoms (records) describes a function that is kept.
fn unwind_records_kept(
    link: &Link<'_>,
    file: usize,
    object: &LinkObject<'_>,
) -> Result<HashMap<usize, bool>> {
    let mut kept = HashMap::new();
    let Some((index, _)) = object.file.find_section(b"__LD", b"__compact_unwind") else {
        return Ok(kept);
    };
    let Some(relocations) = object.relocations.get(index) else {
        return Ok(kept);
    };
    let data = object.file.section_data(index)?;
    for relocation in relocations {
        let Some(atom) = object.atoms.atoms().get(relocation.atom) else {
            continue;
        };
        // The function field starts the record.
        if u64::from(relocation.relocation.relocation.address) != atom.offset {
            continue;
        }
        let decoded = reloc::decode(link, file, object, index, data, &relocation.relocation)?;
        let live = match reloc::place(link, file, object, decoded.referent, decoded.addend)? {
            Place::Atom { atom, .. } => link.live.get(atom).copied().unwrap_or(false),
            _ => true,
        };
        kept.insert(relocation.atom, live);
    }
    Ok(kept)
}

/// Assigns the carried atoms to output sections (in input order) and
/// places them. Returns the layout (sections unsorted, without addresses)
/// and, per atom, whether it holds its own copy (not a merged literal).
fn plan(link: &Link<'_>) -> Result<(Layout, Vec<bool>)> {
    let mut sections: Vec<OutSection> = Vec::new();
    let mut by_name: HashMap<(Vec<u8>, Vec<u8>), usize> = HashMap::new();
    let mut members: Vec<Vec<Member>> = Vec::new();
    for file in 0..link.files.len() {
        let Some(object) = link.object(file) else {
            continue;
        };
        let unwind = unwind_records_kept(link, file, object)?;
        for (index, section) in object.file.sections().iter().enumerate() {
            if !carried(section) {
                continue;
            }
            let Some(range) = object.atoms.section_range(index) else {
                continue;
            };
            let atoms: Vec<usize> = range
                .filter(|&atom| {
                    link.is_live(file, atom) && unwind.get(&atom).copied().unwrap_or(true)
                })
                .collect();
            if atoms.is_empty() {
                continue;
            }
            let key = (section.segname.to_vec(), section.sectname.to_vec());
            let out = match by_name.get(&key) {
                Some(&out) => out,
                None => {
                    let out = sections.len();
                    sections.push(OutSection {
                        segname: key.0.clone(),
                        sectname: key.1.clone(),
                        flags: section.flags,
                        align: 0,
                        addr: 0,
                        size: 0,
                        offset: 0,
                        kind: SectionKind::Input,
                        input_order: out,
                        inputs: Vec::new(),
                        reserved1: 0,
                        reserved2: 0,
                        segment: 0,
                    });
                    members.push(Vec::new());
                    by_name.insert(key, out);
                    out
                }
            };
            let Some(out_section) = sections.get_mut(out) else {
                continue;
            };
            // `__objc_imageinfo` is one record, whatever the input count.
            if section.sectname == b"__objc_imageinfo" && !out_section.inputs.is_empty() {
                continue;
            }
            out_section.inputs.push((
                u32::try_from(file).unwrap_or(NONE),
                u32::try_from(index).unwrap_or(NONE),
            ));
            if let Some(list) = members.get_mut(out) {
                list.extend(atoms.into_iter().map(|atom| ((0, false), file, atom)));
            }
        }
    }

    let mut atom_section = vec![NONE; link.atom_count];
    let mut atom_offset = vec![0u64; link.atom_count];
    let mut canonical = vec![false; link.atom_count];
    for (out, list) in members.iter().enumerate() {
        let Some(section) = sections.get_mut(out) else {
            continue;
        };
        let literals = if layout::is_literal_section(section.flags) {
            layout::dedup_literals(link, list)?
        } else {
            layout::Literals::default()
        };
        for (position, &(_, file, atom)) in list.iter().enumerate() {
            let Some(info) = link.object(file).and_then(|o| o.atoms.atoms().get(atom)) else {
                continue;
            };
            let id = link.atom_id(file, atom);
            let first = literals.canonical(position);
            let offset = if first == position {
                let align = literals.align(position).unwrap_or(info.align);
                section.align = section.align.max(align);
                let offset = align_up(section.size, 1u64.checked_shl(align).unwrap_or(1));
                section.size = offset
                    .checked_add(info.size)
                    .ok_or_else(|| Error::Limit("output section larger than 2^64".into()))?;
                if let Some(slot) = canonical.get_mut(id) {
                    *slot = true;
                }
                offset
            } else {
                // Merged into an earlier copy, already placed.
                list.get(first)
                    .and_then(|&(_, file, atom)| atom_offset.get(link.atom_id(file, atom)))
                    .copied()
                    .unwrap_or(0)
            };
            if let Some(slot) = atom_section.get_mut(id) {
                *slot = u32::try_from(out).unwrap_or(NONE);
            }
            if let Some(slot) = atom_offset.get_mut(id) {
                *slot = offset;
            }
        }
    }
    Ok((
        Layout {
            sections,
            segments: Vec::new(),
            atom_section,
            atom_offset,
            common_offset: Vec::new(),
            header_size: 0,
            members: Vec::new(),
            islands: Vec::new(),
        },
        canonical,
    ))
}

/// The order of a section in the object: segment, then place in it, then
/// input order; zero-fill sections last.
fn rank(section: &OutSection) -> (bool, u8, u8, usize) {
    let segment = match section.segname.as_slice() {
        b"__TEXT" => 0,
        b"__DATA_CONST" => 1,
        b"__DATA" => 2,
        b"__LD" => 4,
        _ => 3,
    };
    let within = match (section.segname.as_slice(), section.sectname.as_slice()) {
        (b"__TEXT", b"__text") => 0,
        (b"__TEXT", b"__eh_frame") => 2,
        _ => 1,
    };
    (section.is_zerofill(), segment, within, section.input_order)
}

/// Sorts the sections and assigns their addresses from 0. Returns the end
/// of the last section with contents.
fn arrange(layout: &mut Layout) -> Result<u64> {
    let mut order: Vec<usize> = (0..layout.sections.len()).collect();
    order.sort_by_key(|&index| layout.sections.get(index).map(rank));
    let mut remap = vec![NONE; layout.sections.len()];
    for (new, &old) in order.iter().enumerate() {
        if let Some(slot) = remap.get_mut(old) {
            *slot = u32::try_from(new).unwrap_or(NONE);
        }
    }
    let mut sections: Vec<OutSection> = order
        .iter()
        .filter_map(|&old| layout.sections.get(old).cloned())
        .collect();
    for slot in &mut layout.atom_section {
        if *slot != NONE {
            *slot = remap
                .get(to_usize(u64::from(*slot)))
                .copied()
                .unwrap_or(NONE);
        }
    }
    let mut address = 0u64;
    let mut content_end = 0u64;
    for section in &mut sections {
        address = align_up(address, 1u64.checked_shl(section.align).unwrap_or(1));
        section.addr = address;
        address = address
            .checked_add(section.size)
            .ok_or_else(|| Error::Limit("relocatable object larger than 2^64".into()))?;
        if !section.is_zerofill() {
            content_end = address;
        }
    }
    layout.segments = vec![OutSegment {
        sections: (0..sections.len()).collect(),
        ..OutSegment::default()
    }];
    layout.sections = sections;
    Ok(content_end)
}

/// The `LC_LINKER_OPTION` commands of the live objects, in input order,
/// without repeats.
fn linker_options<'a>(link: &Link<'a>) -> Result<Vec<Vec<&'a [u8]>>> {
    let mut out: Vec<Vec<&'a [u8]>> = Vec::new();
    for file in 0..link.files.len() {
        let Some(object) = link.object(file) else {
            continue;
        };
        for strings in object.file.linker_options()? {
            if !out.contains(&strings) {
                out.push(strings);
            }
        }
    }
    Ok(out)
}

fn linker_option_size(strings: &[&[u8]]) -> u64 {
    let text: u64 = strings
        .iter()
        .map(|s| to_u64(s.len()).saturating_add(1))
        .fold(0u64, u64::saturating_add);
    align_up(12u64.saturating_add(text), 8)
}

/// One `nlist_64` before encoding.
#[derive(Clone, Debug)]
struct Entry {
    name: Vec<u8>,
    n_type: u8,
    n_sect: u8,
    n_desc: u16,
    n_value: u64,
}

/// The output symbol table and where each input symbol went.
struct Symbols {
    locals: Vec<Entry>,
    extdefs: Vec<Entry>,
    undefs: Vec<Entry>,
    /// Output index of each (file, symbol table index) local symbol.
    local: HashMap<(usize, u32), u32>,
    /// Output index of each global symbol, by [`SymbolId`], or [`NONE`].
    global: Vec<u32>,
}

impl Symbols {
    #[allow(clippy::too_many_lines)]
    fn build(link: &Link<'_>, layout: &Layout, keep_private_externs: bool) -> Result<Self> {
        // Where a section symbol of `object` lands: its ordinal and address.
        let place_symbol = |file: usize, object: &LinkObject<'_>, symbol: u32, value: u64| {
            let atom = object.atoms.symbol_atom(symbol)?;
            let info = object.atoms.atoms().get(atom)?;
            let id = link.atom_id(file, atom);
            let section = *layout.atom_section.get(id)?;
            if section == NONE {
                return None;
            }
            let header = object
                .file
                .sections()
                .get(to_usize(u64::from(info.section)))?;
            let old = header.addr.checked_add(info.offset)?;
            let new = layout.atom_address(id)?;
            let ordinal = u8::try_from(section.checked_add(1)?).ok()?;
            Some((ordinal, new.wrapping_add(value.wrapping_sub(old))))
        };

        let mut locals: Vec<(Entry, Option<(usize, u32)>)> = Vec::new();
        let mut labels = 0usize;
        for file in 0..link.files.len() {
            let Some(object) = link.object(file) else {
                continue;
            };
            for symbol in object.file.symbols().iter() {
                let symbol = symbol?;
                if symbol.is_external() {
                    continue;
                }
                let (n_sect, n_value) = match symbol.n_type & N_TYPE {
                    N_SECT => match place_symbol(file, object, symbol.index, symbol.n_value) {
                        Some(placed) => placed,
                        None => continue,
                    },
                    N_ABS => (0, symbol.n_value),
                    _ => continue,
                };
                locals.push((
                    Entry {
                        name: symbol.name.to_vec(),
                        n_type: symbol.n_type & N_TYPE,
                        n_sect,
                        n_desc: symbol.n_desc,
                        n_value,
                    },
                    Some((file, symbol.index)),
                ));
            }
            // An atom that no symbol starts (the bytes before a section's
            // first symbol) gets a temporary label, so that it stays an
            // atom of its own once concatenated after another object's.
            for (atom, info) in object.atoms.atoms().iter().enumerate() {
                if info.kind != AtomKind::Regular
                    || info.size == 0
                    || !object.atoms.atom_symbols(atom).is_empty()
                {
                    continue;
                }
                let id = link.atom_id(file, atom);
                let (Some(&section), Some(address)) =
                    (layout.atom_section.get(id), layout.atom_address(id))
                else {
                    continue;
                };
                let Some(ordinal) = section
                    .checked_add(1)
                    .and_then(|o| u8::try_from(o).ok())
                    .filter(|_| section != NONE)
                else {
                    continue;
                };
                let name = format!("ltmp_r{labels}").into_bytes();
                labels = labels.saturating_add(1);
                locals.push((
                    Entry {
                        name,
                        n_type: N_SECT,
                        n_sect: ordinal,
                        n_desc: 0,
                        n_value: address,
                    },
                    None,
                ));
            }
        }

        // Which globals the live objects refer to.
        let count = link.symbols.len();
        let mut referenced = vec![false; count];
        for index in 0..link.files.len() {
            let Some(object) = link.object(index) else {
                continue;
            };
            let ids = link.resolution.symbol_ids(FileId::new(index));
            for (use_, id) in object.uses.iter().zip(ids) {
                if matches!(use_, SymbolUse::Reference { .. })
                    && let Some(slot) = referenced.get_mut(id.index())
                {
                    *slot = true;
                }
            }
        }

        let mut extdefs: Vec<(Entry, SymbolId)> = Vec::new();
        let mut undefs: Vec<(Entry, SymbolId)> = Vec::new();
        let mut hidden: Vec<(Entry, SymbolId)> = Vec::new();
        for (index, def) in link.defs.iter().enumerate() {
            let id = SymbolId::new(index);
            let name = link.symbols.name(id).bytes().to_vec();
            match def {
                SymbolDef::Object { file, symbol } => {
                    let file = to_usize(u64::from(*file));
                    let Some(object) = link.object(file) else {
                        continue;
                    };
                    let entry = object.file.symbols().get(*symbol)?;
                    let (n_type, n_sect, n_value) = match entry.n_type & N_TYPE {
                        N_SECT => match place_symbol(file, object, *symbol, entry.n_value) {
                            Some((n_sect, n_value)) => (N_SECT, n_sect, n_value),
                            None => continue,
                        },
                        N_ABS => (N_ABS, 0, entry.n_value),
                        N_INDR => {
                            return Err(Error::Unimplemented(format!(
                                "indirect symbol {} with -r",
                                String::from_utf8_lossy(&name)
                            )));
                        }
                        _ => continue,
                    };
                    let private = entry.is_private_external()
                        || link.files.get(file).is_some_and(|f| f.hidden);
                    if private && !keep_private_externs {
                        hidden.push((
                            Entry {
                                name,
                                n_type,
                                n_sect,
                                n_desc: entry.n_desc & !(N_WEAK_DEF | N_WEAK_REF),
                                n_value,
                            },
                            id,
                        ));
                        continue;
                    }
                    extdefs.push((
                        Entry {
                            name,
                            n_type: n_type | N_EXT | if private { N_PEXT } else { 0 },
                            n_sect,
                            n_desc: entry.n_desc,
                            n_value,
                        },
                        id,
                    ));
                }
                SymbolDef::Common {
                    file, size, align, ..
                } => {
                    let private = link
                        .files
                        .get(to_usize(u64::from(*file)))
                        .is_some_and(|f| f.hidden);
                    undefs.push((
                        Entry {
                            name,
                            n_type: N_UNDF | N_EXT | if private { N_PEXT } else { 0 },
                            n_sect: 0,
                            n_desc: u16::try_from((*align).min(15)).unwrap_or(0) << 8,
                            n_value: *size,
                        },
                        id,
                    ));
                }
                _ => {
                    if !referenced.get(index).copied().unwrap_or(false) {
                        continue;
                    }
                    let strong = link.strong_ref.get(index).copied().unwrap_or(false);
                    undefs.push((
                        Entry {
                            name,
                            n_type: N_UNDF | N_EXT,
                            n_sect: 0,
                            n_desc: if strong { 0 } else { N_WEAK_REF },
                            n_value: 0,
                        },
                        id,
                    ));
                }
            }
        }
        extdefs.sort_by(|a, b| a.0.name.cmp(&b.0.name));
        undefs.sort_by(|a, b| a.0.name.cmp(&b.0.name));

        let mut symbols = Self {
            locals: Vec::with_capacity(locals.len().saturating_add(hidden.len())),
            extdefs: Vec::with_capacity(extdefs.len()),
            undefs: Vec::with_capacity(undefs.len()),
            local: HashMap::new(),
            global: vec![NONE; count],
        };
        let mut next = 0u32;
        let mut index = || -> Result<u32> {
            let current = next;
            next = next
                .checked_add(1)
                .filter(|&n| n <= 0x00ff_ffff)
                .ok_or_else(|| Error::Limit("more than 2^24 symbols in -r output".into()))?;
            Ok(current)
        };
        for (entry, origin) in locals {
            let at = index()?;
            if let Some(origin) = origin {
                symbols.local.insert(origin, at);
            }
            symbols.locals.push(entry);
        }
        for (entry, id) in hidden {
            let at = index()?;
            if let Some(slot) = symbols.global.get_mut(id.index()) {
                *slot = at;
            }
            symbols.locals.push(entry);
        }
        for (entry, id) in extdefs {
            let at = index()?;
            if let Some(slot) = symbols.global.get_mut(id.index()) {
                *slot = at;
            }
            symbols.extdefs.push(entry);
        }
        for (entry, id) in undefs {
            let at = index()?;
            if let Some(slot) = symbols.global.get_mut(id.index()) {
                *slot = at;
            }
            symbols.undefs.push(entry);
        }
        Ok(symbols)
    }

    fn count(&self) -> Result<u32> {
        let (locals, extdefs, undefs) = self.groups()?;
        Ok(locals.saturating_add(extdefs).saturating_add(undefs))
    }

    fn groups(&self) -> Result<(u32, u32, u32)> {
        let count =
            |n: usize| u32::try_from(n).map_err(|_| Error::Limit("too many symbols".into()));
        Ok((
            count(self.locals.len())?,
            count(self.extdefs.len())?,
            count(self.undefs.len())?,
        ))
    }

    fn global_index(&self, id: SymbolId) -> Result<u32> {
        self.global
            .get(id.index())
            .copied()
            .filter(|&i| i != NONE)
            .ok_or_else(|| Error::Internal("relocation against a symbol not in the output".into()))
    }

    /// The `nlist_64` records and the string table (padded to 8 bytes).
    fn encode(&self) -> (Vec<u8>, Vec<u8>) {
        let entries = self.locals.iter().chain(&self.extdefs).chain(&self.undefs);
        let mut nlist = Vec::with_capacity(
            self.locals
                .len()
                .saturating_add(self.extdefs.len())
                .saturating_add(self.undefs.len())
                .saturating_mul(NLIST_SIZE),
        );
        // " \0": offset 1 is the empty string.
        let mut strings: Vec<u8> = vec![b' ', 0];
        let mut offsets: HashMap<&[u8], u32> = HashMap::new();
        for entry in entries {
            let strx = if entry.name.is_empty() {
                1
            } else if let Some(&offset) = offsets.get(entry.name.as_slice()) {
                offset
            } else {
                let offset = u32::try_from(strings.len()).unwrap_or(u32::MAX);
                strings.extend_from_slice(&entry.name);
                strings.push(0);
                offsets.insert(&entry.name, offset);
                offset
            };
            push32(&mut nlist, strx);
            nlist.push(entry.n_type);
            nlist.push(entry.n_sect);
            nlist.extend_from_slice(&entry.n_desc.to_le_bytes());
            push64(&mut nlist, entry.n_value);
        }
        pad_to(&mut strings, 8);
        (nlist, strings)
    }
}

/// A relocation group: its address, and its entries in order (the
/// `ADDEND` or `SUBTRACTOR` entry first).
type Group = (u32, Vec<[u8; 8]>);

/// Encodes a little-endian `relocation_info`.
fn pack(
    address: u32,
    symbolnum: u32,
    pcrel: bool,
    length: u8,
    is_extern: bool,
    r_type: u8,
) -> [u8; 8] {
    let word = (symbolnum & 0x00ff_ffff)
        | (u32::from(pcrel) << 24)
        | (u32::from(length & 3) << 25)
        | (u32::from(is_extern) << 27)
        | (u32::from(r_type & 0xf) << 28);
    let mut out = [0u8; 8];
    out[..4].copy_from_slice(&address.to_le_bytes());
    out[4..].copy_from_slice(&word.to_le_bytes());
    out
}

/// What writing the input sections needs.
struct Context<'x, 'a> {
    link: &'x Link<'a>,
    layout: &'x Layout,
    symbols: &'x Symbols,
    canonical: &'x [bool],
    arm64: bool,
}

impl Context<'_, '_> {
    /// How far atom `atom` (global numbering) moved from its address in its
    /// object.
    fn moved(&self, atom: usize) -> Option<i64> {
        let (file, local) = layout::atom_location(self.link, atom);
        let object = self.link.object(file)?;
        let info = object.atoms.atoms().get(local)?;
        let header = object
            .file
            .sections()
            .get(to_usize(u64::from(info.section)))?;
        let old = header.addr.checked_add(info.offset)?;
        let new = self.layout.atom_address(atom)?;
        Some(new.wrapping_sub(old) as i64)
    }

    /// The atom of symbol-table entry `symbol` of `file` (global numbering).
    fn local_atom(&self, file: usize, object: &LinkObject<'_>, symbol: u32) -> Option<usize> {
        object
            .atoms
            .symbol_atom(symbol)
            .map(|atom| self.link.atom_id(file, atom))
    }

    /// The output symbol an external relocation names, and the atom that
    /// symbol is in (when it is defined in a section).
    fn symbol(
        &self,
        file: usize,
        object: &LinkObject<'_>,
        referent: Referent,
    ) -> Result<(u32, Option<usize>)> {
        match referent {
            Referent::Global(id) => Ok((self.symbols.global_index(id)?, self.link.symbol_atom(id))),
            Referent::Local(symbol) => {
                let index = self
                    .symbols
                    .local
                    .get(&(file, symbol))
                    .copied()
                    .ok_or_else(|| {
                        Error::Unimplemented(format!(
                            "{}: relocation against a local symbol of coalesced code with -r",
                            self.display(file)
                        ))
                    })?;
                Ok((index, self.local_atom(file, object, symbol)))
            }
            Referent::Address { .. } => Err(Error::Internal(
                "section relocation where a symbol was expected".into(),
            )),
        }
    }

    fn display(&self, file: usize) -> String {
        self.link
            .files
            .get(file)
            .map_or_else(String::new, |f| f.display())
    }

    /// Copies the atoms of `section` into `out` and rewrites their
    /// relocations into `groups`.
    fn write_section(
        &self,
        section: &OutSection,
        out: &mut [u8],
        groups: &mut Vec<Group>,
    ) -> Result<()> {
        let link = self.link;
        for &(file, input) in &section.inputs {
            let file = to_usize(u64::from(file));
            let input = to_usize(u64::from(input));
            let Some(object) = link.object(file) else {
                continue;
            };
            let data = object.file.section_data(input)?;
            let Some(range) = object.atoms.section_range(input) else {
                continue;
            };
            for atom in range {
                let id = link.atom_id(file, atom);
                if !self.canonical.get(id).copied().unwrap_or(false) {
                    continue;
                }
                let Some(info) = object.atoms.atoms().get(atom) else {
                    continue;
                };
                let at = to_usize(self.layout.atom_offset.get(id).copied().unwrap_or(0));
                let source = data
                    .get(to_usize(info.offset)..to_usize(info.offset.saturating_add(info.size)))
                    .unwrap_or(&[]);
                if let Some(slot) = out.get_mut(at..at.saturating_add(source.len())) {
                    slot.copy_from_slice(source);
                }
            }
            let Some(relocations) = object.relocations.get(input) else {
                continue;
            };
            for relocation in relocations {
                let id = link.atom_id(file, relocation.atom);
                if !self.canonical.get(id).copied().unwrap_or(false) {
                    continue;
                }
                let group = self
                    .rewrite(file, object, input, data, relocation, out)
                    .map_err(|error| match error {
                        Error::Limit(message) => {
                            Error::Limit(format!("{}: {message}", self.display(file)))
                        }
                        other => other,
                    })?;
                groups.push(group);
            }
        }
        Ok(())
    }

    /// Rewrites one input relocation of section `section` of `object` for
    /// the output: adjusts the stored value in `out` and returns the
    /// output entries.
    #[allow(clippy::too_many_lines)]
    fn rewrite(
        &self,
        file: usize,
        object: &LinkObject<'_>,
        section: usize,
        data: &[u8],
        relocation: &AtomRelocation,
        out: &mut [u8],
    ) -> Result<Group> {
        let link = self.link;
        let arm64 = self.arm64;
        let paired = &relocation.relocation;
        let raw: Relocation = paired.relocation;
        let decoded = reloc::decode(link, file, object, section, data, paired)?;
        let atom = link.atom_id(file, relocation.atom);
        let info = object
            .atoms
            .atoms()
            .get(relocation.atom)
            .ok_or_else(|| Error::Internal("relocation outside the atoms".into()))?;
        let within = decoded.offset.saturating_sub(info.offset);
        let offset = self
            .layout
            .atom_offset
            .get(atom)
            .copied()
            .unwrap_or(0)
            .saturating_add(within);
        let address = field_address(offset)?;
        let at = to_usize(offset);
        let field_moved = self.moved(atom).unwrap_or(0);

        let target = reloc::place(link, file, object, decoded.referent, decoded.addend)?;
        let target_atom = match target {
            Place::Atom { atom, .. } => Some(atom),
            _ => None,
        };
        if let Some(target_atom) = target_atom
            && self.layout.atom_address(target_atom).is_none()
        {
            return Err(Error::Unimplemented(format!(
                "{}: relocation at {:#x} into coalesced code with -r",
                self.display(file),
                decoded.offset
            )));
        }
        let target_moved = target_atom.and_then(|a| self.moved(a)).unwrap_or(0);

        // The minuend: an external relocation keeps its symbol, and its
        // addend follows the target when the target moved away from the
        // symbol; a section relocation names the target's new section.
        let through_slot = {
            let needs = reloc::needs(arm64, raw.r_type);
            needs.got || needs.tlv
        };
        let (symbolnum, is_extern, shift) = match decoded.referent {
            Referent::Address { .. } => {
                let section = target_atom
                    .and_then(|a| self.layout.atom_section.get(a).copied())
                    .filter(|&s| s != NONE)
                    .ok_or_else(|| {
                        Error::Internal("section relocation without a target atom".into())
                    })?;
                let pc = if raw.pcrel { field_moved } else { 0 };
                (
                    section.saturating_add(1),
                    false,
                    target_moved.wrapping_sub(pc),
                )
            }
            referent => {
                let (index, symbol_atom) = self.symbol(file, object, referent)?;
                let shift = match symbol_atom {
                    Some(symbol_atom) if !through_slot && Some(symbol_atom) != target_atom => {
                        let symbol_moved = self.moved(symbol_atom).unwrap_or(0);
                        target_moved.wrapping_sub(symbol_moved)
                    }
                    _ => 0,
                };
                (index, true, shift)
            }
        };

        let mut entries: Vec<[u8; 8]> = Vec::with_capacity(2);
        if let Some(subtractor) = paired.subtractor {
            let Some(subtrahend) = decoded.subtrahend else {
                return Err(Error::Internal("SUBTRACTOR without a subtrahend".into()));
            };
            let (index, _) = self.symbol(file, object, subtrahend)?;
            entries.push(pack(
                address,
                index,
                false,
                subtractor.length,
                true,
                subtractor.r_type,
            ));
        }

        let in_field = !arm64
            || raw.r_type == ARM64_RELOC_UNSIGNED
            || paired.subtractor.is_some()
            || (raw.r_type == ARM64_RELOC_BRANCH26 && !is_extern);
        if in_field {
            if shift != 0 {
                if arm64 && raw.r_type == ARM64_RELOC_BRANCH26 {
                    adjust_branch(out, at, shift)?;
                } else {
                    let unsigned = (arm64 && raw.r_type == ARM64_RELOC_UNSIGNED)
                        || (!arm64 && raw.r_type == X86_64_RELOC_UNSIGNED);
                    adjust_field(out, at, raw.length, shift, unsigned && !raw.pcrel)?;
                }
            }
        } else {
            // arm64 keeps the addend of an instruction relocation in an
            // ADDEND entry.
            let addend = i64::from(paired.addend.unwrap_or(0)).wrapping_add(shift);
            if addend != 0 {
                if !(-0x80_0000..0x80_0000).contains(&addend) {
                    return Err(Error::Limit(format!(
                        "ARM64_RELOC_ADDEND at {address:#x} out of range ({addend:#x})"
                    )));
                }
                entries.push(pack(
                    address,
                    (addend as u32) & 0x00ff_ffff,
                    false,
                    2,
                    false,
                    ARM64_RELOC_ADDEND,
                ));
            }
        }
        entries.push(pack(
            address, symbolnum, raw.pcrel, raw.length, is_extern, raw.r_type,
        ));
        Ok((address, entries))
    }
}

/// Adds `shift` to the 4- or 8-byte value at `at`.
fn adjust_field(out: &mut [u8], at: usize, length: u8, shift: i64, unsigned: bool) -> Result<()> {
    let outside = || Error::Internal("relocation outside its section".into());
    match length {
        3 => {
            let value = get64(out, at).ok_or_else(outside)?;
            put64(out, at, value.wrapping_add(shift as u64)).ok_or_else(outside)
        }
        2 => {
            let value = get32(out, at).ok_or_else(outside)?;
            let new = if unsigned {
                i64::from(value).wrapping_add(shift)
            } else {
                i64::from(value as i32).wrapping_add(shift)
            };
            if i32::try_from(new).is_err() && u32::try_from(new).is_err() {
                return Err(Error::Limit(format!(
                    "relocated value at {at:#x} out of range for 32 bits"
                )));
            }
            put32(out, at, new as u32).ok_or_else(outside)
        }
        _ => Err(Error::Unimplemented(format!(
            "moving a {}-byte relocated field with -r",
            1u32 << (length & 3)
        ))),
    }
}

/// Adds `shift` bytes to the displacement of the arm64 branch at `at`.
fn adjust_branch(out: &mut [u8], at: usize, shift: i64) -> Result<()> {
    let insn = crate::arch::aarch64::read_insn(out, at)
        .ok_or_else(|| Error::Internal("relocation outside its section".into()))?;
    let imm = i64::from(((insn & 0x03ff_ffff) << 6) as i32 >> 6);
    let delta = imm.wrapping_mul(4).wrapping_add(shift);
    let insn = crate::arch::aarch64::Field::Branch26
        .encode(insn, delta)
        .map_err(|_| Error::Limit(format!("branch at {at:#x} out of range")))?;
    crate::arch::aarch64::write_insn(out, at, insn)
        .ok_or_else(|| Error::Internal("relocation outside its section".into()))
}
