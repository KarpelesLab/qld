//! Output writing (pipeline stage 11).
//!
//! The image is cut into disjoint chunks: the ELF and program headers, every
//! input section, merge group and synthetic part, the symbol and string
//! tables, and the section header table. [`OutputFile::write_chunks`] fills
//! them in parallel. Input sections are copied from the input mapping and
//! relocated in place; relocation problems are reported to the diagnostic
//! sink (every one, not just the first) and fail the link afterwards.
//!
//! Dynamic relocations of input sections are written by the `.rela.dyn`
//! chunk, which runs the same per-relocation decisions
//! ([`reloc::decide`]) over the sections the scan found to need them, then
//! sorts the table: relative relocations first, by offset (`-z combreloc`),
//! then the others by symbol and offset.

#![deny(clippy::arithmetic_side_effects)]

use rayon::prelude::*;

use crate::args::LinkOptions;
use crate::debug::tombstone::{DeadTarget, SectionTombstone, Tombstones};
use crate::diag::{Collect, Diagnostic, DiagnosticSink, Severity};
use crate::elf::read::consts::{ET_DYN, ET_EXEC, SHF_ALLOC, SHF_EXECINSTR};
use crate::elf::read::{
    ElfFormat, ElfKind, Endian, FileHeader, ProgramHeader, RawRecord, Relocations, SectionHeader,
};
use crate::error::{Error, Result};
use crate::ids::SectionId;
use crate::output::{ChunkRange, OutputFile};
use crate::symbols::SymbolFlags;

use super::arch::{
    self, ApplyError, Arch, DynKind, GotKind, Kind, RelaxValues, Width, width_bytes,
};
use super::defined::LinkerSymbols;
use super::dynsym::{self, DynamicPlan};
use super::ehframe::EhSection;
use super::export::PREEMPTIBLE;
use super::layout::{Layout, Member, Trailer};
use super::object::SectionKind;
use super::refs::Refs;
use super::reloc::{self, Context, Dynamic};
use super::rules::Synthetic;
use super::scan::{ScanResult, location};
use super::symtab::{SymtabPlan, write_strtab, write_symtab};
use super::synth::{Owner, SlotReloc, got_slot_relocs, write_build_id_header};
use super::values::Addresses;

/// What one output chunk holds.
#[derive(Clone, Copy, Debug)]
enum Chunk {
    Headers,
    Input(SectionId),
    /// An input section followed by this many bytes of no-op padding, the
    /// gap before the next member of a code section.
    PaddedInput(SectionId, u32),
    Merge(u32),
    Synthetic(Synthetic),
    Symtab,
    Strtab,
    Shstrtab,
    EmitRelocs(u32),
    Prerendered(usize),
    SectionHeaders,
    /// Script padding: section position, index in its fills.
    Fill(u32, u32),
    /// Script data: section position, index in its data.
    Data(u32, u32),
    /// Padding between the contents of a code section: no-op instructions.
    Nop,
    /// The Cortex-A53 erratum patches of a section: section position,
    /// index of their block in its data.
    Patches(u32, u32),
}

/// Adds no-op padding for the gaps of an executable section that nothing
/// else fills, as BFD's x86 default fill does. `members` is where the
/// section's member chunks start in `chunks`: a gap right after an input
/// section extends its chunk ([`Chunk::PaddedInput`]) rather than adding
/// one, which halves the chunks of a large code section (clang: 147,000 of
/// 323,000 chunks were padding).
fn push_code_padding(
    section: &super::layout::OutSection<'_>,
    members: usize,
    chunks: &mut Vec<(ChunkRange, Chunk)>,
) {
    let mut covered: Vec<(u64, u64)> = section
        .members
        .iter()
        .filter(|p| p.size > 0)
        .map(|p| (p.offset, p.size))
        .chain(section.fills.iter().map(|&(o, size, _)| (o, size)))
        .chain(
            section
                .data
                .iter()
                .map(|(o, b)| (*o, u64::try_from(b.len()).unwrap_or(0))),
        )
        .collect();
    covered.sort_unstable();
    let end_of = |range: &ChunkRange| range.offset.saturating_add(range.size);
    // The member chunks, in offset order; gaps come in offset order too, so
    // the member that ends where a gap starts is found by walking forward.
    let member_end = chunks.len();
    let mut next = members;
    let mut nops = Vec::new();
    let mut cursor = 0u64;
    for (offset, size) in covered.into_iter().chain([(section.size, 0)]) {
        if offset > cursor && cursor < section.size {
            let end = offset.min(section.size);
            let start = section.offset.saturating_add(cursor);
            let gap = end.saturating_sub(cursor);
            while next < member_end && chunks.get(next).is_some_and(|(r, _)| end_of(r) < start) {
                next = next.saturating_add(1);
            }
            match (chunks.get_mut(next), u32::try_from(gap)) {
                (Some((range, chunk)), Ok(pad)) if next < member_end && end_of(range) == start => {
                    match *chunk {
                        Chunk::Input(id) => {
                            *chunk = Chunk::PaddedInput(id, pad);
                            range.size = range.size.saturating_add(gap);
                        }
                        _ => nops.push((ChunkRange::new(start, gap), Chunk::Nop)),
                    }
                }
                _ => nops.push((ChunkRange::new(start, gap), Chunk::Nop)),
            }
        }
        cursor = cursor.max(offset.saturating_add(size));
    }
    chunks.extend(nops);
}

/// Inputs to the writer.
pub struct WriteInput<'w, 'x, 'a, F: crate::elf::read::ElfFormat = crate::elf::read::Elf64Le> {
    /// Options.
    pub options: &'w LinkOptions,
    /// Addresses (and through it, layout, merge, eh_frame, synthetic).
    pub addresses: &'w Addresses<'x, 'a, F>,
    /// The symbol table plan.
    pub symtab: &'w SymtabPlan,
    /// Linker-defined symbols.
    pub linker: &'w LinkerSymbols,
    /// The dynamic symbol table plan.
    pub dynamic: &'w DynamicPlan,
    /// The relocation scan (for sections with dynamic relocations).
    pub scan: &'w ScanResult,
    /// Relocation decision context.
    pub context: Context,
    /// Tombstone values for debug relocations to discarded code.
    pub tombstones: &'w Tombstones,
    /// The encoded `.relr.dyn` words ([`encode_relr`]).
    pub relr: &'w [u64],
    /// The entry address.
    pub entry: u64,
    /// Output sections rendered ahead of the write (compressed debug
    /// sections, and those compression did not shrink).
    pub prerendered: &'w [Prerendered],
    /// Diagnostics.
    pub diagnostics: &'w dyn DiagnosticSink,
}

/// An output section whose final bytes were produced before the write.
#[derive(Clone, Debug)]
pub struct Prerendered {
    /// Its position in [`Layout::sections`].
    pub position: u32,
    /// The bytes written for it.
    pub bytes: Vec<u8>,
    /// Whether `bytes` is the compressed form (the section's layout size
    /// then is the compressed size).
    pub compressed: bool,
}

