//! Program headers and file offsets under a linker script.
//!
//! Without `PHDRS`, segments are made as BFD's
//! `_bfd_elf_map_sections_to_segments` makes them: allocated sections sorted
//! by load address go into one `PT_LOAD` until a section's VMA/LMA relation
//! changes, it would overlap or skip a page, a loaded section follows a
//! `NOBITS` one, a writable section follows read-only ones, code meets
//! non-code under `-z separate-code`, or the memory region changes. The ELF
//! and program headers join the first segment when the script left room for
//! them. `PT_PHDR`/`PT_INTERP`, `PT_DYNAMIC`, `PT_NOTE`, `PT_TLS`,
//! `PT_GNU_PROPERTY`, `PT_GNU_EH_FRAME`, `PT_GNU_STACK` and `PT_GNU_RELRO`
//! follow. With `PHDRS`, each output section goes into the segments its
//! `:phdr` list names (or those of the previous allocated section).
//!
//! File offsets then follow `assign_file_positions_for_load_sections`:
//! load segments in load address order, each starting at an offset
//! congruent to its address modulo the page size (for demand-paged
//! output), with gaps inside a segment kept as file space.

#![deny(clippy::arithmetic_side_effects)]

use crate::args::{ExecStack, LinkOptions, MagicMode, SeparateCode};
use crate::elf::layout::{EHDR_SIZE, OutSection, PHDR_SIZE, Segment, Trailer, align_up};
use crate::elf::read::consts::{
    PF_R, PF_W, PF_X, PT_DYNAMIC, PT_GNU_EH_FRAME, PT_GNU_PROPERTY, PT_GNU_RELRO, PT_GNU_STACK,
    PT_INTERP, PT_LOAD, PT_NOTE, PT_PHDR, PT_TLS, SHF_ALLOC, SHF_EXECINSTR, SHF_TLS, SHF_WRITE,
    SHT_NOBITS, SHT_NOTE,
};
use crate::error::{Error, Result};

/// A `PHDRS` entry with its expressions evaluated.
#[derive(Clone, Debug)]
pub struct PhdrSpec {
    /// Its name.
    pub name: Vec<u8>,
    /// `p_type`.
    pub p_type: u32,
    /// `FILEHDR`.
    pub filehdr: bool,
    /// `PHDRS`.
    pub phdrs: bool,
    /// `AT(address)`.
    pub at: Option<u64>,
    /// `FLAGS(flags)`.
    pub flags: Option<u32>,
}

/// What segment construction needs to know about the link.
pub struct SegmentInput<'s> {
    /// Options.
    pub options: &'s LinkOptions,
    /// `PHDRS`, evaluated, when the script has them.
    pub phdrs: Option<&'s [PhdrSpec]>,
    /// For each section (by position), its `:phdr` names after
    /// inheritance, for `PHDRS`.
    pub section_phdrs: &'s [Vec<Vec<u8>>],
    /// For each section, its memory region (for the segment split).
    pub regions: &'s [Option<usize>],
    /// Whether the script used `SIZEOF_HEADERS`.
    pub load_phdrs: bool,
    /// The program header table size the addresses were computed with.
    pub reserved_headers: u64,
    /// The RELRO range from `DATA_SEGMENT_RELRO_END`.
    pub relro: Option<(u64, u64)>,
    /// Whether an input asked for an executable stack.
    pub exec_stack: bool,
    /// Whether some input has a `.note.GNU-stack` section (GNU ld emits
    /// `PT_GNU_STACK` only then, or with `-z [no]execstack`).
    pub stack_note: bool,
    /// Position of the `.interp` section, if any.
    pub interp: Option<usize>,
    /// Position of `.eh_frame_hdr`'s section, if any.
    pub eh_frame_hdr: Option<usize>,
}

/// The program headers and where the file's contents go.
#[derive(Debug)]
pub struct SegmentResult {
    /// Program headers, in table order.
    pub segments: Vec<Segment>,
    /// Offset of the program header table.
    pub phoff: u64,
    /// End of the section contents in the file.
    pub file_end: u64,
}

