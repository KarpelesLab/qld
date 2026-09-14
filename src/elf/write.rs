//! Output writing (pipeline stage 11).
//!
//! The image is cut into disjoint chunks: the ELF and program headers, every
//! input section, merge group and synthetic part, the symbol and string
//! tables, and the section header table. [`OutputFile::write_chunks`] fills
//! them in parallel. Input sections are copied from the input mapping and
//! relocated in place; relocation problems are reported to the diagnostic
//! sink (every one, not just the first) and fail the link afterwards.

#![deny(clippy::arithmetic_side_effects)]

use crate::args::LinkOptions;
use crate::diag::{Diagnostic, DiagnosticSink};
use crate::elf::read::Relocations;
use crate::elf::read::consts::{
    EM_X86_64, ET_EXEC, SHF_ALLOC, reloc_name, x86_64::R_X86_64_IRELATIVE,
};
use crate::error::{Error, Result};
use crate::ids::SectionId;
use crate::output::{ChunkRange, OutputFile};

use super::arch::x86_64::{self, ApplyError, Kind};
use super::defined::LinkerSymbols;
use super::ehframe::EhSection;
use super::layout::{EHDR_SIZE, Layout, Member, PHDR_SIZE, SHDR_SIZE, Trailer};
use super::object::SectionKind;
use super::rules::Synthetic;
use super::scan::location;
use super::symtab::{SymtabPlan, write_strtab, write_symtab};
use super::synth::{Owner, write_build_id_header};
use super::values::Addresses;

/// What one output chunk holds.
#[derive(Clone, Copy, Debug)]
enum Chunk {
    Headers,
    Input(SectionId),
    Merge(u32),
    Synthetic(Synthetic),
    Symtab,
    Strtab,
    Shstrtab,
    SectionHeaders,
}

/// Inputs to the writer.
pub struct WriteInput<'w, 'x, 'a> {
    /// Options.
    pub options: &'w LinkOptions,
    /// Addresses (and through it, layout, merge, eh_frame, synthetic).
    pub addresses: &'w Addresses<'x, 'a>,
    /// The symbol table plan.
    pub symtab: &'w SymtabPlan,
    /// Linker-defined symbols.
    pub linker: &'w LinkerSymbols,
    /// The entry address.
    pub entry: u64,
    /// Diagnostics.
    pub diagnostics: &'w dyn DiagnosticSink,
}

/// Writes the output file and applies the build-id.
///
/// # Errors
///
/// Returns I/O errors, [`Error::Reported`] when relocations failed, and
/// [`Error::Internal`] for layout bugs.
pub fn write(input: &WriteInput<'_, '_, '_>) -> Result<()> {
    let layout = input.addresses.layout;
    let mut chunks: Vec<(ChunkRange, Chunk)> = Vec::new();
    let headers = EHDR_SIZE.saturating_add(
        PHDR_SIZE.saturating_mul(u64::try_from(layout.segments.len()).unwrap_or(0)),
    );
    chunks.push((ChunkRange::new(0, headers), Chunk::Headers));
    for section in &layout.sections {
        if !section.has_file_bytes() || section.size == 0 {
            continue;
        }
        match section.trailer {
            Trailer::Symtab => {
                chunks.push((ChunkRange::new(section.offset, section.size), Chunk::Symtab))
            }
            Trailer::Strtab => {
                chunks.push((ChunkRange::new(section.offset, section.size), Chunk::Strtab))
            }
            Trailer::Shstrtab => {
                chunks.push((
                    ChunkRange::new(section.offset, section.size),
                    Chunk::Shstrtab,
                ));
            }
            Trailer::None => {
                for placed in &section.members {
                    if placed.size == 0 {
                        continue;
                    }
                    let range =
                        ChunkRange::new(section.offset.saturating_add(placed.offset), placed.size);
                    let chunk = match placed.member {
                        Member::Input(id) => Chunk::Input(id),
                        Member::Merge(group) => Chunk::Merge(group),
                        Member::Synthetic(kind) => Chunk::Synthetic(kind),
                    };
                    chunks.push((range, chunk));
                }
            }
        }
    }
    let shnum = u64::try_from(layout.sections.len().saturating_add(1)).unwrap_or(0);
    chunks.push((
        ChunkRange::new(layout.shoff, shnum.saturating_mul(SHDR_SIZE)),
        Chunk::SectionHeaders,
    ));
    chunks.sort_by_key(|(range, _)| range.offset);
    let ranges: Vec<ChunkRange> = chunks.iter().map(|(range, _)| *range).collect();

    let path = input.options.output_path();
    let mut file = OutputFile::create(
        &path,
        layout.file_size,
        &crate::output::OutputOptions::default(),
    )?;
    let errors_before = input.diagnostics.error_count();
    file.write_chunks(&ranges, |index, out| {
        let Some(&(_, chunk)) = chunks.get(index) else {
            return Err(Error::Internal("chunk index out of range".into()));
        };
        write_chunk(input, chunk, out)
    })?;
    let errors = input
        .diagnostics
        .error_count()
        .saturating_sub(errors_before);
    if errors > 0 && !input.options.noinhibit_exec {
        return Err(Error::Reported { errors });
    }
    if let Some((_, offset, _)) = layout.synthetic(Synthetic::BuildId) {
        file.apply_build_id(&input.options.build_id, offset.saturating_add(16))?;
    }
    file.finish()?;
    Ok(())
}