/// Writes the output file and applies the build-id.
///
/// # Errors
///
/// Returns I/O errors, [`Error::Reported`] when relocations failed, and
/// [`Error::Internal`] for layout bugs.
pub fn write<F: crate::elf::read::ElfFormat>(input: &WriteInput<'_, '_, '_, F>) -> Result<()> {
    let layout = input.addresses.layout;
    if !input.options.no_warnings {
        for warning in &layout.warnings {
            input.diagnostics.emit(warning.clone());
        }
    }
    let raw = input
        .options
        .output_format
        .as_ref()
        .and_then(|format| super::rawout::Format::from_name(format.name()));
    let mut chunks: Vec<(ChunkRange, Chunk)> = Vec::new();
    let headers = layout.phoff.max(layout.kind.ehdr_size()).saturating_add(
        layout
            .kind
            .phdr_size()
            .saturating_mul(u64::try_from(layout.segments.len()).unwrap_or(0)),
    );
    chunks.push((ChunkRange::new(0, headers), Chunk::Headers));
    for (position, section) in layout.sections.iter().enumerate() {
        if !section.has_file_bytes() || section.size == 0 {
            continue;
        }
        if let Some(index) = input
            .prerendered
            .iter()
            .position(|p| p.position as usize == position)
        {
            chunks.push((
                ChunkRange::new(section.offset, section.size),
                Chunk::Prerendered(index),
            ));
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
            Trailer::Rela(target) => {
                chunks.push((
                    ChunkRange::new(section.offset, section.size),
                    Chunk::EmitRelocs(target),
                ));
            }
            Trailer::Generated => {
                return Err(Error::Internal(format!(
                    "{} was not rendered before the write",
                    String::from_utf8_lossy(section.name)
                )));
            }
            Trailer::None => {
                let position32 = u32::try_from(position).unwrap_or(u32::MAX);
                // The block of erratum patches starts at the first one.
                let patches = if section.flags & SHF_EXECINSTR != 0
                    && arch::aarch64_errata::enabled(input.options)
                {
                    arch::thunk::patches_in(&layout.thunks, section.output, 0, u64::MAX)
                        .map(|p| p.address)
                        .min()
                } else {
                    None
                };
                for (index, &(offset, size, _)) in section.fills.iter().enumerate() {
                    if size > 0 {
                        chunks.push((
                            ChunkRange::new(section.offset.saturating_add(offset), size),
                            Chunk::Fill(position32, u32::try_from(index).unwrap_or(u32::MAX)),
                        ));
                    }
                }
                for (index, (offset, bytes)) in section.data.iter().enumerate() {
                    let size = u64::try_from(bytes.len()).unwrap_or(0);
                    if size > 0 {
                        let index = u32::try_from(index).unwrap_or(u32::MAX);
                        let chunk = if patches == Some(section.addr.wrapping_add(*offset)) {
                            Chunk::Patches(position32, index)
                        } else {
                            Chunk::Data(position32, index)
                        };
                        chunks.push((
                            ChunkRange::new(section.offset.saturating_add(*offset), size),
                            chunk,
                        ));
                    }
                }
                let members = chunks.len();
                for placed in &section.members {
                    if placed.size == 0 {
                        continue;
                    }
                    let chunk = match placed.member {
                        Member::Input(id) => Chunk::Input(id),
                        Member::Merge(group) => Chunk::Merge(group),
                        Member::Synthetic(Synthetic::DynBss | Synthetic::Common) => continue,
                        Member::Synthetic(kind) => Chunk::Synthetic(kind),
                    };
                    let range =
                        ChunkRange::new(section.offset.saturating_add(placed.offset), placed.size);
                    chunks.push((range, chunk));
                }
                if section.flags & SHF_EXECINSTR != 0 {
                    push_code_padding(section, members, &mut chunks);
                }
            }
        }
    }
    let shnum = u64::try_from(layout.sections.len().saturating_add(1)).unwrap_or(0);
    chunks.push((
        ChunkRange::new(layout.shoff, shnum.saturating_mul(layout.kind.shdr_size())),
        Chunk::SectionHeaders,
    ));
    chunks.sort_by_key(|(range, _)| range.offset);
    let ranges: Vec<ChunkRange> = chunks.iter().map(|(range, _)| *range).collect();

    let path = input.options.output_path();
    let mut file = if raw.is_some() {
        OutputFile::in_memory(layout.file_size)?
    } else {
        OutputFile::create(
            &path,
            layout.file_size,
            &crate::output::OutputOptions::for_link(input.options),
        )?
    };
    // Chunks report into a collector; problems are emitted afterwards in
    // input order, so the diagnostics do not depend on scheduling.
    let collected = Collect::new();
    let build_id = layout
        .synthetic(Synthetic::BuildId)
        .map(|(_, offset, _)| offset.saturating_add(16));
    if let Some(offset) = build_id {
        // Lets the write backing hash chunks as it writes them.
        file.reserve_build_id(&input.options.build_id, offset);
    }
    let local = WriteInput {
        options: input.options,
        addresses: input.addresses,
        symtab: input.symtab,
        linker: input.linker,
        dynamic: input.dynamic,
        scan: input.scan,
        context: input.context,
        tombstones: input.tombstones,
        relr: input.relr,
        entry: input.entry,
        prerendered: input.prerendered,
        diagnostics: &collected,
    };
    file.write_chunks(&ranges, |index, out| {
        let Some(&(_, chunk)) = chunks.get(index) else {
            return Err(Error::Internal("chunk index out of range".into()));
        };
        write_chunk(&local, chunk, out)
    })?;
    emit_collected(collected, input)?;
    if let Some(offset) = build_id {
        file.apply_build_id(&input.options.build_id, offset)?;
    }
    if let Some(format) = raw {
        let name = path.as_os_str().as_encoded_bytes().to_vec();
        let bytes = super::rawout::render(format, layout, file.as_slice()?, input.entry, &name)?;
        drop(file);
        let mut options = crate::output::OutputOptions::for_link(input.options);
        if format != super::rawout::Format::Binary {
            options.mode = crate::output::FileMode::Regular;
        }
        let size = u64::try_from(bytes.len())
            .map_err(|_| Error::Limit("raw output larger than the address space".into()))?;
        let mut out = OutputFile::create(&path, size, &options)?;
        out.write_at(0, &bytes)?;
        out.finish()?;
        return Ok(());
    }
    file.finish()?;
    Ok(())
}

/// Emits the problems chunks reported, in input order; fails the link on
/// errors (unless `--noinhibit-exec`).
fn emit_collected<F: crate::elf::read::ElfFormat>(
    collected: Collect,
    input: &WriteInput<'_, '_, '_, F>,
) -> Result<()> {
    let mut problems = collected.take_sorted();
    problems.sort_by(|a, b| {
        let key = |d: &Diagnostic| {
            let location = d.locations.first();
            (
                d.order,
                location.map(|l| l.section.clone()),
                location.and_then(|l| l.offset),
            )
        };
        key(a).cmp(&key(b)).then_with(|| a.message.cmp(&b.message))
    });
    let errors = problems
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .count();
    for problem in problems {
        input.diagnostics.emit(problem);
    }
    if errors > 0 && !input.options.noinhibit_exec {
        return Err(Error::Reported { errors });
    }
    Ok(())
}

/// Renders the non-allocated `.debug*` output sections and compresses them
/// for `--compress-debug-sections`. A section keeps its uncompressed bytes
/// when compression does not make it smaller, as with GNU ld; either way
/// its bytes are returned, so the write does not relocate it again.
///
/// # Errors
///
/// Returns [`Error::Reported`] when relocations in the sections failed, and
/// [`Error::Internal`] for layout bugs.
pub fn prerender_debug_sections<F: crate::elf::read::ElfFormat>(
    input: &WriteInput<'_, '_, '_, F>,
    compression: crate::debug::section::OutputCompression,
) -> Result<Vec<Prerendered>> {
    let layout = input.addresses.layout;
    let collected = Collect::new();
    let local = WriteInput {
        diagnostics: &collected,
        ..*input
    };
    let mut out = Vec::new();
    for (position, section) in layout.sections.iter().enumerate() {
        if section.trailer != Trailer::None
            || section.is_alloc()
            || !section.has_file_bytes()
            || section.size == 0
            || !section.name.starts_with(b".debug")
        {
            continue;
        }
        let size = usize::try_from(section.size)
            .map_err(|_| Error::Limit("debug section larger than memory".into()))?;
        let mut bytes = vec![0u8; size];
        let mut chunks: Vec<(ChunkRange, Chunk)> = Vec::new();
        for placed in &section.members {
            if placed.size == 0 {
                continue;
            }
            let chunk = match placed.member {
                Member::Input(id) => Chunk::Input(id),
                Member::Merge(group) => Chunk::Merge(group),
                Member::Synthetic(kind) => Chunk::Synthetic(kind),
            };
            chunks.push((ChunkRange::new(placed.offset, placed.size), chunk));
        }
        chunks.sort_by_key(|(range, _)| range.offset);
        let ranges: Vec<ChunkRange> = chunks.iter().map(|(range, _)| *range).collect();
        let slices = crate::output::split_chunks(&mut bytes, &ranges)
            .map_err(|e| Error::Internal(format!("debug section layout: {e}")))?;
        let results: Vec<Result<()>> = slices
            .into_par_iter()
            .zip(chunks.par_iter())
            .map(|(slice, &(_, chunk))| write_chunk(&local, chunk, slice))
            .collect();
        results.into_iter().collect::<Result<()>>()?;
        let compressed =
            crate::debug::section::compress_section::<F>(&bytes, compression, section.align);
        let position =
            u32::try_from(position).map_err(|_| Error::Limit("too many output sections".into()))?;
        if compressed.len() < bytes.len() {
            out.push(Prerendered {
                position,
                bytes: compressed,
                compressed: true,
            });
        } else {
            out.push(Prerendered {
                position,
                bytes,
                compressed: false,
            });
        }
    }
    emit_collected(collected, input)?;
    Ok(out)
}

fn write_chunk<F: crate::elf::read::ElfFormat>(
    input: &WriteInput<'_, '_, '_, F>,
    chunk: Chunk,
    out: &mut [u8],
) -> Result<()> {
    let addresses = input.addresses;
    let layout = addresses.layout;
    match chunk {
        Chunk::Headers => write_headers(input, out),
        Chunk::SectionHeaders => write_section_headers_as::<F>(layout, out),
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
        Chunk::EmitRelocs(target) => super::emit::write(input, target, out),
        Chunk::Prerendered(index) => {
            if let Some(prerendered) = input.prerendered.get(index) {
                copy_into(out, &prerendered.bytes);
            }
            Ok(())
        }
        Chunk::Input(id) => write_input(input, id, out),
        Chunk::PaddedInput(id, pad) => {
            let content = out.len().saturating_sub(pad as usize);
            let (section, padding) = out.split_at_mut(content);
            input.context.arch.write_nops(padding);
            write_input(input, id, section)
        }
        Chunk::Fill(position, index) => {
            let pattern = layout
                .sections
                .get(position as usize)
                .and_then(|s| s.fills.get(index as usize))
                .and_then(|&(_, _, pattern)| layout.fill_patterns.get(pattern as usize));
            if let Some(pattern) = pattern
                && !pattern.is_empty()
            {
                for (slot, byte) in out.iter_mut().zip(pattern.iter().cycle()) {
                    *slot = *byte;
                }
            }
            Ok(())
        }
        Chunk::Nop => {
            input.context.arch.write_nops(out);
            Ok(())
        }
        Chunk::Patches(position, index) => write_patches(input, position, index, out),
        Chunk::Data(position, index) => {
            if let Some((_, bytes)) = layout
                .sections
                .get(position as usize)
                .and_then(|s| s.data.get(index as usize))
            {
                copy_into(out, bytes);
            }
            Ok(())
        }
    }
}

fn write_headers<F: crate::elf::read::ElfFormat>(
    input: &WriteInput<'_, '_, '_, F>,
    out: &mut [u8],
) -> Result<()> {
    write_headers_as::<F>(input, out)
}