/// A segment being built.
#[derive(Clone, Debug, Default)]
struct Map {
    p_type: u32,
    flags: Option<u32>,
    paddr: Option<u64>,
    vaddr_offset: u64,
    filehdr: bool,
    phdrs: bool,
    sections: Vec<usize>,
    align: Option<u64>,
    size: Option<u64>,
}

fn is_tbss(s: &OutSection<'_>) -> bool {
    s.flags & SHF_TLS != 0 && s.sh_type == SHT_NOBITS
}

fn is_load(s: &OutSection<'_>) -> bool {
    s.flags & SHF_ALLOC != 0 && s.sh_type != SHT_NOBITS
}

/// GNU's `elf_sort_sections`.
fn sort_sections(sections: &[OutSection<'_>], list: &mut [usize]) {
    list.sort_by(|&a, &b| {
        let (Some(sa), Some(sb)) = (sections.get(a), sections.get(b)) else {
            return core::cmp::Ordering::Equal;
        };
        let toend = |s: &OutSection<'_>| !is_load(s) && s.flags & SHF_TLS == 0 && s.size != 0;
        let size = |s: &OutSection<'_>| if is_load(s) { s.size } else { 0 };
        sa.lma
            .cmp(&sb.lma)
            .then(sa.addr.cmp(&sb.addr))
            .then(toend(sa).cmp(&toend(sb)))
            .then(size(sa).cmp(&size(sb)))
            .then(a.cmp(&b))
    });
}

fn too_large() -> Error {
    Error::Limit("output larger than the address space".into())
}

/// GNU's test for starting a new `PT_LOAD` at `hdr` after `prev`.
#[allow(clippy::too_many_arguments, clippy::fn_params_excessive_bools)]
fn needs_new_segment(
    prev: &OutSection<'_>,
    last_size: u64,
    hdr: &OutSection<'_>,
    paged: bool,
    separate: bool,
    executable: bool,
    writable: bool,
    max_page: u64,
) -> bool {
    let page_mask = !max_page.wrapping_sub(1);
    let prev_end = prev.lma.wrapping_add(last_size);
    if prev.lma.wrapping_sub(prev.addr) != hdr.lma.wrapping_sub(hdr.addr) {
        return true;
    }
    if hdr.lma < prev_end || prev_end < prev.lma {
        return true;
    }
    if paged && (prev_end.wrapping_sub(1) & page_mask) == (hdr.lma & page_mask) {
        return false;
    }
    if align_up(prev_end, max_page)
        .map(|v| v.wrapping_add(max_page))
        .is_ok_and(|v| v > prev.lma && v <= hdr.lma)
    {
        return true;
    }
    if !is_load(prev) && prev.flags & SHF_TLS == 0 && (is_load(hdr) || hdr.flags & SHF_TLS != 0) {
        return true;
    }
    if !paged {
        return false;
    }
    if separate && executable != (hdr.flags & SHF_EXECINSTR != 0) {
        return true;
    }
    !writable && hdr.flags & SHF_WRITE != 0
}

/// Builds the segment map without `PHDRS`.
#[allow(clippy::too_many_lines)]
fn default_maps(input: &SegmentInput<'_>, sections: &[OutSection<'_>], phdr_size: u64) -> Vec<Map> {
    let options = input.options;
    let paged = options.magic == MagicMode::Normal;
    let separate =
        paged && options.separate_code.unwrap_or(SeparateCode::Code) != SeparateCode::None;
    let max_page = options
        .max_page_size
        .filter(|p| p.is_power_of_two())
        .unwrap_or(crate::elf::layout::DEFAULT_PAGE);
    let mut alloc: Vec<usize> = sections
        .iter()
        .enumerate()
        .filter(|(_, s)| s.trailer == Trailer::None && s.flags & SHF_ALLOC != 0)
        .map(|(i, _)| i)
        .collect();
    sort_sections(sections, &mut alloc);
    let page_mask = !max_page.wrapping_sub(1);
    let mut maps = Vec::new();
    let mut phdr_in_segment = input.load_phdrs;
    if let Some(&first) = alloc.first()
        && let Some(s) = sections.get(first)
        && s.lma & max_page.wrapping_sub(1) >= phdr_size & max_page.wrapping_sub(1)
    {
        phdr_in_segment = true;
    }
    if let Some(interp) = input.interp
        && sections
            .get(interp)
            .is_some_and(|s| is_load(s) && s.size != 0)
    {
        maps.push(Map {
            p_type: PT_PHDR,
            flags: Some(PF_R),
            phdrs: true,
            ..Map::default()
        });
        maps.push(Map {
            p_type: PT_INTERP,
            sections: vec![interp],
            ..Map::default()
        });
        phdr_in_segment = true;
    }
    if !paged {
        phdr_in_segment = false;
    }
    if phdr_in_segment && let Some(first) = alloc.first().and_then(|&i| sections.get(i)) {
        let mut phdr_lma = first.lma.wrapping_sub(phdr_size) & page_mask;
        let mut separate_phdr = false;
        if separate && first.flags & SHF_EXECINSTR != 0 {
            separate_phdr = true;
            if phdr_lma.wrapping_add(phdr_size).wrapping_sub(1) & page_mask == first.lma & page_mask
            {
                if phdr_lma >= max_page {
                    phdr_lma = phdr_lma.wrapping_sub(max_page);
                } else {
                    separate_phdr = false;
                }
            }
        }
        if first.lma < phdr_lma || first.lma < phdr_size {
            phdr_in_segment = false;
        } else if separate_phdr {
            maps.push(Map {
                p_type: PT_LOAD,
                paddr: Some(phdr_lma),
                vaddr_offset: first.addr.wrapping_sub(phdr_size) & page_mask,
                filehdr: true,
                phdrs: true,
                ..Map::default()
            });
            phdr_in_segment = false;
        }
    }

    let mut start = 0usize;
    let mut last: Option<usize> = None;
    let mut last_size = 0u64;
    let mut writable = false;
    let mut executable = false;
    let mut first_map = true;
    for (i, &index) in alloc.iter().enumerate() {
        let Some(hdr) = sections.get(index) else {
            continue;
        };
        let new_segment = match last.and_then(|l| sections.get(l)) {
            None => false,
            Some(prev) => {
                let split = needs_new_segment(
                    prev, last_size, hdr, paged, separate, executable, writable, max_page,
                );
                split
                    || input.regions.get(index).copied().flatten()
                        != input.regions.get(last.unwrap_or(0)).copied().flatten()
            }
        };
        if !new_segment {
            writable |= hdr.flags & SHF_WRITE != 0;
            executable |= hdr.flags & SHF_EXECINSTR != 0;
            last = Some(index);
            last_size = if is_tbss(hdr) { 0 } else { hdr.size };
            continue;
        }
        maps.push(Map {
            p_type: PT_LOAD,
            sections: alloc.get(start..i).unwrap_or_default().to_vec(),
            filehdr: first_map && phdr_in_segment,
            phdrs: first_map && phdr_in_segment,
            ..Map::default()
        });
        first_map = false;
        writable = hdr.flags & SHF_WRITE != 0;
        executable = hdr.flags & SHF_EXECINSTR != 0;
        last = Some(index);
        last_size = if is_tbss(hdr) { 0 } else { hdr.size };
        start = i;
        phdr_in_segment = false;
    }
    if let Some(l) = last
        && (alloc.len().saturating_sub(start) != 1 || !sections.get(l).is_some_and(is_tbss))
    {
        maps.push(Map {
            p_type: PT_LOAD,
            sections: alloc.get(start..).unwrap_or_default().to_vec(),
            filehdr: first_map && phdr_in_segment,
            phdrs: first_map && phdr_in_segment,
            ..Map::default()
        });
    }
    if let Some(dynamic) = sections
        .iter()
        .position(|s| s.name == b".dynamic" && s.trailer == Trailer::None && is_load(s))
    {
        maps.push(Map {
            p_type: PT_DYNAMIC,
            sections: vec![dynamic],
            ..Map::default()
        });
    }
    // Notes: runs of adjacent loaded notes with the same alignment.
    let mut i = 0usize;
    let mut tls: Vec<usize> = Vec::new();
    while let Some(s) = sections.get(i) {
        if s.trailer != Trailer::None {
            i = i.saturating_add(1);
            continue;
        }
        if is_load(s) && s.sh_type == SHT_NOTE {
            let mut run = vec![i];
            let mut j = i;
            while let (Some(a), Some(b)) = (sections.get(j), sections.get(j.saturating_add(1))) {
                let adjacent = align_up(a.lma.wrapping_add(a.size), b.align.max(1))
                    .is_ok_and(|end| end == b.lma);
                if b.trailer == Trailer::None
                    && b.align == s.align
                    && is_load(b)
                    && b.sh_type == SHT_NOTE
                    && adjacent
                {
                    j = j.saturating_add(1);
                    run.push(j);
                } else {
                    break;
                }
            }
            maps.push(Map {
                p_type: PT_NOTE,
                sections: run,
                ..Map::default()
            });
            if let Some(s) = sections.get(j)
                && s.flags & SHF_TLS != 0
            {
                tls.push(j);
            }
            i = j.saturating_add(1);
            continue;
        }
        if s.flags & SHF_TLS != 0 && s.flags & SHF_ALLOC != 0 {
            tls.push(i);
        }
        i = i.saturating_add(1);
    }
    if !tls.is_empty() {
        maps.push(Map {
            p_type: PT_TLS,
            flags: Some(PF_R),
            sections: tls,
            ..Map::default()
        });
    }
    if let Some(property) = sections
        .iter()
        .position(|s| s.name == b".note.gnu.property" && s.trailer == Trailer::None && s.size != 0)
    {
        maps.push(Map {
            p_type: PT_GNU_PROPERTY,
            flags: Some(PF_R),
            sections: vec![property],
            ..Map::default()
        });
    }
    if let Some(hdr) = input.eh_frame_hdr
        && sections.get(hdr).is_some_and(is_load)
    {
        maps.push(Map {
            p_type: PT_GNU_EH_FRAME,
            sections: vec![hdr],
            ..Map::default()
        });
    }
    if options.gnu_stack
        && (input.stack_note
            || options.exec_stack != ExecStack::FromInputs
            || options.stack_size.is_some_and(|s| s > 0))
    {
        let exec = match options.exec_stack {
            ExecStack::Executable => true,
            ExecStack::NonExecutable => false,
            ExecStack::FromInputs => input.exec_stack,
        };
        maps.push(Map {
            p_type: PT_GNU_STACK,
            flags: Some(PF_R | PF_W | if exec { PF_X } else { 0 }),
            align: Some(16),
            size: Some(options.stack_size.unwrap_or(0)),
            ..Map::default()
        });
    }
    if options.relro
        && let Some((relro_start, relro_end)) = input.relro
    {
        let found = maps.iter().any(|m| {
            m.p_type == PT_LOAD
                && m.sections
                    .first()
                    .and_then(|&f| sections.get(f))
                    .is_some_and(|f| f.addr >= relro_start && f.addr < relro_end)
                && m.sections
                    .iter()
                    .filter_map(|&s| sections.get(s))
                    .any(|s| s.size > 0 && is_load(s))
        });
        if found {
            maps.push(Map {
                p_type: PT_GNU_RELRO,
                ..Map::default()
            });
        }
    }
    maps
}