fn write_chunk(input: &WriteInput<'_, '_, '_>, chunk: Chunk, out: &mut [u8]) -> Result<()> {
    let addresses = input.addresses;
    let layout = addresses.layout;
    match chunk {
        Chunk::Headers => write_headers(input, out),
        Chunk::SectionHeaders => write_section_headers(layout, out),
        Chunk::Shstrtab => {
            if let Some(dest) = out.get_mut(..layout.shstrtab.len()) {
                dest.copy_from_slice(&layout.shstrtab);
            }
            Ok(())
        }
        Chunk::Symtab => {
            write_symtab(input.symtab, addresses, input.linker, out);
            Ok(())
        }
        Chunk::Strtab => {
            write_strtab(input.symtab, &addresses.refs, out);
            Ok(())
        }
        Chunk::Merge(group) => addresses
            .merged
            .merged
            .write_group(group as usize, out)
            .map_err(Error::from),
        Chunk::Synthetic(kind) => write_synthetic(input, kind, out),
        Chunk::Input(id) => write_input(input, id, out),
    }
}

fn write_headers(input: &WriteInput<'_, '_, '_>, out: &mut [u8]) -> Result<()> {
    let layout = input.addresses.layout;
    let too_small = || Error::Internal("header chunk too small".into());
    let header = out.get_mut(..64).ok_or_else(too_small)?;
    header.fill(0);
    header[..4].copy_from_slice(b"\x7fELF");
    header[4] = 2; // ELFCLASS64
    header[5] = 1; // ELFDATA2LSB
    header[6] = 1; // EV_CURRENT
    header[7] = if input.addresses.synth.iplt.is_empty() {
        0
    } else {
        3
    }; // ELFOSABI_GNU
    header[16..18].copy_from_slice(&ET_EXEC.to_le_bytes());
    header[18..20].copy_from_slice(&EM_X86_64.to_le_bytes());
    header[20..24].copy_from_slice(&1u32.to_le_bytes());
    header[24..32].copy_from_slice(&input.entry.to_le_bytes());
    header[32..40].copy_from_slice(&EHDR_SIZE.to_le_bytes());
    header[40..48].copy_from_slice(&layout.shoff.to_le_bytes());
    header[52..54].copy_from_slice(&64u16.to_le_bytes());
    header[54..56].copy_from_slice(&56u16.to_le_bytes());
    let phnum = u16::try_from(layout.segments.len())
        .map_err(|_| Error::Limit("too many program headers".into()))?;
    header[56..58].copy_from_slice(&phnum.to_le_bytes());
    header[58..60].copy_from_slice(&64u16.to_le_bytes());
    let shnum = layout.sections.len().saturating_add(1);
    let (shnum_field, shstrndx) = match u16::try_from(shnum) {
        Ok(n) if n < 0xff00 => (n, u16::try_from(layout.sections.len()).unwrap_or(0)),
        _ => (0, 0xffff),
    };
    header[60..62].copy_from_slice(&shnum_field.to_le_bytes());
    header[62..64].copy_from_slice(&shstrndx.to_le_bytes());

    let phdrs = out.get_mut(64..).ok_or_else(too_small)?;
    for (segment, entry) in layout
        .segments
        .iter()
        .zip(phdrs.as_chunks_mut::<56>().0.iter_mut())
    {
        entry[0..4].copy_from_slice(&segment.p_type.to_le_bytes());
        entry[4..8].copy_from_slice(&segment.flags.to_le_bytes());
        entry[8..16].copy_from_slice(&segment.offset.to_le_bytes());
        entry[16..24].copy_from_slice(&segment.vaddr.to_le_bytes());
        entry[24..32].copy_from_slice(&segment.vaddr.to_le_bytes());
        entry[32..40].copy_from_slice(&segment.filesz.to_le_bytes());
        entry[40..48].copy_from_slice(&segment.memsz.to_le_bytes());
        entry[48..56].copy_from_slice(&segment.align.to_le_bytes());
    }
    Ok(())
}