fn write_headers_as<F: ElfFormat>(input: &WriteInput<'_, '_, '_, F>, out: &mut [u8]) -> Result<()> {
    let layout = input.addresses.layout;
    let too_small = || Error::Internal("header chunk too small".into());
    let ehdr_size = usize::try_from(layout.kind.ehdr_size()).unwrap_or(64);
    let header = out.get_mut(..ehdr_size).ok_or_else(too_small)?;
    let pic = input.addresses.synth.mode.is_some_and(|m| m.pic);
    let phoff = if layout.segments.is_empty() {
        0
    } else {
        layout.phoff
    };
    let phnum = u16::try_from(layout.segments.len())
        .map_err(|_| Error::Limit("too many program headers".into()))?;
    let shnum = layout.sections.len().saturating_add(1);
    let (shnum_field, shstrndx) = match u16::try_from(shnum) {
        Ok(n) if n < 0xff00 => (n, u16::try_from(layout.sections.len()).unwrap_or(0)),
        _ => (0, 0xffff),
    };
    let file_header = FileHeader {
        class: F::CLASS,
        data: <F::Endian as Endian>::ELF_DATA,
        ident_version: 1, // EV_CURRENT
        os_abi: if input.addresses.synth.iplt.is_empty() {
            0
        } else {
            3 // ELFOSABI_GNU
        },
        abi_version: 0,
        e_type: if pic { ET_DYN } else { ET_EXEC },
        e_machine: input.context.arch.machine(),
        e_version: 1,
        e_entry: input.entry,
        e_phoff: phoff,
        e_shoff: layout.shoff,
        e_flags: input.context.arch.output_flags(input.addresses.refs.files),
        e_ehsize: 0,
        e_phentsize: 0,
        e_shentsize: 0,
        e_phnum: phnum,
        e_shnum: shnum_field,
        e_shstrndx: shstrndx,
    };
    header.copy_from_slice(F::encode_ehdr(&file_header).as_bytes());

    let table_start = usize::try_from(phoff.max(layout.kind.ehdr_size())).unwrap_or(ehdr_size);
    let phdrs = out.get_mut(table_start..).ok_or_else(too_small)?;
    let size = <F::Phdr as RawRecord>::SIZE;
    for (segment, entry) in layout
        .segments
        .iter()
        .zip(phdrs.chunks_exact_mut(size.max(1)))
    {
        let header = ProgramHeader {
            p_type: segment.p_type,
            p_flags: segment.flags,
            p_offset: segment.offset,
            p_vaddr: segment.vaddr,
            p_paddr: segment.paddr.unwrap_or(segment.vaddr),
            p_filesz: segment.filesz,
            p_memsz: segment.memsz,
            p_align: segment.align,
        };
        entry.copy_from_slice(F::encode_phdr(&header).as_bytes());
    }
    Ok(())
}

fn write_section_headers_as<F: ElfFormat>(layout: &Layout<'_>, out: &mut [u8]) -> Result<()> {
    out.fill(0);
    let size = <F::Shdr as RawRecord>::SIZE;
    let shnum = layout.sections.len().saturating_add(1);
    if u16::try_from(shnum).map_or(true, |n| n >= 0xff00)
        && let Some(first) = out.get_mut(..size)
    {
        // Extended numbering: section 0 holds the count and the index.
        let header = SectionHeader {
            sh_size: u64::try_from(shnum).unwrap_or(0),
            sh_link: u32::try_from(layout.sections.len()).unwrap_or(0),
            ..SectionHeader::default()
        };
        first.copy_from_slice(F::encode_shdr(&header).as_bytes());
    }
    let entries = out.get_mut(size..).unwrap_or_default();
    for (section, entry) in layout
        .sections
        .iter()
        .zip(entries.chunks_exact_mut(size.max(1)))
    {
        let header = SectionHeader {
            sh_name: section.name_offset,
            sh_type: section.sh_type,
            sh_flags: section.flags,
            sh_addr: section.addr,
            sh_offset: section.offset,
            sh_size: section.size,
            sh_link: section.link,
            sh_info: section.info,
            sh_addralign: section.align,
            sh_entsize: section.entsize,
        };
        entry.copy_from_slice(F::encode_shdr(&header).as_bytes());
    }
    Ok(())
}

fn copy_into(out: &mut [u8], bytes: &[u8]) {
    if let Some(dest) = out.get_mut(..bytes.len()) {
        dest.copy_from_slice(bytes);
    }
}

fn write_synthetic<F: crate::elf::read::ElfFormat>(
    input: &WriteInput<'_, '_, '_, F>,
    kind: Synthetic,
    out: &mut [u8],
) -> Result<()> {
    write_synthetic_as::<F>(input, kind, out)
}

fn write_synthetic_as<F: ElfFormat>(
    input: &WriteInput<'_, '_, '_, F>,
    kind: Synthetic,
    out: &mut [u8],
) -> Result<()> {
    let addresses = input.addresses;
    let synth = addresses.synth;
    let plan = input.dynamic;
    match kind {
        Synthetic::None | Synthetic::EhFrameEnd | Synthetic::Common | Synthetic::DynBss => {}
        Synthetic::DynRelro => out.fill(0),
        Synthetic::BuildId => {
            write_build_id_header(out, synth.build_id.unwrap_or(0));
        }
        Synthetic::GnuProperty => {
            if let Some(note) = &synth.property_note {
                copy_into(out, note);
            }
        }
        Synthetic::Interp => {
            if let Some(interp) = &synth.interp {
                copy_into(out, interp);
            }
        }
        Synthetic::Comment => copy_into(out, &super::synth::comment()),
        Synthetic::DynStr => copy_into(out, &plan.dynstr),
        Synthetic::GnuHash => copy_into(out, &plan.gnu_hash),
        Synthetic::Hash => copy_into(out, &plan.sysv_hash),
        Synthetic::VerNeed => copy_into(out, &plan.verneed),
        Synthetic::VerDef => copy_into(out, &plan.verdef),
        Synthetic::VerSym => {
            for (value, slot) in plan
                .versym
                .iter()
                .zip(out.as_chunks_mut::<2>().0.iter_mut())
            {
                *slot = <F::Endian as Endian>::put_u16(*value);
            }
        }
        Synthetic::DynSym => dynsym::write_dynsym::<F>(plan, addresses, out),
        Synthetic::Dynamic => dynsym::write_dynamic::<F>(plan, addresses, out),
        Synthetic::RelaDyn => write_rela_dyn::<F>(input, out)?,
        Synthetic::RelrDyn => {
            // Words past the encoding (the section keeps the size layout
            // planned) are empty bitmaps, which the dynamic linker skips.
            let mut words = input.relr.iter().copied();
            #[allow(
                clippy::chunks_exact_to_as_chunks,
                reason = "the size is an associated constant, not a literal"
            )]
            for slot in out.chunks_exact_mut(<F::Word as RawRecord>::SIZE.max(1)) {
                slot.copy_from_slice(F::encode_word(words.next().unwrap_or(1)).as_bytes());
            }
        }
        Synthetic::Got => write_got::<F>(input, out),
        Synthetic::GotPlt => write_got_plt::<F>(input, out),
        Synthetic::Plt => write_plt(input, out)?,
        Synthetic::PltSec => {
            let arch = synth.arch;
            let (base, ..) = addresses
                .layout
                .synthetic(Synthetic::PltSec)
                .unwrap_or_default();
            let size = arch.plt_sec_entry_size(synth.plt_flags());
            let step = usize::try_from(size).unwrap_or(16);
            for (index, entry) in out.chunks_exact_mut(step).enumerate() {
                let index64 = u64::try_from(index).unwrap_or(u64::MAX);
                let address = base.saturating_add(index64.saturating_mul(size));
                let slot = addresses.igot_address(index).unwrap_or(0);
                arch.write_plt_jump(
                    entry,
                    address,
                    slot,
                    synth.plt_flags(),
                    addresses.got_base(),
                )
                .map_err(|_| Error::Internal("PLT slot out of range".into()))?;
            }
        }
        Synthetic::PltGot => {
            let arch = synth.arch;
            let (base, ..) = addresses
                .layout
                .synthetic(Synthetic::PltGot)
                .unwrap_or_default();
            let size = usize::try_from(arch.plt_got_entry_size(synth.plt_flags())).unwrap_or(8);
            for (index, owner) in synth.plt_got.iter().enumerate() {
                let start = index.saturating_mul(size);
                let Some(entry) = out.get_mut(start..start.saturating_add(size)) else {
                    break;
                };
                let address = base.saturating_add(u64::try_from(start).unwrap_or(0));
                let slot = addresses.got_address(owner).unwrap_or(0);
                arch.write_plt_jump(
                    entry,
                    address,
                    slot,
                    synth.plt_flags(),
                    addresses.got_base(),
                )
                .map_err(|_| Error::Internal("PLT GOT slot out of range".into()))?;
            }
        }
        Synthetic::RelaPlt => write_rela_plt::<F>(input, out),
        Synthetic::EhFrameHdr => {
            write_eh_frame_hdr::<F>(addresses, out);
        }
    }
    Ok(())
}

/// The value a GOT entry holds for `owner`, before dynamic relocation.
fn owner_value<F: crate::elf::read::ElfFormat>(
    addresses: &Addresses<'_, '_, F>,
    owner: Owner,
) -> u64 {
    if let Some(stub) = addresses.iplt_address(owner) {
        return stub;
    }
    symbol_value(addresses, owner)
}