/// Builds the segment map from `PHDRS`.
fn user_maps(
    input: &SegmentInput<'_>,
    specs: &[PhdrSpec],
    sections: &[OutSection<'_>],
) -> Result<Vec<Map>> {
    let mut maps = Vec::with_capacity(specs.len());
    for spec in specs {
        let mut list = Vec::new();
        for (position, section) in sections.iter().enumerate() {
            if section.trailer != Trailer::None {
                continue;
            }
            let names = input
                .section_phdrs
                .get(position)
                .map_or(&[][..], Vec::as_slice);
            if names.contains(&spec.name)
                && (section.flags & SHF_ALLOC != 0 || spec.p_type != PT_LOAD)
            {
                list.push(position);
            }
        }
        maps.push(Map {
            p_type: spec.p_type,
            flags: spec.flags,
            paddr: spec.at,
            filehdr: spec.filehdr,
            phdrs: spec.phdrs,
            sections: list,
            ..Map::default()
        });
    }
    Ok(maps)
}

/// Builds the program headers and assigns file offsets to `sections`.
///
/// # Errors
///
/// [`Error::Limit`] when offsets overflow; a link error when the headers do
/// not fit in front of the first segment.
#[allow(clippy::too_many_lines)]
pub fn assign(input: &SegmentInput<'_>, sections: &mut [OutSection<'_>]) -> Result<SegmentResult> {
    let options = input.options;
    let paged = options.magic == MagicMode::Normal;
    let estimate = input.reserved_headers.saturating_sub(EHDR_SIZE);
    let mut maps = match input.phdrs {
        Some(specs) => user_maps(input, specs, sections)?,
        None => default_maps(input, sections, input.reserved_headers),
    };
    // The table holds at least as many entries as the layout assumed.
    let count = u64::try_from(maps.len()).map_err(|_| too_large())?;
    let table = PHDR_SIZE.saturating_mul(count).max(estimate);
    let max_page = if paged {
        options
            .max_page_size
            .filter(|p| p.is_power_of_two())
            .unwrap_or(crate::elf::layout::DEFAULT_PAGE)
    } else {
        1
    };
    let max_page_set = options.max_page_size.is_some();
    let min_page = crate::elf::layout::DEFAULT_PAGE;
    // Sections of each map in address order.
    for map in &mut maps {
        if map.sections.len() > 1 {
            sort_sections(sections, &mut map.sections);
        }
    }
    let first_lma = |m: &Map, sections: &[OutSection<'_>]| {
        m.sections
            .first()
            .and_then(|&s| sections.get(s))
            .map_or(m.paddr.unwrap_or(0), |s| s.lma)
    };
    let mut order: Vec<usize> = (0..maps.len()).collect();
    order.sort_by(|&a, &b| {
        let (Some(ma), Some(mb)) = (maps.get(a), maps.get(b)) else {
            return core::cmp::Ordering::Equal;
        };
        ma.p_type
            .cmp(&mb.p_type)
            .then(mb.filehdr.cmp(&ma.filehdr))
            .then_with(|| {
                if ma.p_type == PT_LOAD {
                    let pa = ma.paddr.unwrap_or_else(|| first_lma(ma, sections));
                    let pb = mb.paddr.unwrap_or_else(|| first_lma(mb, sections));
                    pa.cmp(&pb)
                } else {
                    core::cmp::Ordering::Equal
                }
            })
            .then(a.cmp(&b))
    });

    let mut segments = vec![Segment::default(); maps.len()];
    let mut off = EHDR_SIZE;
    let phdr_load = order
        .iter()
        .take_while(|&&i| maps.get(i).is_some_and(|m| m.p_type == PT_LOAD))
        .find(|&&i| maps.get(i).is_some_and(|m| m.phdrs))
        .copied();
    if phdr_load.is_none() {
        off = off.checked_add(table).ok_or_else(too_large)?;
    }
    let mut phoff = EHDR_SIZE;
    let mut placed = vec![false; sections.len()];
    for &m_index in &order {
        let Some(map) = maps.get(m_index) else {
            continue;
        };
        let mut p = Segment {
            p_type: map.p_type,
            flags: map.flags.unwrap_or(0),
            ..Segment::default()
        };
        let first = map.sections.first().and_then(|&s| sections.get(s));
        p.vaddr = match first {
            Some(s) => s.addr.wrapping_add(map.vaddr_offset),
            None => map.vaddr_offset,
        };
        let mut paddr = match (map.paddr, first) {
            (Some(at), _) => at,
            (None, Some(s)) => s.lma.wrapping_add(map.vaddr_offset),
            (None, None) => 0,
        };
        let mut align_pagesize = 0u64;
        if map.p_type == PT_LOAD && paged {
            if !max_page_set {
                align_pagesize = min_page;
            }
            p.align = max_page;
        } else if let Some(align) = map.align {
            p.align = align;
        } else if map.sections.is_empty() {
            p.align = 8;
        }
        if Some(m_index) == phdr_load {
            off = off.checked_add(table).ok_or_else(too_large)?;
        }
        let mut no_contents = false;
        let mut off_adjust = 0u64;
        if map.p_type == PT_LOAD && !map.sections.is_empty() {
            let section_align = map
                .sections
                .iter()
                .filter_map(|&s| sections.get(s))
                .map(|s| s.align.max(1))
                .max()
                .unwrap_or(1);
            if section_align > min_page {
                align_pagesize = 0;
            }
            let align = if section_align < max_page {
                max_page
            } else {
                if paged {
                    p.align = section_align;
                }
                section_align
            };
            no_contents = !map
                .sections
                .iter()
                .filter_map(|&s| sections.get(s))
                .any(|s| s.sh_type != SHT_NOBITS);
            off_adjust = p
                .vaddr
                .wrapping_sub(off)
                .checked_rem(align.max(1))
                .unwrap_or(0);
            off = off.checked_add(off_adjust).ok_or_else(too_large)?;
            if !no_contents {
                off_adjust = 0;
            }
        }
        if map.filehdr {
            if map.flags.is_none() {
                p.flags |= PF_R;
            }
            p.filesz = EHDR_SIZE;
            p.memsz = EHDR_SIZE;
            if map.p_type == PT_LOAD && first.is_some() {
                if p.vaddr < off || (map.paddr.is_none() && paddr < off) {
                    return Err(Error::Option(format!(
                        "{}: not enough room for program headers, try linking with -N",
                        options.output_path().display()
                    )));
                }
                p.vaddr = p.vaddr.wrapping_sub(off);
                if map.paddr.is_none() {
                    paddr = paddr.wrapping_sub(off);
                }
            }
        }
        if map.phdrs {
            if map.flags.is_none() {
                p.flags |= PF_R;
            }
            p.filesz = p.filesz.saturating_add(table);
            p.memsz = p.memsz.saturating_add(table);
            if !map.filehdr {
                if map.p_type == PT_LOAD {
                    p.offset = off.wrapping_sub(table);
                    phoff = p.offset;
                    if first.is_some() {
                        p.vaddr = p.vaddr.wrapping_sub(off.wrapping_sub(p.offset));
                        if map.paddr.is_none() {
                            paddr = paddr.wrapping_sub(off.wrapping_sub(p.offset));
                        }
                    }
                } else if let Some(load) = phdr_load.and_then(|l| segments.get(l)) {
                    let base = if maps.get(phdr_load.unwrap_or(0)).is_some_and(|m| m.filehdr) {
                        EHDR_SIZE
                    } else {
                        0
                    };
                    p.vaddr = load.vaddr.wrapping_add(base);
                    if map.paddr.is_none() {
                        paddr = load.paddr.unwrap_or(load.vaddr).wrapping_add(base);
                    }
                    p.offset = load.offset.wrapping_add(base);
                } else {
                    p.offset = EHDR_SIZE;
                }
            }
        }
        if map.p_type == PT_LOAD {
            if !map.filehdr && !map.phdrs {
                p.offset = off;
                if no_contents {
                    let align = max_page.max(p.align).max(1);
                    p.offset = off
                        .wrapping_add(align)
                        .wrapping_sub(1)
                        .checked_rem(align)
                        .unwrap_or(0);
                    p.offset = p.offset.wrapping_add(1);
                }
            } else {
                let adjust = off.wrapping_sub(p.offset.wrapping_add(p.filesz));
                if !no_contents {
                    p.filesz = p.filesz.wrapping_add(adjust);
                }
                p.memsz = p.memsz.wrapping_add(adjust);
            }
        }
        if align_pagesize != 0 {
            p.align = align_pagesize;
        }
        for (k, &s_index) in map.sections.iter().enumerate() {
            let Some(section) = sections.get_mut(s_index) else {
                continue;
            };
            let tls_segment = map.p_type == PT_TLS;
            if (map.p_type == PT_LOAD || tls_segment)
                && (section.sh_type != SHT_NOBITS
                    || (section.flags & SHF_ALLOC != 0
                        && (section.flags & SHF_TLS == 0 || tls_segment)))
            {
                let p_end = paddr.wrapping_add(p.memsz);
                let mut adjust = section.lma.wrapping_sub(p_end);
                if adjust != 0 && (section.lma < p_end || p_end < paddr) {
                    adjust = 0;
                    section.lma = p_end;
                }
                p.memsz = p.memsz.wrapping_add(adjust);
                if map.p_type == PT_LOAD {
                    if section.sh_type != SHT_NOBITS {
                        off_adjust = 0;
                        if p.filesz.wrapping_add(adjust) < p.memsz {
                            adjust = p.memsz.wrapping_sub(p.filesz);
                        }
                    }
                    if section.sh_type != SHT_NOBITS || k == 0 {
                        off = off.checked_add(adjust).ok_or_else(too_large)?;
                        if section.sh_type == SHT_NOBITS {
                            off_adjust = off_adjust.wrapping_add(adjust);
                        }
                    }
                }
                if section.sh_type != SHT_NOBITS {
                    p.filesz = p.filesz.wrapping_add(adjust);
                }
            }
            if section.sh_type == SHT_NOBITS
                && section.flags & SHF_TLS != 0
                && !placed.get(s_index).copied().unwrap_or(false)
            {
                let align = section.align.max(1);
                section.offset = off.wrapping_add(
                    section
                        .addr
                        .wrapping_sub(off)
                        .checked_rem(align)
                        .unwrap_or(0),
                );
                if let Some(slot) = placed.get_mut(s_index) {
                    *slot = true;
                }
            } else if map.p_type == PT_LOAD {
                section.offset = off;
                if let Some(slot) = placed.get_mut(s_index) {
                    *slot = true;
                }
                if section.sh_type != SHT_NOBITS {
                    off = off.checked_add(section.size).ok_or_else(too_large)?;
                }
            }
            if section.sh_type != SHT_NOBITS {
                p.filesz = p.filesz.wrapping_add(section.size);
                if section.flags & SHF_ALLOC != 0 {
                    p.memsz = p.memsz.wrapping_add(section.size);
                }
            } else if section.flags & SHF_ALLOC != 0
                && (tls_segment || section.flags & SHF_TLS == 0)
            {
                p.memsz = p.memsz.wrapping_add(section.size);
            }
            if section.align > p.align && (map.p_type != PT_LOAD || !paged) && map.align.is_none() {
                p.align = section.align;
            }
            if map.flags.is_none() {
                p.flags |= PF_R;
                if section.flags & SHF_EXECINSTR != 0 {
                    p.flags |= PF_X;
                }
                if section.flags & SHF_WRITE != 0 {
                    p.flags |= PF_W;
                }
            }
        }
        off = off.wrapping_sub(off_adjust);
        p.paddr = (paddr != p.vaddr).then_some(paddr);
        if let Some(slot) = segments.get_mut(m_index) {
            *slot = p;
        }
    }

    // Sections outside load segments, in section order.
    for (index, section) in sections.iter_mut().enumerate() {
        if placed.get(index).copied().unwrap_or(false) || section.trailer != Trailer::None {
            continue;
        }
        if section.flags & SHF_ALLOC != 0 {
            let align = if paged && section.size != 0 {
                max_page
            } else {
                section.align.max(1)
            };
            off = off
                .checked_add(
                    section
                        .addr
                        .wrapping_sub(off)
                        .checked_rem(align.max(1))
                        .unwrap_or(0),
                )
                .ok_or_else(too_large)?;
        } else {
            off = align_up(off, section.align.max(1))?;
        }
        section.offset = off;
        if section.sh_type != SHT_NOBITS {
            off = off.checked_add(section.size).ok_or_else(too_large)?;
        }
    }

    // Non-load segments take their offsets from their sections.
    for (m_index, map) in maps.iter().enumerate() {
        let Some(p) = segments.get_mut(m_index) else {
            continue;
        };
        match map.p_type {
            PT_GNU_RELRO => {
                if let Some((start, end)) = input.relro {
                    let mut done = false;
                    for (l_index, load) in maps.iter().enumerate() {
                        if load.p_type != PT_LOAD {
                            continue;
                        }
                        let (Some(&first), Some(&last)) =
                            (load.sections.first(), load.sections.last())
                        else {
                            continue;
                        };
                        let (Some(fs), Some(ls)) = (sections.get(first), sections.get(last)) else {
                            continue;
                        };
                        let last_end = ls.addr.wrapping_add(if is_tbss(ls) { 0 } else { ls.size });
                        if !(last_end > start && fs.addr < end) {
                            continue;
                        }
                        if let Some(s) = load
                            .sections
                            .iter()
                            .filter_map(|&i| sections.get(i))
                            .find(|s| s.addr >= start && s.addr < end && s.size != 0)
                        {
                            p.vaddr = s.addr;
                            p.paddr = (s.lma != s.addr).then_some(s.lma);
                            p.offset = s.offset;
                            p.memsz = end.wrapping_sub(p.vaddr);
                            p.filesz = p.memsz;
                            if l_index != m_index {
                                // Trim to the loaded contents.
                            }
                            p.align = 1;
                            p.flags = PF_R;
                            done = true;
                        }
                        break;
                    }
                    if !done {
                        *p = Segment::default();
                    }
                }
            }
            PT_GNU_STACK => {
                p.memsz = map.size.unwrap_or(0);
            }
            PT_LOAD => {}
            _ if !map.sections.is_empty() => {
                if map.p_type == PT_PHDR {
                    continue;
                }
                let Some(first) = map.sections.first().and_then(|&s| sections.get(s)) else {
                    continue;
                };
                p.offset = first.offset;
                p.filesz = 0;
                for &s_index in map.sections.iter().rev() {
                    if let Some(s) = sections.get(s_index)
                        && s.sh_type != SHT_NOBITS
                    {
                        p.filesz = s.offset.wrapping_sub(p.offset).wrapping_add(s.size);
                        if map.p_type == PT_NOTE && s.flags & SHF_ALLOC != 0 {
                            p.memsz = p.filesz;
                        }
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    // GNU trims PT_GNU_RELRO's file size to its load segment's contents.
    let loads: Vec<Segment> = segments
        .iter()
        .filter(|s| s.p_type == PT_LOAD)
        .copied()
        .collect();
    for p in &mut segments {
        if p.p_type == PT_GNU_RELRO
            && let Some(load) = loads
                .iter()
                .find(|l| l.vaddr <= p.vaddr && p.vaddr < l.vaddr.wrapping_add(l.memsz.max(1)))
        {
            let limit = load.vaddr.wrapping_add(load.filesz).wrapping_sub(p.vaddr);
            if p.filesz > limit {
                p.filesz = limit;
            }
        }
    }
    let file_end = sections
        .iter()
        .filter(|s| s.sh_type != SHT_NOBITS && s.trailer == Trailer::None)
        .map(|s| s.offset.saturating_add(s.size))
        .max()
        .unwrap_or(0)
        .max(EHDR_SIZE.saturating_add(table))
        .max(off);
    Ok(SegmentResult {
        segments,
        phoff,
        file_end,
    })
}