fn write_section_headers(layout: &Layout<'_>, out: &mut [u8]) -> Result<()> {
    out.fill(0);
    let shnum = layout.sections.len().saturating_add(1);
    if let Some(first) = out.get_mut(..64)
        && u16::try_from(shnum).map_or(true, |n| n >= 0xff00)
    {
        {
            // Extended numbering: section 0 holds the count and the index.
            first[32..40].copy_from_slice(&u64::try_from(shnum).unwrap_or(0).to_le_bytes());
            first[40..44].copy_from_slice(
                &u32::try_from(layout.sections.len())
                    .unwrap_or(0)
                    .to_le_bytes(),
            );
        }
    }
    let entries = out.get_mut(64..).unwrap_or_default();
    for (section, entry) in layout
        .sections
        .iter()
        .zip(entries.as_chunks_mut::<64>().0.iter_mut())
    {
        entry[0..4].copy_from_slice(&section.name_offset.to_le_bytes());
        entry[4..8].copy_from_slice(&section.sh_type.to_le_bytes());
        entry[8..16].copy_from_slice(&section.flags.to_le_bytes());
        entry[16..24].copy_from_slice(&section.addr.to_le_bytes());
        entry[24..32].copy_from_slice(&section.offset.to_le_bytes());
        entry[32..40].copy_from_slice(&section.size.to_le_bytes());
        entry[40..44].copy_from_slice(&section.link.to_le_bytes());
        entry[44..48].copy_from_slice(&section.info.to_le_bytes());
        entry[48..56].copy_from_slice(&section.align.to_le_bytes());
        entry[56..64].copy_from_slice(&section.entsize.to_le_bytes());
    }
    Ok(())
}