/// The address of `owner`'s symbol.
fn symbol_value<F: crate::elf::read::ElfFormat>(
    addresses: &Addresses<'_, '_, F>,
    owner: Owner,
) -> u64 {
    match owner {
        Owner::Global(id) => addresses.globals.get(id.index()).copied().unwrap_or(0),
        Owner::Local { file, symbol } => addresses
            .refs
            .target(file as usize, symbol as usize)
            .and_then(|t| addresses.symbol_address(&t, 0))
            .map_or(0, |(s, _)| s),
    }
}

fn write_got<F: ElfFormat>(input: &WriteInput<'_, '_, '_, F>, out: &mut [u8]) {
    let addresses = input.addresses;
    let synth = addresses.synth;
    let Some((base, ..)) = addresses.layout.synthetic(Synthetic::Got) else {
        return;
    };
    let tls = addresses.layout.tls.unwrap_or_default();
    let mode = synth.mode;
    let rel = synth.arch.uses_rel();
    let size = <F::Word as RawRecord>::SIZE;
    let size64 = u64::try_from(size).unwrap_or(8).max(1);
    let mut put = |address: u64, value: u64| {
        let start = address.wrapping_sub(base);
        if let Some(word) = usize::try_from(start)
            .ok()
            .and_then(|s| out.get_mut(s..s.checked_add(size)?))
        {
            word.copy_from_slice(F::encode_word(value).as_bytes());
        }
    };
    let refs = &addresses.refs;
    // PowerPC64 keeps the TOC pointer's link-time value in the first word.
    if synth.arch.got_header_words() > 0 {
        put(base, addresses.got_base());
    }
    let dtv_offset = synth.arch.dtv_offset();
    for (list, kind) in [
        (&synth.got, GotKind::Address),
        (&synth.tlsgd, GotKind::TlsGd),
        (&synth.gottpoff, GotKind::TpOff),
        (&synth.tlsdesc, GotKind::TlsDesc),
    ] {
        for owner in list.iter() {
            let Some(address) = addresses.got_entry_address(owner, kind) else {
                continue;
            };
            let relocs = match mode {
                Some(mode) => got_slot_relocs(refs, mode, owner, kind),
                None => [SlotReloc::None; 2],
            };
            let value = owner_value(addresses, owner);
            match kind {
                GotKind::Address => {
                    let word = match relocs[0] {
                        SlotReloc::Symbolic(_) => 0,
                        _ => value,
                    };
                    put(address, word);
                }
                GotKind::TpOff => {
                    let word = match relocs[0] {
                        SlotReloc::None => value.wrapping_sub(tls.tp(synth.arch)),
                        // `SHT_REL`: the offset in the module's block is the
                        // addend, so it is in the word.
                        SlotReloc::Module(_) if rel => value.wrapping_sub(tls.start),
                        _ => 0,
                    };
                    put(address, word);
                }
                GotKind::TlsGd => {
                    let dtpoff = value.wrapping_sub(tls.start).wrapping_sub(dtv_offset);
                    let (module, offset) = match relocs {
                        [SlotReloc::None, SlotReloc::None] => (1, dtpoff),
                        [_, SlotReloc::None] => (0, dtpoff),
                        [_, SlotReloc::Module(_)] if rel => (0, dtpoff),
                        _ => (0, 0),
                    };
                    put(address, module);
                    put(address.wrapping_add(size64), offset);
                }
                GotKind::TlsDesc => {
                    // `SHT_REL`: the descriptor's argument word holds the
                    // addend, the offset of a local variable in its block.
                    let argument = match relocs[0] {
                        SlotReloc::Module(_) if rel => value.wrapping_sub(tls.start),
                        _ => 0,
                    };
                    put(address, 0);
                    put(address.wrapping_add(size64), argument);
                }
                GotKind::TlsLd => {
                    put(address, 0);
                    put(address.wrapping_add(size64), 0);
                }
            }
        }
    }
}

fn write_got_plt<F: ElfFormat>(input: &WriteInput<'_, '_, '_, F>, out: &mut [u8]) {
    let addresses = input.addresses;
    let synth = addresses.synth;
    let size = <F::Word as RawRecord>::SIZE.max(1);
    let reserved = usize::try_from(synth.got_plt_reserved).unwrap_or(0);
    if synth.dynamic() {
        if synth.arch.got_plt_holds_dynamic()
            && let Some(first) = out.get_mut(..size)
        {
            let dynamic = addresses
                .layout
                .synthetic(Synthetic::Dynamic)
                .map_or(0, |(addr, ..)| addr);
            first.copy_from_slice(F::encode_word(dynamic).as_bytes());
        }
        let slots = out.chunks_exact_mut(size).skip(reserved);
        for (index, (owner, slot)) in synth
            .plt
            .iter()
            .chain(synth.iplt.iter())
            .zip(slots)
            .enumerate()
        {
            let index64 = u64::try_from(index).unwrap_or(u64::MAX);
            let value = if synth.iplt.index(owner).is_some() {
                symbol_value(addresses, owner)
            } else {
                let lazy = addresses.lazy_plt_address(index64).unwrap_or(0);
                let plt = addresses
                    .layout
                    .synthetic(Synthetic::Plt)
                    .map_or(0, |(addr, ..)| addr);
                synth.arch.lazy_slot_value(plt, lazy, synth.plt_flags())
            };
            slot.copy_from_slice(F::encode_word(value).as_bytes());
        }
        return;
    }
    let slots = out.chunks_exact_mut(size).skip(reserved);
    for (owner, entry) in synth.iplt.iter().zip(slots) {
        // Filled by IRELATIVE at startup; hold the resolver address
        // meanwhile, as GNU ld does.
        let value = symbol_value(addresses, owner);
        entry.copy_from_slice(F::encode_word(value).as_bytes());
    }
}

fn write_plt<F: crate::elf::read::ElfFormat>(
    input: &WriteInput<'_, '_, '_, F>,
    out: &mut [u8],
) -> Result<()> {
    let addresses = input.addresses;
    let synth = addresses.synth;
    let arch = synth.arch;
    let flags = synth.plt_flags();
    let (base, ..) = addresses
        .layout
        .synthetic(Synthetic::Plt)
        .unwrap_or_default();
    let range = || Error::Internal("PLT slot out of range".into());
    if !synth.dynamic() {
        let size = arch.iplt_entry_size(flags);
        let step = usize::try_from(size).unwrap_or(16);
        for (index, entry) in out
            .chunks_exact_mut(step)
            .enumerate()
            .take(synth.iplt.len())
        {
            let stub = base.saturating_add(u64::try_from(index).unwrap_or(0).saturating_mul(size));
            let slot = addresses.igot_address(index).unwrap_or(0);
            arch.write_iplt(entry, stub, slot, flags, addresses.got_base())
                .map_err(|_| range())?;
        }
        return Ok(());
    }
    let got_plt = addresses
        .layout
        .synthetic(Synthetic::GotPlt)
        .map_or(0, |(addr, ..)| addr);
    let header_size = usize::try_from(arch.plt_header_size(flags)).unwrap_or(16);
    let Some((header, rest)) = out.split_at_mut_checked(header_size) else {
        return Ok(());
    };
    arch.write_plt_header(header, base, got_plt, flags)
        .map_err(|_| range())?;
    let step = usize::try_from(arch.plt_entry_size(flags)).unwrap_or(16);
    for (index, entry) in rest.chunks_exact_mut(step).enumerate() {
        let index64 = u64::try_from(index).unwrap_or(u64::MAX);
        let address = addresses.lazy_plt_address(index64).unwrap_or(0);
        let slot = addresses.igot_address(index).unwrap_or(0);
        let reloc_index = u32::try_from(index).map_err(|_| range())?;
        arch.write_plt_entry(entry, address, slot, reloc_index, base, flags)
            .map_err(|_| range())?;
    }
    Ok(())
}

/// The size of one dynamic relocation entry: `Elf_Rel` where the
/// architecture uses `SHT_REL`, else `Elf_Rela`.
fn dyn_entry_size<F: ElfFormat>(arch: Arch) -> usize {
    if arch.uses_rel() {
        <F::Rel as RawRecord>::SIZE
    } else {
        <F::Rela as RawRecord>::SIZE
    }
}

/// Encodes a dynamic relocation into `out`, an entry of
/// [`dyn_entry_size`]: an `Elf_Rel` (its addend is in the word it
/// relocates) or an `Elf_Rela`.
fn put_rela<F: ElfFormat>(out: &mut [u8], offset: u64, symbol: u32, r_type: u32, addend: i64) {
    let rel = crate::elf::read::Relocation {
        offset,
        symbol,
        r_type,
        addend,
    };
    if out.len() == <F::Rel as RawRecord>::SIZE {
        out.copy_from_slice(F::encode_rel(&rel).as_bytes());
    } else if let Some(entry) = out.get_mut(..<F::Rela as RawRecord>::SIZE) {
        entry.copy_from_slice(F::encode_rela(&rel).as_bytes());
    }
}