fn write_synthetic(input: &WriteInput<'_, '_, '_>, kind: Synthetic, out: &mut [u8]) -> Result<()> {
    let addresses = input.addresses;
    let synth = addresses.synth;
    match kind {
        Synthetic::None => {}
        Synthetic::BuildId => {
            write_build_id_header(out, synth.build_id.unwrap_or(0));
        }
        Synthetic::GnuProperty => {
            if let (Some(note), Some(dest)) = (
                &synth.property_note,
                synth
                    .property_note
                    .as_ref()
                    .and_then(|n| out.get_mut(..n.len())),
            ) {
                dest.copy_from_slice(note);
            }
        }
        Synthetic::Comment => {
            let text = super::synth::comment();
            if let Some(dest) = out.get_mut(..text.len()) {
                dest.copy_from_slice(&text);
            }
        }
        Synthetic::EhFrameEnd | Synthetic::Common => {}
        Synthetic::Got => {
            for (owner, entry) in synth.got.iter().zip(out.as_chunks_mut::<8>().0.iter_mut()) {
                let value = owner_value(addresses, owner);
                entry.copy_from_slice(&value.to_le_bytes());
            }
        }
        Synthetic::IgotPlt => {
            let reserved = usize::try_from(synth.got_plt_reserved).unwrap_or(0);
            let slots = out.as_chunks_mut::<8>().0.iter_mut().skip(reserved);
            for (owner, entry) in synth.iplt.iter().zip(slots) {
                // Filled by IRELATIVE at startup; hold the resolver address
                // meanwhile, as GNU ld does.
                let value = resolver_address(addresses, owner);
                entry.copy_from_slice(&value.to_le_bytes());
            }
        }
        Synthetic::Iplt => {
            let (base, ..) = addresses
                .layout
                .synthetic(Synthetic::Iplt)
                .unwrap_or_default();
            for (index, entry) in out
                .as_chunks_mut::<16>()
                .0
                .iter_mut()
                .enumerate()
                .take(synth.iplt.len())
            {
                let stub =
                    base.saturating_add(u64::try_from(index).unwrap_or(0).saturating_mul(16));
                let slot = addresses.igot_address(index).unwrap_or(0);
                x86_64::write_iplt(entry, stub, slot)
                    .map_err(|_| Error::Internal("IFUNC PLT slot out of range".into()))?;
            }
        }
        Synthetic::RelaIplt => {
            for (index, (owner, entry)) in synth
                .iplt
                .iter()
                .zip(out.as_chunks_mut::<24>().0.iter_mut())
                .enumerate()
            {
                let slot = addresses.igot_address(index).unwrap_or(0);
                let resolver = resolver_address(addresses, owner);
                entry[0..8].copy_from_slice(&slot.to_le_bytes());
                entry[8..16].copy_from_slice(&u64::from(R_X86_64_IRELATIVE).to_le_bytes());
                entry[16..24].copy_from_slice(&resolver.to_le_bytes());
            }
        }
        Synthetic::EhFrameHdr => {
            write_eh_frame_hdr(addresses, out);
        }
    }
    Ok(())
}

/// The value a GOT entry holds for `owner`.
fn owner_value(addresses: &Addresses<'_, '_>, owner: Owner) -> u64 {
    if let Some(stub) = addresses.iplt_address(owner) {
        return stub;
    }
    match owner {
        Owner::Global(id) => addresses.globals.get(id.index()).copied().unwrap_or(0),
        Owner::Local { file, symbol } => addresses
            .refs
            .target(file as usize, symbol as usize)
            .and_then(|t| addresses.symbol_address(&t, 0))
            .map_or(0, |(s, _)| s),
    }
}

/// The resolver address of IFUNC `owner`.
fn resolver_address(addresses: &Addresses<'_, '_>, owner: Owner) -> u64 {
    match owner {
        Owner::Global(id) => addresses.globals.get(id.index()).copied().unwrap_or(0),
        Owner::Local { file, symbol } => addresses
            .refs
            .target(file as usize, symbol as usize)
            .and_then(|t| addresses.symbol_address(&t, 0))
            .map_or(0, |(s, _)| s),
    }
}

fn write_eh_frame_hdr(addresses: &Addresses<'_, '_>, out: &mut [u8]) {
    let layout = addresses.layout;
    let Some((hdr, ..)) = layout.synthetic(Synthetic::EhFrameHdr) else {
        return;
    };
    let eh_frame = layout.by_name(b".eh_frame").map_or(0, |s| s.addr);
    let mut table: Vec<(u64, u64)> = Vec::new();
    for section in &addresses.eh_frames.sections {
        let Some(start) = addresses.section_address(section.id) else {
            continue;
        };
        for record in &section.records {
            let (true, Some(_), Some(pc_begin)) = (record.live, record.cie, record.pc_begin) else {
                continue;
            };
            let Some(rel) = section.relocs.get(pc_begin as usize) else {
                continue;
            };
            let Some(target) = addresses.refs.target(section.file, rel.symbol as usize) else {
                continue;
            };
            let Some((s, a)) = addresses.symbol_address(&target, rel.addend) else {
                continue;
            };
            let location = s.wrapping_add_signed(a);
            table.push((location, start.wrapping_add(u64::from(record.out_offset))));
        }
    }
    table.sort_unstable();
    let rel32 =
        |value: u64, base: u64| -> [u8; 4] { (value.wrapping_sub(base) as i32).to_le_bytes() };
    let Some(head) = out.get_mut(..12) else {
        return;
    };
    head[0] = 1;
    head[1] = 0x1b; // DW_EH_PE_pcrel | DW_EH_PE_sdata4
    head[2] = 0x03; // DW_EH_PE_udata4
    head[3] = 0x3b; // DW_EH_PE_datarel | DW_EH_PE_sdata4
    head[4..8].copy_from_slice(&rel32(eh_frame, hdr.wrapping_add(4)));
    head[8..12].copy_from_slice(&u32::try_from(table.len()).unwrap_or(0).to_le_bytes());
    let entries = out.get_mut(12..).unwrap_or_default();
    for ((location, fde), entry) in table.iter().zip(entries.as_chunks_mut::<8>().0.iter_mut()) {
        entry[0..4].copy_from_slice(&rel32(*location, hdr));
        entry[4..8].copy_from_slice(&rel32(*fde, hdr));
    }
}

/// Tombstone value for relocations from non-allocated sections to discarded
/// code: 1 in `.debug_ranges`/`.debug_loc` (where 0 ends a list), 0
/// elsewhere. W10 will replace this with the canonical rules in
/// `crate::debug`.
fn tombstone(section_name: &[u8]) -> u64 {
    if section_name == b".debug_ranges" || section_name == b".debug_loc" {
        1
    } else {
        0
    }
}