fn write_rela_plt<F: ElfFormat>(input: &WriteInput<'_, '_, '_, F>, out: &mut [u8]) {
    let addresses = input.addresses;
    let synth = addresses.synth;
    let irelative = synth.arch.dyn_reloc(DynKind::Irelative);
    let entry_size = dyn_entry_size::<F>(synth.arch).max(1);
    if !synth.dynamic() {
        for (index, (owner, entry)) in synth
            .iplt
            .iter()
            .zip(out.chunks_exact_mut(entry_size))
            .enumerate()
        {
            let slot = addresses.igot_address(index).unwrap_or(0);
            let resolver = symbol_value(addresses, owner);
            put_rela::<F>(entry, slot, 0, irelative, resolver as i64);
        }
        return;
    }
    for (index, (owner, entry)) in synth
        .plt
        .iter()
        .chain(synth.iplt.iter())
        .zip(out.chunks_exact_mut(entry_size))
        .enumerate()
    {
        let slot = addresses.igot_address(index).unwrap_or(0);
        match owner {
            Owner::Global(id) if synth.iplt.index(owner).is_none() => {
                put_rela::<F>(
                    entry,
                    slot,
                    input.dynamic.index_of(id),
                    synth.arch.dyn_reloc(DynKind::JumpSlot),
                    0,
                );
            }
            _ => {
                let resolver = symbol_value(addresses, owner);
                put_rela::<F>(entry, slot, 0, irelative, resolver as i64);
            }
        }
    }
}

/// One dynamic relocation before it is encoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct DynReloc {
    /// Sort class: relative relocations first, `IRELATIVE` last.
    class: u8,
    symbol: u32,
    offset: u64,
    r_type: u32,
    addend: i64,
    /// A relative relocation that `.relr.dyn` can hold.
    packable: bool,
}

fn dyn_reloc(arch: Arch, offset: u64, symbol: u32, kind: DynKind, addend: i64) -> DynReloc {
    let class = match kind {
        DynKind::Relative => 0,
        DynKind::Irelative => 2,
        _ => 1,
    };
    DynReloc {
        class,
        symbol,
        offset,
        r_type: arch.dyn_reloc(kind),
        addend,
        packable: false,
    }
}

/// Every dynamic relocation of the output except `.rela.plt`'s, unsorted:
/// GOT entries, copy relocations, and input sections (in parallel).
fn collect_dyn_relocs<F: crate::elf::read::ElfFormat>(
    addresses: &Addresses<'_, '_, F>,
    context: &Context,
    plan: &DynamicPlan,
    scan: &ScanResult,
) -> Vec<DynReloc> {
    let synth = addresses.synth;
    let refs = &addresses.refs;
    let Some(mode) = synth.mode else {
        return Vec::new();
    };
    let arch = context.arch;
    let tls = addresses.layout.tls.unwrap_or_default();
    let mut relocs: Vec<DynReloc> = Vec::new();
    for (list, kind) in [
        (&synth.got, GotKind::Address),
        (&synth.tlsgd, GotKind::TlsGd),
        (&synth.gottpoff, GotKind::TpOff),
        (&synth.tlsdesc, GotKind::TlsDesc),
    ] {
        for owner in list.iter() {
            let Some(address) = addresses.got_entry_address(owner, kind) else {
                continue;
            };
            let symbol = match owner {
                Owner::Global(id) => plan.index_of(id),
                Owner::Local { .. } => 0,
            };
            let value = owner_value(addresses, owner);
            for (word, reloc) in got_slot_relocs(refs, mode, owner, kind)
                .into_iter()
                .enumerate()
            {
                let at = address.wrapping_add(if word == 0 {
                    0
                } else {
                    arch.kind().word_size()
                });
                match reloc {
                    SlotReloc::None => {}
                    SlotReloc::Relative => {
                        let mut reloc = dyn_reloc(arch, at, 0, DynKind::Relative, value as i64);
                        reloc.packable = true;
                        relocs.push(reloc);
                    }
                    SlotReloc::Symbolic(dyn_kind) => {
                        relocs.push(dyn_reloc(arch, at, symbol, dyn_kind, 0));
                    }
                    SlotReloc::Module(dyn_kind) => {
                        let addend = if dyn_kind == DynKind::DtpMod {
                            0
                        } else {
                            value.wrapping_sub(tls.start) as i64
                        };
                        relocs.push(dyn_reloc(arch, at, 0, dyn_kind, addend));
                    }
                }
            }
        }
    }
    if synth.tlsld
        && let Some(address) =
            addresses.got_entry_address(Owner::Local { file: 0, symbol: 0 }, GotKind::TlsLd)
    {
        relocs.push(dyn_reloc(arch, address, 0, DynKind::DtpMod, 0));
    }
    if !arch.irelative_in_rela_plt() {
        let first = synth.plt.len();
        for (index, owner) in synth.iplt.iter().enumerate() {
            let slot = addresses
                .igot_address(first.saturating_add(index))
                .unwrap_or(0);
            let resolver = symbol_value(addresses, owner);
            relocs.push(dyn_reloc(
                arch,
                slot,
                0,
                DynKind::Irelative,
                resolver as i64,
            ));
        }
    }
    for copy in &synth.copies {
        let address = addresses
            .globals
            .get(copy.symbol.index())
            .copied()
            .unwrap_or(0);
        relocs.push(dyn_reloc(
            arch,
            address,
            plan.index_of(copy.symbol),
            DynKind::Copy,
            0,
        ));
    }
    let sections: Vec<(usize, u32)> = scan
        .files
        .iter()
        .enumerate()
        .flat_map(|(file, scan)| scan.dyn_sections.iter().map(move |d| (file, d.section)))
        .collect();
    let from_sections: Vec<Vec<DynReloc>> = sections
        .par_iter()
        .map(|&(file, section)| section_dyn_relocs(addresses, context, plan, file, section))
        .collect();
    for list in from_sections {
        relocs.extend(list);
    }
    relocs
}

fn write_rela_dyn<F: ElfFormat>(input: &WriteInput<'_, '_, '_, F>, out: &mut [u8]) -> Result<()> {
    let synth = input.addresses.synth;
    let mut relocs = collect_dyn_relocs(input.addresses, &input.context, input.dynamic, input.scan);
    if synth.relr {
        relocs.retain(|r| !r.packable);
    }
    let expected = usize::try_from(synth.rela_dyn_count()).unwrap_or(usize::MAX);
    if relocs.len() != expected {
        return Err(Error::Internal(format!(
            "dynamic relocation count changed after planning ({expected} planned, {} made)",
            relocs.len()
        )));
    }
    relocs.par_sort_unstable();
    let entry_size = dyn_entry_size::<F>(synth.arch).max(1);
    for (reloc, entry) in relocs.iter().zip(out.chunks_exact_mut(entry_size)) {
        put_rela::<F>(
            entry,
            reloc.offset,
            reloc.symbol,
            reloc.r_type,
            reloc.addend,
        );
    }
    Ok(())
}

/// The addresses of the relative relocations `.relr.dyn` holds, sorted.
#[must_use]
pub fn relr_addresses<F: crate::elf::read::ElfFormat>(
    addresses: &Addresses<'_, '_, F>,
    context: &Context,
    plan: &DynamicPlan,
    scan: &ScanResult,
) -> Vec<u64> {
    let mut places: Vec<u64> = collect_dyn_relocs(addresses, context, plan, scan)
        .into_iter()
        .filter(|r| r.packable)
        .map(|r| r.offset)
        .collect();
    places.par_sort_unstable();
    places.dedup();
    places
}

/// Encodes sorted, even relocation addresses as `SHT_RELR` words: an
/// address entry, then bitmaps of the following 63 words, repeatedly.
#[must_use]
pub fn encode_relr(places: &[u64], kind: ElfKind) -> Vec<u64> {
    let word = kind.word_size();
    // One address bit, then one bitmap bit per following word.
    let bits = word.saturating_mul(8).saturating_sub(1);
    let span = bits.saturating_mul(word);
    let mut words = Vec::new();
    let mut i = 0usize;
    while let Some(&start) = places.get(i) {
        words.push(start);
        let mut base = start.wrapping_add(word);
        i = i.saturating_add(1);
        loop {
            let mut bitmap = 0u64;
            while let Some(&place) = places.get(i) {
                let delta = place.wrapping_sub(base);
                if place < base || delta >= span || delta.checked_rem(word) != Some(0) {
                    break;
                }
                bitmap |= 1u64 << delta.checked_div(word).unwrap_or(0);
                i = i.saturating_add(1);
            }
            if bitmap == 0 {
                break;
            }
            words.push((bitmap << 1) | 1);
            base = base.wrapping_add(span);
        }
    }
    words
}