fn write_input(input: &WriteInput<'_, '_, '_>, id: SectionId, out: &mut [u8]) -> Result<()> {
    let addresses = input.addresses;
    let refs = &addresses.refs;
    let (file_index, section_index) = refs
        .sections
        .locate(id)
        .ok_or_else(|| Error::Internal("unknown section in output".into()))?;
    let file = refs
        .files
        .get(file_index)
        .ok_or_else(|| Error::Internal("unknown file in output".into()))?;
    let object = file
        .object
        .as_ref()
        .ok_or_else(|| Error::Internal("unparsed file in output".into()))?;
    let section = object
        .section(section_index)
        .ok_or_else(|| Error::Internal("unknown section in output".into()))?;
    let data = object.elf.section_data(&section.header)?;
    let base = addresses.section_address(id).unwrap_or(0);

    if section.kind == SectionKind::EhFrame {
        let Some(eh) = addresses
            .eh_frames
            .find(id)
            .and_then(|i| addresses.eh_frames.sections.get(i))
        else {
            return Ok(());
        };
        return write_eh_frame(input, eh, base, out);
    }

    if let Some(dest) = out.get_mut(..data.len()) {
        dest.copy_from_slice(data);
    }
    if section.relocs == 0 {
        return Ok(());
    }
    let relocations = object
        .section(section.relocs)
        .map(|r| object.elf.relocation_section(section.relocs, &r.header))
        .transpose()?
        .flatten();
    let Some(Relocations::Rela(relas)) = relocations.map(|r| r.relocations) else {
        return Ok(());
    };
    let alloc = section.header.sh_flags & SHF_ALLOC != 0;
    let order = file.position.raw();
    let mut skip = false;
    for rel in relas.iter() {
        if skip {
            skip = false;
            continue;
        }
        let report = |message: String| {
            input.diagnostics.emit(
                Diagnostic::error(message)
                    .at(location(refs, file_index, section_index, rel.offset))
                    .order(order),
            );
        };
        let Some(target) = refs.target(file_index, rel.symbol as usize) else {
            continue;
        };
        let is_ifunc = target.is_ifunc();
        let Ok(class) = x86_64::classify(
            rel.r_type,
            rel.addend,
            data,
            rel.offset,
            input.options.relax && !is_ifunc,
        ) else {
            continue; // Reported by the scan.
        };
        skip = class.kind.skips_next();
        if class.kind == Kind::None {
            continue;
        }
        let place = base.wrapping_add(rel.offset);
        let owner = Addresses::owner(&target, file_index, rel.symbol);
        let resolved = addresses.symbol_address(&target, rel.addend);
        let (mut s, a) = match resolved {
            Some(value) => value,
            None => {
                if alloc {
                    let name = symbol_name(refs, file_index, rel.symbol);
                    report(format!(
                        "relocation refers to a symbol in a discarded section: {name}"
                    ));
                    continue;
                }
                let value = tombstone(section.name);
                let _ = x86_64::write_value(out, rel.offset, class.width, value);
                continue;
            }
        };
        if is_ifunc
            && alloc
            && let Some(stub) = addresses.iplt_address(owner)
        {
            s = stub;
        }
        let sa = s.wrapping_add_signed(a);
        let tls = addresses.layout.tls.unwrap_or_default();
        let result = match class.kind {
            Kind::None => Ok(()),
            Kind::Abs => x86_64::write_value(out, rel.offset, class.width, sa),
            Kind::Pc => x86_64::write_value(out, rel.offset, class.width, sa.wrapping_sub(place)),
            Kind::GotPc | Kind::GotEntry => match addresses.got_address(owner) {
                Some(entry) => {
                    let value = if class.kind == Kind::GotPc {
                        entry.wrapping_add_signed(a).wrapping_sub(place)
                    } else {
                        entry
                            .wrapping_sub(addresses.got_base())
                            .wrapping_add_signed(a)
                    };
                    x86_64::write_value(out, rel.offset, class.width, value)
                }
                None => Err(ApplyError::BadInstruction),
            },
            Kind::RelaxGotPc => {
                x86_64::relax_got(out, rel.offset, class.kind, sa.wrapping_sub(place) as i64)
            }
            Kind::RelaxGotPcNoPic => x86_64::relax_got(out, rel.offset, class.kind, sa as i64),
            Kind::GotRel => x86_64::write_value(
                out,
                rel.offset,
                class.width,
                sa.wrapping_sub(addresses.got_base()),
            ),
            Kind::GotBasePc => x86_64::write_value(
                out,
                rel.offset,
                class.width,
                addresses
                    .got_base()
                    .wrapping_add_signed(a)
                    .wrapping_sub(place),
            ),
            Kind::Size => {
                let size = target.raw.map_or(0, |r| r.st_size);
                x86_64::write_value(out, rel.offset, class.width, size.wrapping_add_signed(a))
            }
            Kind::TpOff => {
                x86_64::write_value(out, rel.offset, class.width, sa.wrapping_sub(tls.tp()))
            }
            Kind::DtpOff => {
                let value = if alloc {
                    sa.wrapping_sub(tls.tp())
                } else {
                    sa.wrapping_sub(tls.start)
                };
                x86_64::write_value(out, rel.offset, class.width, value)
            }
            Kind::GdToLe | Kind::LdToLe | Kind::IeToLe | Kind::DescToLe | Kind::DescCallToLe => {
                x86_64::relax_tls(
                    out,
                    rel.offset,
                    class.kind,
                    sa.wrapping_sub(tls.tp()) as i64,
                )
            }
        };
        if let Err(error) = result {
            let type_name = reloc_name(EM_X86_64, rel.r_type)
                .map_or_else(|| rel.r_type.to_string(), str::to_owned);
            let name = symbol_name(refs, file_index, rel.symbol);
            let message = match error {
                ApplyError::Overflow => {
                    format!("relocation {type_name} out of range; references '{name}'")
                }
                ApplyError::OutOfBounds => {
                    format!("relocation {type_name} is outside its section; references '{name}'")
                }
                ApplyError::BadInstruction => format!(
                    "relocation {type_name} cannot be applied to this instruction; references '{name}'"
                ),
            };
            report(message);
        }
    }
    Ok(())
}

fn symbol_name(refs: &super::refs::Refs<'_, '_>, file: usize, symbol: u32) -> String {
    refs.files
        .get(file)
        .and_then(|f| f.object.as_ref())
        .and_then(|o| o.elf.symbols().get(symbol as usize).ok())
        .map_or_else(
            || format!("symbol {symbol}"),
            |s| String::from_utf8_lossy(s.name).into_owned(),
        )
}

fn write_eh_frame(
    input: &WriteInput<'_, '_, '_>,
    eh: &EhSection<'_>,
    base: u64,
    out: &mut [u8],
) -> Result<()> {
    let addresses = input.addresses;
    let refs = &addresses.refs;
    let eh_index = addresses.eh_frames.find(eh.id).unwrap_or(usize::MAX);
    for (record_index, record) in eh.records.iter().enumerate() {
        if !record.live {
            continue;
        }
        let start = record.offset as usize;
        let size = record.size as usize;
        let out_start = record.out_offset as usize;
        let (Some(bytes), Some(dest)) = (
            eh.data.get(start..start.saturating_add(size)),
            out.get_mut(out_start..out_start.saturating_add(size)),
        ) else {
            return Err(Error::Internal(
                ".eh_frame record outside its section".into(),
            ));
        };
        dest.copy_from_slice(bytes);
        let record_address = base.wrapping_add(u64::from(record.out_offset));
        if record.cie.is_some() {
            // The CIE pointer: distance from this field to the kept CIE.
            if let Some((cie_section, cie_record)) =
                addresses.eh_frames.cie_of(eh_index, record_index)
                && let Some(cie_eh) = addresses.eh_frames.sections.get(cie_section)
                && let Some(cie) = cie_eh.records.get(cie_record)
                && let Some(cie_base) = addresses.section_address(cie_eh.id)
            {
                let length_size = if bytes.get(..4) == Some(&[0xff; 4]) {
                    12u64
                } else {
                    4
                };
                let field = record_address.wrapping_add(length_size);
                let cie_address = cie_base.wrapping_add(u64::from(cie.out_offset));
                let pointer = u32::try_from(field.wrapping_sub(cie_address)).unwrap_or(0);
                let at = out_start.saturating_add(length_size as usize);
                if let Some(slot) = out.get_mut(at..at.saturating_add(4)) {
                    slot.copy_from_slice(&pointer.to_le_bytes());
                }
            }
        }
        for index in record.relocs.0..record.relocs.1 {
            let Some(rel) = eh.relocs.get(index as usize) else {
                continue;
            };
            let Some(target) = refs.target(eh.file, rel.symbol as usize) else {
                continue;
            };
            let Ok(class) = x86_64::classify(rel.r_type, rel.addend, eh.data, rel.offset, false)
            else {
                continue;
            };
            let local = rel
                .offset
                .wrapping_sub(u64::from(record.offset))
                .wrapping_add(u64::from(record.out_offset));
            let place = base.wrapping_add(local);
            let Some((s, a)) = addresses.symbol_address(&target, rel.addend) else {
                continue;
            };
            let sa = s.wrapping_add_signed(a);
            let value = match class.kind {
                Kind::Abs => sa,
                Kind::Pc => sa.wrapping_sub(place),
                _ => continue,
            };
            if x86_64::write_value(out, local, class.width, value).is_err() {
                input.diagnostics.emit(
                    Diagnostic::error("relocation in .eh_frame out of range".to_string())
                        .at(location(refs, eh.file, eh.index, rel.offset)),
                );
            }
        }
    }
    Ok(())
}