/// The dynamic relocations of section `section` of `file`, by re-running
/// the scan's decisions.
fn section_dyn_relocs<F: crate::elf::read::ElfFormat>(
    addresses: &Addresses<'_, '_, F>,
    context: &Context,
    plan: &DynamicPlan,
    file_index: usize,
    section_index: u32,
) -> Vec<DynReloc> {
    let refs = &addresses.refs;
    let mut out = Vec::new();
    let Some(object) = refs.files.get(file_index).and_then(|f| f.object.as_ref()) else {
        return out;
    };
    let Some(section) = object.section(section_index) else {
        return out;
    };
    let Some(id) = refs.sections.id(file_index, section_index) else {
        return out;
    };
    // Merged pieces and code linker relaxation shrank (RISC-V) move
    // offsets; other sections keep them.
    let moved = refs.sections.kind_in(file_index, section_index) == Some(SectionKind::Merge)
        || !addresses.layout.relax.is_empty();
    let data = if section.kind == SectionKind::Merge || section.is_nobits() {
        &[][..]
    } else {
        object.section_data(section).unwrap_or_default()
    };
    let Some(Ok(Some(relocations))) = object
        .section(section.relocs)
        .map(|r| object.elf.relocation_section(section.relocs, &r.header))
    else {
        return out;
    };
    let arch = context.arch;
    let base = addresses.section_address(id).unwrap_or(0);
    let mut skip = false;
    arch::for_each_relocation!(arch, relocations.relocations, data, |rel| {
        if skip {
            skip = false;
            continue;
        }
        let Some(target) = refs.target(file_index, rel.symbol as usize) else {
            continue;
        };
        let flags = target
            .global
            .map_or(SymbolFlags::EMPTY, |id| refs.symbols.flags(id));
        let Ok(decision) =
            reloc::decide(context, &rel, data, &target, flags, section.header.sh_flags)
        else {
            continue;
        };
        skip = decision.class.skip_next;
        if decision.problem.is_some() {
            continue;
        }
        let place = if moved {
            addresses
                .section_offset_address(file_index, section_index, rel.offset)
                .unwrap_or(0)
        } else {
            base.wrapping_add(rel.offset)
        };
        match decision.dynamic {
            Dynamic::None => {}
            Dynamic::Relative => {
                let owner = Addresses::<F>::owner(&target, file_index, rel.symbol);
                let (s, a) = target_value(addresses, &target, owner, rel.addend);
                let mut reloc = dyn_reloc(
                    context.arch,
                    place,
                    0,
                    DynKind::Relative,
                    s.wrapping_add_signed(a) as i64,
                );
                reloc.packable = reloc::packable(section.header.sh_addralign, rel.offset);
                out.push(reloc);
            }
            Dynamic::Symbolic(dyn_kind) => {
                let symbol = target.global.map_or(0, |id| plan.index_of(id));
                out.push(dyn_reloc(context.arch, place, symbol, dyn_kind, rel.addend));
            }
        }
    });
    out
}

/// `(S, A)` for a relocation: IFUNCs resolve to their PLT stub, calls to
/// preemptible symbols to their PLT entry.
fn target_value<F: crate::elf::read::ElfFormat>(
    addresses: &Addresses<'_, '_, F>,
    target: &super::refs::Target,
    owner: Owner,
    addend: i64,
) -> (u64, i64) {
    let (mut s, a) = addresses
        .symbol_address(target, addend)
        .unwrap_or((0, addend));
    if target.is_ifunc()
        && let Some(stub) = addresses.iplt_address(owner)
    {
        s = stub;
    }
    (s, a)
}

fn write_eh_frame_hdr<F: ElfFormat>(addresses: &Addresses<'_, '_, F>, out: &mut [u8]) {
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
            let Some(rel) = section.reloc(pc_begin as usize) else {
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
    let rel32 = |value: u64, base: u64| -> [u8; 4] {
        <F::Endian as Endian>::put_u32(value.wrapping_sub(base) as u32)
    };
    let Some(head) = out.get_mut(..12) else {
        return;
    };
    head[0] = 1;
    head[1] = 0x1b; // DW_EH_PE_pcrel | DW_EH_PE_sdata4
    head[2] = 0x03; // DW_EH_PE_udata4
    head[3] = 0x3b; // DW_EH_PE_datarel | DW_EH_PE_sdata4
    head[4..8].copy_from_slice(&rel32(eh_frame, hdr.wrapping_add(4)));
    head[8..12].copy_from_slice(&<F::Endian as Endian>::put_u32(
        u32::try_from(table.len()).unwrap_or(0),
    ));
    let entries = out.get_mut(12..).unwrap_or_default();
    for ((location, fde), entry) in table.iter().zip(entries.as_chunks_mut::<8>().0.iter_mut()) {
        entry[0..4].copy_from_slice(&rel32(*location, hdr));
        entry[4..8].copy_from_slice(&rel32(*fde, hdr));
    }
}

/// Why a relocation's target section is not in the output, if it is not.
pub(crate) fn dead_target<F: crate::elf::read::ElfFormat>(
    refs: &Refs<'_, '_, F>,
    target: &super::refs::Target,
) -> Option<DeadTarget> {
    let id = refs.target_section(target)?;
    if refs.sections.is_live(id) {
        return None;
    }
    Some(if refs.sections.resolve(id).is_some() {
        DeadTarget::Folded
    } else {
        DeadTarget::Discarded
    })
}

/// The Cortex-A53 erratum patches of input section `id`, which starts at
/// `base` and is `len` bytes long, as `(offset of the site, patch address)`.
fn erratum_patches<F: crate::elf::read::ElfFormat>(
    input: &WriteInput<'_, '_, '_, F>,
    id: SectionId,
    base: u64,
    len: usize,
) -> Vec<(u64, u64)> {
    if !arch::aarch64_errata::enabled(input.options) {
        return Vec::new();
    }
    let layout = input.addresses.layout;
    let Some(output) = layout
        .section_shndx
        .get(id.index())
        .and_then(|&shndx| layout.output_of_shndx(shndx))
    else {
        return Vec::new();
    };
    let end = base.saturating_add(u64::try_from(len).unwrap_or(u64::MAX));
    arch::thunk::patches_in(&layout.thunks, output, base, end)
        .filter_map(|p| match p.patch {
            Some((section, offset)) if section == id => Some((offset, p.address)),
            _ => None,
        })
        .collect()
}

/// Writes input section `id`: its relocated contents, then the branches to
/// its Cortex-A53 erratum patches.
fn write_input<F: crate::elf::read::ElfFormat>(
    input: &WriteInput<'_, '_, '_, F>,
    id: SectionId,
    out: &mut [u8],
) -> Result<()> {
    relocate_input(input, id, out)?;
    let base = input.addresses.section_address(id).unwrap_or(0);
    for (offset, patch) in erratum_patches(input, id, base, out.len()) {
        let site = base.wrapping_add(offset);
        let written = crate::arch::aarch64::erratum_branch(site, patch)
            .ok()
            .and_then(|branch| {
                crate::arch::aarch64::write_insn(out, usize::try_from(offset).ok()?, branch)
            });
        if written.is_none() {
            input.diagnostics.emit(Diagnostic::error(format!(
                "Cortex-A53 erratum patch at {patch:#x} is out of range of {site:#x}"
            )));
        }
    }
    Ok(())
}

/// Writes the Cortex-A53 erratum patches of the output section at
/// `position`, whose block is its data entry `index`: each one is the
/// relocated instruction it replaces, and a branch back after it.
fn write_patches<F: crate::elf::read::ElfFormat>(
    input: &WriteInput<'_, '_, '_, F>,
    position: u32,
    index: u32,
    out: &mut [u8],
) -> Result<()> {
    let addresses = input.addresses;
    let layout = addresses.layout;
    let Some(section) = layout.sections.get(position as usize) else {
        return Ok(());
    };
    let Some(&(offset, _)) = section.data.get(index as usize) else {
        return Ok(());
    };
    let start = section.addr.wrapping_add(offset);
    // The relocated sections are rebuilt here, since their own chunks may
    // not be written yet; relocation problems are reported by those.
    let quiet = Collect::new();
    let scratch_input = WriteInput {
        diagnostics: &quiet,
        ..*input
    };
    let mut scratch: Option<(SectionId, Vec<u8>)> = None;
    let patches = arch::thunk::patches_in(&layout.thunks, section.output, 0, u64::MAX);
    for placed in patches {
        let Some((id, site_offset)) = placed.patch else {
            continue;
        };
        if scratch.as_ref().is_none_or(|(current, _)| *current != id) {
            let size = addresses
                .refs
                .sections
                .locate(id)
                .and_then(|(file, index)| {
                    let object = addresses.refs.files.get(file)?.object.as_ref()?;
                    object.section(index).map(|s| s.header.sh_size)
                })
                .and_then(|size| usize::try_from(size).ok())
                .unwrap_or(0);
            let mut bytes = vec![0u8; size];
            relocate_input(&scratch_input, id, &mut bytes)?;
            scratch = Some((id, bytes));
        }
        let moved = scratch.as_ref().and_then(|(_, bytes)| {
            crate::arch::aarch64::read_insn(bytes, usize::try_from(site_offset).ok()?)
        });
        let words = moved.and_then(|moved| {
            crate::arch::aarch64::erratum_patch(placed.address, placed.target, moved).ok()
        });
        let at = placed
            .address
            .checked_sub(start)
            .and_then(|at| usize::try_from(at).ok());
        match (words, at) {
            (Some([first, second]), Some(at)) => {
                crate::arch::aarch64::write_insn(out, at, first);
                crate::arch::aarch64::write_insn(out, at.saturating_add(4), second);
            }
            _ => input.diagnostics.emit(Diagnostic::error(format!(
                "Cortex-A53 erratum patch at {:#x} is out of range of {:#x}",
                placed.address, placed.target
            ))),
        }
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn relocate_input<F: crate::elf::read::ElfFormat>(
    input: &WriteInput<'_, '_, '_, F>,
    id: SectionId,
    out: &mut [u8],
) -> Result<()> {
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
    let data = object.section_data(section)?;
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
    // RISC-V relocations depend on each other and on linker relaxation.
    if input.context.arch == Arch::RiscV64 {
        let section = super::arch::riscv::apply::SectionWrite {
            id,
            file: file_index,
            index: section_index,
            section,
            data,
            base,
        };
        return super::arch::riscv::apply::write_section(input, section, out);
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
    let Some(relocations) = relocations.map(|r| r.relocations) else {
        return Ok(());
    };
    let alloc = section.header.sh_flags & SHF_ALLOC != 0;
    let tombstone = if alloc {
        SectionTombstone::default()
    } else {
        input.tombstones.for_section(section.name)
    };
    let executable = input.context.mode.executable() || !input.context.mode.dynamic;
    let arch = input.context.arch;
    let order = file.position.raw();
    let tls = addresses.layout.tls.unwrap_or_default();
    let tp = tls.tp(arch);
    // PowerPC64: `.toc` entries this section takes the address of, whose
    // accesses keep going through the entry; found on first use.
    let pinned_toc: std::cell::OnceCell<Vec<(u32, u64)>> = std::cell::OnceCell::new();
    let mut skip = false;
    arch::for_each_relocation!(arch, relocations, data, |rel| {
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
        if let Some((from, to)) = prohibited_cross_reference(input, id, &target) {
            let name = cross_reference_name(refs, file_index, rel.symbol, &target);
            report(format!(
                "prohibited cross reference from {from} to `{name}' in {to}"
            ));
        }
        let flags = target
            .global
            .map_or(SymbolFlags::EMPTY, |id| refs.symbols.flags(id));
        let Ok(decision) = reloc::decide(
            &input.context,
            &rel,
            data,
            &target,
            flags,
            section.header.sh_flags,
        ) else {
            continue; // Reported by the scan.
        };
        let class = decision.class;
        skip = class.skip_next;
        if class.kind == Kind::None || (alloc && decision.problem.is_some()) {
            continue;
        }
        let place = base.wrapping_add(rel.offset);
        let owner = Addresses::<F>::owner(&target, file_index, rel.symbol);
        if !alloc
            && matches!(class.kind, Kind::Abs | Kind::DtpOff)
            && let Some(dead) = dead_target(refs, &target)
            && let Some(value) = tombstone.get(dead)
        {
            let value = crate::debug::tombstone::truncate(value, width_bytes(class.width));
            let _ = arch::write_value(out, rel.offset, class.width, value);
            continue;
        }
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
                // Both labels of a difference in a discarded section count
                // from zero, which keeps the difference (lld does the same).
                if let Some(delta) = add_delta(class.kind, rel.addend as u64) {
                    let _ = arch::add_value(out, rel.offset, class.width, delta);
                    continue;
                }
                let value = tombstone.get(DeadTarget::Discarded).unwrap_or(0);
                let value = crate::debug::tombstone::truncate(value, width_bytes(class.width));
                let _ = arch::write_value(out, rel.offset, class.width, value);
                continue;
            }
        };
        if alloc {
            if target.is_ifunc()
                && let Some(stub) = addresses.iplt_address(owner)
            {
                s = stub;
            }
            if flags.contains(SymbolFlags::NEEDS_PLT | PREEMPTIBLE)
                && class.kind == Kind::Pc
                && arch.is_branch(rel.r_type)
                && let Some(plt) = addresses.plt_address(owner)
            {
                s = plt;
            }
        }
        let sa = s.wrapping_add_signed(a);
        let slot_address = || -> Result<u64, ApplyError> {
            addresses
                .got_entry_address(owner, class.slot)
                .ok_or(ApplyError::BadInstruction)
        };
        let put =
            |out: &mut [u8], value: u64| arch::write_value(out, rel.offset, class.width, value);
        let page = crate::arch::aarch64::page;
        let page_delta = |target: u64| arch.page_delta(target, place, rel.r_type);
        let result = match class.kind {
            Kind::None => Ok(()),
            Kind::Abs => match decision.dynamic {
                Dynamic::Symbolic(_) => Ok(()),
                _ => put(out, sa),
            },
            Kind::Pc
                if matches!(target.def, super::refs::Def::Undefined { .. })
                    && sa == 0
                    && arch
                        .nop_undefined_branch(out, rel.offset, rel.r_type)
                        .unwrap_or(false) =>
            {
                Ok(())
            }
            Kind::Pc => match class.width {
                // A branch that cannot reach its target goes through the
                // range-extension thunk layout placed for this output
                // section (`elf::arch::thunk`).
                Width::Field(crate::arch::aarch64::Field::Branch26) => {
                    let mut sa = sa;
                    if !crate::arch::aarch64::branch_in_range(place, sa)
                        && let Some(output) = addresses
                            .layout
                            .section_shndx
                            .get(id.index())
                            .copied()
                            .and_then(|shndx| addresses.layout.output_of_shndx(shndx))
                        && let Some(thunk) = addresses.layout.thunk_for(output, sa)
                    {
                        sa = thunk;
                    }
                    put(out, sa.wrapping_sub(place))
                }
                // PowerPC64 calls: local entry points, stubs and TOC
                // restores.
                Width::Ppc(crate::arch::ppc64::Field::Rel24) => ppc64_branch(
                    out,
                    addresses,
                    id,
                    &rel,
                    PpcBranch {
                        place,
                        target: sa,
                        st_other: target.raw.map_or(0, |raw| raw.st_other),
                        via_stub: alloc
                            && ((target.is_ifunc() && addresses.iplt_address(owner).is_some())
                                || (flags.contains(SymbolFlags::NEEDS_PLT | PREEMPTIBLE)
                                    && arch.is_branch(rel.r_type)
                                    && addresses.plt_address(owner).is_some())),
                        owner,
                        width: class.width,
                    },
                ),
                _ => put(out, sa.wrapping_sub(place)),
            },
            Kind::Page => put(out, page_delta(sa)),
            Kind::Got => {
                slot_address().and_then(|g| put(out, g.wrapping_add_signed(a).wrapping_sub(place)))
            }
            Kind::GotPage => {
                slot_address().and_then(|g| put(out, page_delta(g.wrapping_add_signed(a))))
            }
            Kind::PageOff => put(out, sa),
            Kind::Add | Kind::Sub => {
                let delta = add_delta(class.kind, sa).unwrap_or_default();
                arch::add_value(out, rel.offset, class.width, delta)
            }
            Kind::Relax => arch.relax(out, rel.offset, rel.r_type, sa, place),
            Kind::GotAbs => slot_address().and_then(|g| put(out, g.wrapping_add_signed(a))),
            Kind::GotPageOff => slot_address().and_then(|g| {
                put(
                    out,
                    g.wrapping_add_signed(a)
                        .wrapping_sub(page(addresses.got_base())),
                )
            }),
            Kind::GotSlotRel => slot_address().and_then(|g| {
                put(
                    out,
                    g.wrapping_sub(addresses.got_base()).wrapping_add_signed(a),
                )
            }),
            // The relaxed sequence loads the thread pointer offset, so it
            // reads the TpOff entry the scan reserved (see reloc.rs), not the
            // slot the classification names for its unrelaxed form.
            Kind::GdToIe | Kind::DescToIe => addresses
                .got_entry_address(owner, GotKind::TpOff)
                .ok_or(ApplyError::BadInstruction)
                .and_then(|g| {
                    arch.relax_tls(
                        out,
                        rel.offset,
                        class.kind,
                        rel.r_type,
                        RelaxValues {
                            tpoff: sa.wrapping_sub(tp) as i64,
                            got: g,
                            got_pc: g.wrapping_add_signed(a).wrapping_sub(place) as i64,
                            place,
                            got_base: addresses.got_base(),
                        },
                    )
                }),
            Kind::RelaxGotPc => {
                arch.relax_got(out, rel.offset, class.kind, sa.wrapping_sub(place) as i64)
            }
            Kind::RelaxGotPcNoPic => arch.relax_got(out, rel.offset, class.kind, sa as i64),
            Kind::RelaxGotOff => arch.relax_got(
                out,
                rel.offset,
                class.kind,
                sa.wrapping_sub(addresses.got_base()) as i64,
            ),
            // PowerPC64: a load through a `.toc` entry becomes the
            // TOC-relative address of the symbol the entry holds.
            Kind::GotRel
                if alloc
                    && matches!(
                        class.width,
                        Width::Ppc(
                            crate::arch::ppc64::Field::HaToc | crate::arch::ppc64::Field::LoDsToc
                        )
                    ) =>
            {
                ppc64_toc_access(
                    out,
                    addresses,
                    file_index,
                    &rel,
                    relocations,
                    &pinned_toc,
                    input.context.mode.pic,
                    sa,
                    class.width,
                )
            }
            Kind::GotRel => put(out, sa.wrapping_sub(addresses.got_base())),
            Kind::GotBasePc => put(
                out,
                addresses
                    .got_base()
                    .wrapping_add_signed(a)
                    .wrapping_sub(place),
            ),
            Kind::Size => {
                let size = target.raw.map_or(0, |r| r.st_size);
                put(out, size.wrapping_add_signed(a))
            }
            Kind::Addend => put(out, rel.addend as u64),
            // An undefined (weak) TLS symbol has no thread pointer offset;
            // GNU ld and lld write the addend, as for an absolute value.
            Kind::TpOff if matches!(target.def, super::refs::Def::Undefined { .. }) => {
                put(out, a as u64)
            }
            Kind::TpOff => put(out, sa.wrapping_sub(tp)),
            Kind::DtpOff if matches!(target.def, super::refs::Def::Undefined { .. }) => {
                put(out, a as u64)
            }
            Kind::DtpOff => {
                // Where the dynamic thread vector is biased (PowerPC64),
                // relaxed local-dynamic code computes the biased block
                // start too, so the offset is the same either way.
                let dtv_offset = arch.dtv_offset();
                let value = if alloc && executable && dtv_offset == 0 {
                    sa.wrapping_sub(tp)
                } else {
                    sa.wrapping_sub(tls.start).wrapping_sub(dtv_offset)
                };
                put(out, value)
            }
            Kind::GdToLe | Kind::LdToLe | Kind::IeToLe | Kind::DescToLe | Kind::DescCallToLe => {
                // Local-dynamic code takes the start of the module's TLS
                // block as its base, so that is what the relaxed sequence
                // must compute.
                // An undefined (weak) variable has no offset: like lld,
                // relax to the addend (glibc's static `setlocale.o` reaches
                // `_nl_current_LC_*` this way, behind a `_used` check).
                let tpoff = if class.kind == Kind::LdToLe {
                    tls.start.wrapping_sub(tp) as i64
                } else if matches!(target.def, super::refs::Def::Undefined { .. }) {
                    a
                } else {
                    sa.wrapping_sub(tp) as i64
                };
                arch.relax_tls(
                    out,
                    rel.offset,
                    class.kind,
                    rel.r_type,
                    RelaxValues {
                        tpoff,
                        got: 0,
                        got_pc: 0,
                        place,
                        got_base: 0,
                    },
                )
            }
        };
        if let Err(error) = result {
            let type_name = arch.reloc_label(rel.r_type);
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
    });
    if alloc && arch == Arch::AArch64 && input.context.relax {
        // AArch64 ADRP relaxations: they look at pairs of relocations, so
        // they run over the relocated section rather than in the loop.
        let got_target = |symbol: u32| -> Option<u64> {
            let target = refs.target(file_index, symbol as usize)?;
            let flags = target
                .global
                .map_or(SymbolFlags::EMPTY, |id| refs.symbols.flags(id));
            if target.is_tls() || !reloc::classify_context(&input.context, &target, flags).relax_got
            {
                return None;
            }
            let (s, a) = addresses.symbol_address(&target, 0)?;
            Some(s.wrapping_add_signed(a))
        };
        // An instruction moved into an erratum patch is no longer half of a
        // pair.
        let patched: Vec<u64> = erratum_patches(input, id, base, out.len())
            .into_iter()
            .map(|(offset, _)| offset)
            .collect();
        if let Relocations::Rela(relas) = relocations {
            arch::aarch64::relax_adrp_pairs(out, relas.iter(), base, &patched, &got_target);
        }
    }
    Ok(())
}

/// The amount a label-difference relocation ([`Kind::Add`] or
/// [`Kind::Sub`]) adds to its field, for the value `value`.
fn add_delta(kind: Kind, value: u64) -> Option<u64> {
    match kind {
        Kind::Add => Some(value),
        Kind::Sub => Some(value.wrapping_neg()),
        _ => None,
    }
}

/// What [`ppc64_branch`] needs to know about a call besides the relocation.
struct PpcBranch {
    place: u64,
    /// `S + A`, or the stub's address.
    target: u64,
    st_other: u8,
    via_stub: bool,
    owner: Owner,
    width: Width,
}

/// Writes a PowerPC64 branch: the callee's local entry point, the thunk
/// the call goes through if it needs one, and the TOC restore after a call
/// through a stub. Out of line, so the other architectures' relocation loop
/// does not carry it.
#[inline(never)]
fn ppc64_branch<F: crate::elf::read::ElfFormat>(
    out: &mut [u8],
    addresses: &Addresses<'_, '_, F>,
    id: SectionId,
    rel: &crate::elf::read::Relocation,
    call: PpcBranch,
) -> std::result::Result<(), ApplyError> {
    let arch = super::arch::Arch::Ppc64;
    let slot = if call.via_stub {
        super::values::plt_slot_address(addresses.synth, addresses.layout, call.owner)
    } else {
        None
    };
    let branch = super::arch::Branch {
        r_type: rel.r_type,
        place: call.place,
        target: call.target,
        st_other: call.st_other,
        via_stub: call.via_stub,
        slot,
    };
    let mut sa = arch.branch_destination(branch);
    if let Some(destination) = arch.branch_thunk(branch)
        && let Some(output) = addresses
            .layout
            .section_shndx
            .get(id.index())
            .copied()
            .and_then(|shndx| addresses.layout.output_of_shndx(shndx))
        && let Some(thunk) = addresses.layout.thunk_for(output, destination)
    {
        sa = thunk;
    }
    arch.finish_call(out, rel.offset, branch)?;
    arch::write_value(out, rel.offset, call.width, sa.wrapping_sub(call.place))
}

/// Writes a PowerPC64 TOC-relative access, relaxing it to address the
/// symbol a `.toc` entry holds when it loads that entry. `pinned` holds the
/// `.toc` entries the section (whose relocations are `relas`) takes the
/// address of, found on first use.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn ppc64_toc_access<F: crate::elf::read::ElfFormat>(
    out: &mut [u8],
    addresses: &Addresses<'_, '_, F>,
    file_index: usize,
    rel: &crate::elf::read::Relocation,
    relocations: Relocations<'_, F>,
    pinned: &std::cell::OnceCell<Vec<(u32, u64)>>,
    pic: bool,
    sa: u64,
    width: Width,
) -> std::result::Result<(), ApplyError> {
    let pinned = pinned.get_or_init(|| {
        super::arch::ppc64::pinned_toc_entries(&addresses.refs, file_index, relocations)
    });
    let (sa, width) =
        match super::arch::ppc64::toc_indirection(addresses, file_index, rel, pinned, pic) {
            Some((address, field)) => (address, Width::Ppc(field)),
            None => (sa, width),
        };
    arch::write_value(
        out,
        rel.offset,
        width,
        sa.wrapping_sub(addresses.got_base()),
    )
}

/// The output section names of a reference from input section `from` to
/// `target` when a `NOCROSSREFS` list prohibits it, as GNU ld checks: both
/// output sections are in one list and differ, and for `NOCROSSREFS_TO`
/// the target is in the list's first section.
pub(crate) fn prohibited_cross_reference<F: crate::elf::read::ElfFormat>(
    input: &WriteInput<'_, '_, '_, F>,
    from: SectionId,
    target: &super::refs::Target,
) -> Option<(String, String)> {
    let addresses = input.addresses;
    let layout = addresses.layout;
    if layout.nocrossrefs.is_empty() {
        return None;
    }
    let name_of = |shndx: u32| -> Option<&[u8]> {
        let position = usize::try_from(shndx.checked_sub(1)?).ok()?;
        layout.sections.get(position).map(|s| s.name)
    };
    let shndx_of = |id: SectionId| layout.section_shndx.get(id.index()).copied();
    let from_name = name_of(shndx_of(from)?)?;
    let to_shndx = match target.def {
        super::refs::Def::Section { file, section, .. } => {
            shndx_of(addresses.refs.sections.id(file, section)?)?
        }
        super::refs::Def::Linker(id) => {
            u32::from(super::defined::linker_shndx(addresses, input.linker, id)?)
        }
        _ => return None,
    };
    let to_name = name_of(to_shndx)?;
    if from_name == to_name {
        return None;
    }
    let listed = |names: &[Vec<u8>], name: &[u8]| names.iter().any(|n| n.as_slice() == name);
    layout
        .nocrossrefs
        .iter()
        .any(|(first_only, names)| {
            let target_listed = if *first_only {
                names.first().is_some_and(|n| n.as_slice() == to_name)
            } else {
                listed(names, to_name)
            };
            target_listed && listed(names, from_name)
        })
        .then(|| {
            (
                String::from_utf8_lossy(from_name).into_owned(),
                String::from_utf8_lossy(to_name).into_owned(),
            )
        })
}

/// The name a cross-reference error uses for a symbol: GNU ld names a
/// section symbol after its input section, which has no symbol name.
pub(crate) fn cross_reference_name<F: crate::elf::read::ElfFormat>(
    refs: &Refs<'_, '_, F>,
    file: usize,
    symbol: u32,
    target: &super::refs::Target,
) -> String {
    if target.is_section_symbol()
        && let super::refs::Def::Section { section, .. } = target.def
        && let Some(name) = refs
            .files
            .get(file)
            .and_then(|f| f.object.as_ref())
            .and_then(|o| o.section(section))
            .map(|s| String::from_utf8_lossy(s.name).into_owned())
    {
        return name;
    }
    symbol_name(refs, file, symbol)
}

pub(crate) fn symbol_name<F: crate::elf::read::ElfFormat>(
    refs: &Refs<'_, '_, F>,
    file: usize,
    symbol: u32,
) -> String {
    refs.symbol_name(file, symbol)
        .unwrap_or_else(|| format!("symbol {symbol}"))
}

fn write_eh_frame<F: crate::elf::read::ElfFormat>(
    input: &WriteInput<'_, '_, '_, F>,
    eh: &EhSection<'_, F>,
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
            let Some(rel) = eh.reloc(index as usize) else {
                continue;
            };
            let Some(target) = refs.target(eh.file, rel.symbol as usize) else {
                continue;
            };
            let Ok(class) = input.context.arch.classify(
                rel.r_type,
                rel.addend,
                eh.data,
                rel.offset,
                super::arch::ClassifyContext::static_exec(false),
            ) else {
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
            // LoongArch assembles the advances of the call frame
            // instructions as label differences when the code may relax.
            let written = if let Some(delta) = add_delta(class.kind, sa) {
                arch::add_value(out, local, class.width, delta)
            } else {
                let value = match class.kind {
                    Kind::Abs => sa,
                    Kind::Pc => sa.wrapping_sub(place),
                    _ => continue,
                };
                arch::write_value(out, local, class.width, value)
            };
            if written.is_err() {
                input.diagnostics.emit(
                    Diagnostic::error("relocation in .eh_frame out of range".to_string())
                        .at(location(refs, eh.file, eh.index, rel.offset)),
                );
            }
        }
    }
    Ok(())
}
