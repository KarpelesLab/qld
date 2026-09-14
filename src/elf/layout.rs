//! Layout (pipeline stage 10): output section contents, sizes, addresses,
//! file offsets and program headers.
//!
//! 1. **Members.** Each output section's live input sections are ordered by
//!    the input description that matched them (then `SORT_BY_NAME` or
//!    `SORT_BY_INIT_PRIORITY` where the rule asks, then input order). A merge
//!    group takes the place of its first input section; synthetic content
//!    goes first, except the common block, the `.comment` string and the
//!    `.eh_frame` terminator, which go last.
//! 2. **Sizes**, per output section in parallel.
//! 3. **Segments.** Allocated sections are split into `PT_LOAD`s by
//!    permissions (read-only, executable, writable; `-z separate-code` keeps
//!    code apart from read-only data). The ELF and program headers share the
//!    first, read-only, segment. `PT_TLS`, `PT_NOTE`, `PT_GNU_PROPERTY`,
//!    `PT_GNU_EH_FRAME` and `PT_GNU_STACK` follow.
//! 4. **Addresses.** From the base address (0x400000), each new `PT_LOAD`
//!    starts on a page boundary; file offsets equal address minus base, so
//!    they are congruent modulo the page size. `.tbss` takes no address
//!    space. Non-allocated sections follow in the file, then the section
//!    header table.
//!
//! Empty output sections are dropped from the output but keep an address
//! (the location counter where they would have been), which linker-defined
//! symbols such as `__init_array_start` use.

#![deny(clippy::arithmetic_side_effects)]

use rayon::prelude::*;

use crate::args::{ExecStack, LinkOptions, SeparateCode};
use crate::elf::read::consts::{
    PF_R, PF_W, PF_X, PT_GNU_EH_FRAME, PT_GNU_PROPERTY, PT_GNU_STACK, PT_LOAD, PT_NOTE, PT_TLS,
    SHF_ALLOC, SHF_EXECINSTR, SHF_TLS, SHF_WRITE, SHT_NOBITS, SHT_NOTE, SHT_PROGBITS, SHT_STRTAB,
    SHT_SYMTAB,
};
use crate::error::{Error, Result};
use crate::ids::SectionId;

use super::ehframe::EhFrames;
use super::inputs::ElfInput;
use super::merge::Merged;
use super::object::SectionKind;
use super::place::Placement;
use super::rules::{RuleSet, SortMode, Synthetic, priority};
use super::sections::{NONE, Sections};
use super::synth::Synth;

/// Default base address of an x86-64 executable.
pub const DEFAULT_BASE: u64 = 0x40_0000;
/// Default maximum page size on x86-64.
pub const DEFAULT_PAGE: u64 = 0x1000;
/// Size of the ELF header.
pub const EHDR_SIZE: u64 = 64;
/// Size of one program header.
pub const PHDR_SIZE: u64 = 56;
/// Size of one section header.
pub const SHDR_SIZE: u64 = 64;
/// [`Layout::section_shndx`] value of a live input section whose output
/// section was dropped because it is empty: it has an address but no
/// section header.
pub const EMPTY_SHNDX: u32 = u32::MAX;

/// Something placed in an output section.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Member {
    /// An input section.
    Input(SectionId),
    /// A merge group.
    Merge(u32),
    /// Synthetic content.
    Synthetic(Synthetic),
}

/// A member with its position.
#[derive(Clone, Copy, Debug)]
pub struct Placed {
    /// What it is.
    pub member: Member,
    /// Offset in the output section.
    pub offset: u64,
    /// Size.
    pub size: u64,
}

/// What a trailing, linker-generated, non-allocated section holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trailer {
    /// Not a trailer: a regular output section.
    None,
    /// `.symtab`.
    Symtab,
    /// `.strtab`.
    Strtab,
    /// `.shstrtab`.
    Shstrtab,
}

/// An output section after layout.
#[derive(Clone, Debug)]
pub struct OutSection<'a> {
    /// Name.
    pub name: &'a [u8],
    /// Index in [`Placement::outputs`], or [`NONE`] for trailers.
    pub output: u32,
    /// Trailer kind.
    pub trailer: Trailer,
    /// Type.
    pub sh_type: u32,
    /// Flags.
    pub flags: u64,
    /// Address (0 if not allocated).
    pub addr: u64,
    /// File offset.
    pub offset: u64,
    /// Size.
    pub size: u64,
    /// Alignment.
    pub align: u64,
    /// Entry size.
    pub entsize: u64,
    /// `sh_link`.
    pub link: u32,
    /// `sh_info`.
    pub info: u32,
    /// Contents.
    pub members: Vec<Placed>,
    /// Offset of the name in `.shstrtab`.
    pub name_offset: u32,
}

impl OutSection<'_> {
    /// Whether the section is allocated.
    #[must_use]
    pub fn is_alloc(&self) -> bool {
        self.flags & SHF_ALLOC != 0
    }

    /// Whether the section occupies file space.
    #[must_use]
    pub fn has_file_bytes(&self) -> bool {
        self.sh_type != SHT_NOBITS
    }
}

/// A program header.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Segment {
    /// `p_type`.
    pub p_type: u32,
    /// `p_flags`.
    pub flags: u32,
    /// `p_offset`.
    pub offset: u64,
    /// `p_vaddr` and `p_paddr`.
    pub vaddr: u64,
    /// `p_filesz`.
    pub filesz: u64,
    /// `p_memsz`.
    pub memsz: u64,
    /// `p_align`.
    pub align: u64,
}

/// The thread-local storage template.
#[derive(Clone, Copy, Debug, Default)]
pub struct Tls {
    /// Address of the TLS segment.
    pub start: u64,
    /// Its memory size.
    pub memsz: u64,
    /// Its alignment.
    pub align: u64,
}

impl Tls {
    /// The thread pointer's address for local-exec offsets (variant II: the
    /// TLS block ends at the thread pointer).
    #[must_use]
    pub fn tp(&self) -> u64 {
        let align = self.align.max(1);
        let size = self
            .memsz
            .checked_add(align.wrapping_sub(1))
            .map_or(self.memsz, |v| v & !align.wrapping_sub(1));
        self.start.wrapping_add(size)
    }
}

/// Sizes of the trailing tables, known before layout.
#[derive(Clone, Copy, Debug, Default)]
pub struct TrailerSizes {
    /// `.symtab` size (0 when stripped).
    pub symtab: u64,
    /// `.strtab` size.
    pub strtab: u64,
    /// Index of the first global symbol.
    pub first_global: u32,
}

/// The finished layout.
#[derive(Debug)]
pub struct Layout<'a> {
    /// Output sections in section header order (index 0 is the null section,
    /// which is not stored: header index = position + 1).
    pub sections: Vec<OutSection<'a>>,
    /// Address and output section index (position in `sections`) of the
    /// place each placement output would occupy; `(address, end, NONE)` for
    /// dropped outputs.
    pub output_places: Vec<(u64, u64, u32)>,
    /// Address of every input section (0 when not in the output).
    pub section_addr: Vec<u64>,
    /// Header index (position + 1 in `sections`) of every input section's
    /// output section, or 0.
    pub section_shndx: Vec<u32>,
    /// Address and header index of every merge group.
    pub merge_place: Vec<(u64, u32)>,
    /// Address, file offset and size of each synthetic part that exists.
    pub synthetic: Vec<(Synthetic, u64, u64, u64)>,
    /// Program headers.
    pub segments: Vec<Segment>,
    /// The TLS template, if any.
    pub tls: Option<Tls>,
    /// Base address.
    pub base: u64,
    /// Section header table offset.
    pub shoff: u64,
    /// Total file size.
    pub file_size: u64,
    /// `.shstrtab` contents.
    pub shstrtab: Vec<u8>,
    /// End of the text segment (`_etext`).
    pub etext: u64,
    /// End of initialized data (`_edata`).
    pub edata: u64,
    /// Start of `.bss`.
    pub bss_start: u64,
    /// End of the image (`_end`).
    pub end: u64,
}

impl Layout<'_> {
    /// Address, file offset and size of a synthetic part.
    #[must_use]
    pub fn synthetic(&self, kind: Synthetic) -> Option<(u64, u64, u64)> {
        self.synthetic
            .iter()
            .find(|(k, ..)| *k == kind)
            .map(|&(_, addr, offset, size)| (addr, offset, size))
    }

    /// The output section named `name`, by placement name.
    #[must_use]
    pub fn by_name(&self, name: &[u8]) -> Option<&OutSection<'_>> {
        self.sections.iter().find(|s| s.name == name)
    }
}

/// Everything layout reads.
pub struct LayoutInput<'l, 'a> {
    /// Options.
    pub options: &'l LinkOptions,
    /// Rules.
    pub rules: &'l RuleSet,
    /// Inputs.
    pub files: &'l [ElfInput<'a>],
    /// Input sections.
    pub sections: &'l Sections,
    /// Placement.
    pub placement: &'l Placement<'a>,
    /// Merge groups.
    pub merged: &'l Merged<'l, 'a>,
    /// `.eh_frame` sections.
    pub eh_frames: &'l EhFrames<'a>,
    /// Synthetic sections.
    pub synth: &'l Synth,
    /// Trailing table sizes.
    pub trailers: TrailerSizes,
    /// Whether any input requested an executable stack.
    pub exec_stack: bool,
}

fn align_up(value: u64, align: u64) -> Result<u64> {
    let align = align.max(1);
    if !align.is_power_of_two() {
        return Err(Error::Internal(format!(
            "alignment {align:#x} is not a power of two"
        )));
    }
    value
        .checked_add(align.wrapping_sub(1))
        .map(|v| v & !align.wrapping_sub(1))
        .ok_or_else(|| Error::Limit("output larger than the address space".into()))
}

fn add(a: u64, b: u64) -> Result<u64> {
    a.checked_add(b)
        .ok_or_else(|| Error::Limit("output larger than the address space".into()))
}

/// Sort key of an input member within its output section.
#[derive(Clone, Copy, Debug)]
struct Key {
    sub: u16,
    priority: u32,
    id: SectionId,
    member: Member,
}

fn synthetic_goes_last(kind: Synthetic) -> bool {
    matches!(
        kind,
        Synthetic::Common | Synthetic::Comment | Synthetic::EhFrameEnd
    )
}

/// Runs layout.
///
/// # Errors
///
/// Returns [`Error::Limit`] when the image does not fit the address space.
pub fn layout<'a>(input: &LayoutInput<'_, 'a>) -> Result<Layout<'a>> {
    let placement = input.placement;
    let sections = input.sections;
    let files = input.files;
    let output_count = placement.outputs.len();

    // 1. Members, grouped by output section in section ID order.
    let mut members: Vec<Vec<Key>> = (0..output_count).map(|_| Vec::new()).collect();
    for (file_index, file) in files.iter().enumerate() {
        let Some(object) = &file.object else {
            continue;
        };
        for (index, section) in object.sections.iter().enumerate() {
            let Some(id) = sections.id(file_index, u32::try_from(index).unwrap_or(NONE)) else {
                continue;
            };
            if !sections.is_live(id) {
                continue;
            }
            let Some(output) = placement.output_of(id) else {
                continue;
            };
            let member = if section.kind == SectionKind::Merge {
                match input.merged.group_of(id) {
                    Some(group) if input.merged.group_first.get(group as usize) == Some(&id) => {
                        Member::Merge(group)
                    }
                    Some(_) => continue,
                    None => Member::Input(id),
                }
            } else {
                Member::Input(id)
            };
            let sub = placement.sub.get(id.index()).copied().unwrap_or(0);
            let sort = placement
                .outputs
                .get(output as usize)
                .and_then(|o| input.rules.outputs.get(usize::from(o.rule)))
                .and_then(|r| r.inputs.get(usize::from(sub)))
                .map_or(SortMode::None, |i| i.sort);
            let priority = match sort {
                SortMode::InitPriority => priority(section.name),
                _ => 0,
            };
            if let Some(list) = members.get_mut(output as usize) {
                list.push(Key {
                    sub,
                    priority,
                    id,
                    member,
                });
            }
        }
    }

    // 2. Sort and size each output section, in parallel.
    let name_of = |id: SectionId| -> &[u8] {
        sections
            .locate(id)
            .and_then(|(file, index)| {
                files
                    .get(file)?
                    .object
                    .as_ref()?
                    .section(index)
                    .map(|s| s.name)
            })
            .unwrap_or_default()
    };
    let sized: Vec<Result<(Vec<Placed>, u64, u64)>> = members
        .into_par_iter()
        .enumerate()
        .map(|(output_index, mut keys)| {
            let output = placement.outputs.get(output_index);
            let rule = output.and_then(|o| input.rules.outputs.get(usize::from(o.rule)));
            let by_name = rule.is_some_and(|r| r.inputs.iter().any(|i| i.sort == SortMode::Name));
            keys.sort_by(|a, b| {
                a.sub.cmp(&b.sub).then_with(|| {
                    let named = by_name
                        && rule
                            .and_then(|r| r.inputs.get(usize::from(a.sub)))
                            .is_some_and(|i| i.sort == SortMode::Name);
                    let by_name = if named {
                        name_of(a.id).cmp(name_of(b.id))
                    } else {
                        core::cmp::Ordering::Equal
                    };
                    by_name
                        .then(a.priority.cmp(&b.priority))
                        .then(a.id.cmp(&b.id))
                })
            });
            let synthetic = output.map_or(Synthetic::None, |o| o.synthetic);
            let mut list: Vec<Member> = Vec::with_capacity(keys.len().saturating_add(1));
            // With `-z now` there is no lazy binding, and GNU ld puts the
            // IFUNC slots (`.igot.plt`) in `.got`.
            let bind_now = input.options.bind_now;
            match synthetic {
                Synthetic::IgotPlt if bind_now => {}
                Synthetic::Got if bind_now => {
                    list.push(Member::Synthetic(Synthetic::Got));
                    list.push(Member::Synthetic(Synthetic::IgotPlt));
                }
                Synthetic::None => {}
                kind if !synthetic_goes_last(kind) => list.push(Member::Synthetic(kind)),
                _ => {}
            }
            list.extend(keys.iter().map(|k| k.member));
            if synthetic_goes_last(synthetic) {
                list.push(Member::Synthetic(synthetic));
            }
            let mut offset = 0u64;
            let mut align = 1u64;
            let mut placed = Vec::with_capacity(list.len());
            for member in list {
                let (size, member_align) = member_size(input, member)?;
                if size == 0 && matches!(member, Member::Synthetic(_)) {
                    continue;
                }
                let member_align = member_align.max(1);
                offset = align_up(offset, member_align)?;
                align = align.max(member_align);
                placed.push(Placed {
                    member,
                    offset,
                    size,
                });
                offset = add(offset, size)?;
            }
            Ok((placed, offset, align))
        })
        .collect();

    // Order outputs.
    let mut order: Vec<usize> = (0..output_count).collect();
    order.sort_by_key(|&i| placement.outputs.get(i).map(|o| o.rank));

    let mut out_sections: Vec<OutSection<'a>> = Vec::new();
    let mut index_of_output = vec![NONE; output_count];
    let mut dropped: Vec<(usize, Vec<Placed>)> = Vec::new();
    let mut sized: Vec<Option<(Vec<Placed>, u64, u64)>> = sized
        .into_iter()
        .map(|r| r.map(Some))
        .collect::<Result<_>>()?;
    for &output_index in &order {
        let (Some(output), Some(slot)) = (
            placement.outputs.get(output_index),
            sized.get_mut(output_index),
        ) else {
            continue;
        };
        let Some((placed, size, align)) = slot.take() else {
            continue;
        };
        if placed.is_empty() {
            continue;
        }
        if size == 0 {
            // Dropped, but symbols in its (empty) input sections still need
            // an address: the location counter where it would have been.
            dropped.push((output_index, placed));
            continue;
        }
        let mut flags = output.flags;
        let mut sh_type = output.sh_type;
        if flags == 0 {
            // Purely synthetic: take flags from the kind.
            (flags, sh_type) = synthetic_flags(output.synthetic);
        } else if output.synthetic != Synthetic::None
            && placed
                .iter()
                .any(|p| matches!(p.member, Member::Synthetic(_)))
        {
            let (extra, synth_type) = synthetic_flags(output.synthetic);
            flags |= extra;
            if synth_type != SHT_NOBITS && sh_type == SHT_NOBITS {
                sh_type = synth_type;
            }
        }
        let entsize = entsize_of(input, output_index, &placed);
        if let Some(slot) = index_of_output.get_mut(output_index) {
            *slot = u32::try_from(out_sections.len()).unwrap_or(NONE);
        }
        out_sections.push(OutSection {
            name: output.name,
            output: u32::try_from(output_index).unwrap_or(NONE),
            trailer: Trailer::None,
            sh_type,
            flags,
            addr: 0,
            offset: 0,
            size,
            align,
            entsize,
            link: 0,
            info: 0,
            members: placed,
            name_offset: 0,
        });
    }

    // Trailers.
    if input.trailers.symtab > 0 {
        out_sections.push(trailer(
            b".symtab",
            Trailer::Symtab,
            SHT_SYMTAB,
            input.trailers.symtab,
            8,
        ));
        out_sections.push(trailer(
            b".strtab",
            Trailer::Strtab,
            SHT_STRTAB,
            input.trailers.strtab,
            1,
        ));
    }
    out_sections.push(trailer(b".shstrtab", Trailer::Shstrtab, SHT_STRTAB, 0, 1));

    // Section names.
    let mut shstrtab = vec![0u8];
    for section in &mut out_sections {
        section.name_offset = u32::try_from(shstrtab.len())
            .map_err(|_| Error::Limit("section name table larger than 4 GiB".into()))?;
        shstrtab.extend_from_slice(section.name);
        shstrtab.push(0);
    }
    let shstrtab_len = u64::try_from(shstrtab.len()).unwrap_or(u64::MAX);
    let symtab_position = out_sections
        .iter()
        .position(|s| s.trailer == Trailer::Symtab);
    let strtab_position = out_sections
        .iter()
        .position(|s| s.trailer == Trailer::Strtab);
    for section in &mut out_sections {
        match section.trailer {
            Trailer::Shstrtab => section.size = shstrtab_len,
            Trailer::Symtab => {
                section.link = strtab_position
                    .and_then(|p| u32::try_from(p.saturating_add(1)).ok())
                    .unwrap_or(0);
                section.info = input.trailers.first_global;
                section.entsize = 24;
            }
            _ => {}
        }
    }
    let _ = symtab_position;

    // 3. Segment plan (before addresses: the header size depends on it).
    let separate = input.options.separate_code.unwrap_or(SeparateCode::Code);
    let perm = |section: &OutSection<'_>| -> u32 {
        let mut flags = PF_R;
        if section.flags & SHF_EXECINSTR != 0 {
            flags |= PF_X;
        }
        if section.flags & SHF_WRITE != 0 {
            flags |= PF_W;
        }
        if separate == SeparateCode::None && flags == PF_R {
            flags |= PF_X;
        }
        flags
    };
    let alloc: Vec<usize> = out_sections
        .iter()
        .enumerate()
        .filter(|(_, s)| s.is_alloc())
        .map(|(i, _)| i)
        .collect();
    let mut load_count = 1usize;
    let mut previous = if separate == SeparateCode::None {
        PF_R | PF_X
    } else {
        PF_R
    };
    for &i in &alloc {
        if let Some(section) = out_sections.get(i) {
            let p = perm(section);
            if p != previous {
                load_count = load_count.saturating_add(1);
                previous = p;
            }
        }
    }
    let mut note_groups = 0usize;
    let mut last_note_align: Option<u64> = None;
    let mut has_tls = false;
    for &i in &alloc {
        let Some(section) = out_sections.get(i) else {
            continue;
        };
        if section.sh_type == SHT_NOTE {
            if last_note_align != Some(section.align) {
                note_groups = note_groups.saturating_add(1);
            }
            last_note_align = Some(section.align);
        } else {
            last_note_align = None;
        }
        has_tls |= section.flags & SHF_TLS != 0;
    }
    let has_property = input.synth.property_note.is_some();
    let has_eh_hdr = input.synth.eh_frame_hdr && input.synth.fde_count > 0;
    let gnu_stack = input.options.gnu_stack;
    let phnum = load_count
        .saturating_add(note_groups)
        .saturating_add(usize::from(has_tls))
        .saturating_add(usize::from(has_property))
        .saturating_add(usize::from(has_eh_hdr))
        .saturating_add(usize::from(gnu_stack));
    let phnum_u64 = u64::try_from(phnum).unwrap_or(u64::MAX);

    // 4. Addresses.
    let page = input
        .options
        .max_page_size
        .filter(|p| p.is_power_of_two())
        .unwrap_or(DEFAULT_PAGE);
    let base = input
        .options
        .text_segment
        .or(input.options.image_base)
        .unwrap_or(DEFAULT_BASE);
    let base = align_up(base, 1)?;
    let headers = add(EHDR_SIZE, PHDR_SIZE.saturating_mul(phnum_u64))?;
    let mut dot = add(base, headers)?;
    let mut previous = if separate == SeparateCode::None {
        PF_R | PF_X
    } else {
        PF_R
    };
    let mut segments: Vec<Segment> = vec![Segment {
        p_type: PT_LOAD,
        flags: previous,
        offset: 0,
        vaddr: base,
        filesz: headers,
        memsz: headers,
        align: page,
    }];
    let mut tls: Option<Tls> = None;
    let mut etext = dot;
    let mut edata = dot;
    let mut bss_start: Option<u64> = None;
    let mut output_places = vec![(0u64, 0u64, NONE); output_count];
    let mut out_iter = order.iter().peekable();
    let mut place_empty_until = |rank_output: u32, dot: u64, places: &mut Vec<(u64, u64, u32)>| {
        // Give dropped outputs ranked before `rank_output` the current dot.
        while let Some(&&next) = out_iter.peek() {
            if u32::try_from(next).ok() == Some(rank_output) {
                out_iter.next();
                break;
            }
            if let Some(slot) = places.get_mut(next)
                && slot.2 == NONE
            {
                *slot = (dot, dot, NONE);
            }
            out_iter.next();
        }
    };
    for &i in &alloc {
        let p = out_sections.get(i).map_or(PF_R, perm);
        let Some(section) = out_sections.get_mut(i) else {
            continue;
        };
        place_empty_until(section.output, dot, &mut output_places);
        if p != previous {
            dot = align_up(dot, page)?;
            previous = p;
            segments.push(Segment {
                p_type: PT_LOAD,
                flags: p,
                offset: dot.wrapping_sub(base),
                vaddr: dot,
                filesz: 0,
                memsz: 0,
                align: page,
            });
        }
        let address = align_up(dot, section.align)?;
        section.addr = address;
        section.offset = address
            .checked_sub(base)
            .ok_or_else(|| Error::Internal("section below the base address".into()))?;
        let end = add(address, section.size)?;
        let tbss = section.flags & SHF_TLS != 0 && section.sh_type == SHT_NOBITS;
        if section.flags & SHF_TLS != 0 {
            let t = tls.get_or_insert(Tls {
                start: address,
                memsz: 0,
                align: 1,
            });
            t.memsz = end.saturating_sub(t.start);
            t.align = t.align.max(section.align);
        }
        if let Some(segment) = segments.last_mut() {
            if section.has_file_bytes() {
                segment.filesz = end.saturating_sub(segment.vaddr);
            }
            if !tbss {
                segment.memsz = end.saturating_sub(segment.vaddr);
            }
        }
        if p & PF_X != 0 {
            etext = end;
        }
        if section.sh_type == SHT_NOBITS && !tbss && section.flags & SHF_WRITE != 0 {
            bss_start.get_or_insert(address);
        } else if section.flags & SHF_WRITE != 0 && !tbss {
            edata = end;
        }
        if let Some(slot) = output_places.get_mut(section.output as usize) {
            *slot = (address, end, u32::try_from(i).unwrap_or(NONE));
        }
        if !tbss {
            dot = end;
        }
    }
    let end = align_up(dot, 8)?;
    place_empty_until(NONE, dot, &mut output_places);

    // Non-allocated sections.
    let mut file_end = segments
        .iter()
        .map(|s| s.offset.saturating_add(s.filesz))
        .max()
        .unwrap_or(headers);
    for (i, section) in out_sections.iter_mut().enumerate() {
        if section.is_alloc() {
            continue;
        }
        section.offset = align_up(file_end, section.align)?;
        file_end = add(section.offset, section.size)?;
        if let Some(slot) = output_places.get_mut(section.output as usize) {
            *slot = (0, 0, u32::try_from(i).unwrap_or(NONE));
        }
    }
    let shnum = u64::try_from(out_sections.len().saturating_add(1)).unwrap_or(u64::MAX);
    let shoff = align_up(file_end, 8)?;
    let file_size = add(shoff, shnum.saturating_mul(SHDR_SIZE))?;

    // Other program headers.
    let mut group: Option<Segment> = None;
    for &i in &alloc {
        let Some(section) = out_sections.get(i) else {
            continue;
        };
        if section.sh_type == SHT_NOTE {
            match &mut group {
                Some(g) if g.align == section.align => {
                    let end = section.addr.saturating_add(section.size);
                    g.filesz = end.saturating_sub(g.vaddr);
                    g.memsz = g.filesz;
                }
                _ => {
                    if let Some(done) = group.take() {
                        segments.push(done);
                    }
                    group = Some(Segment {
                        p_type: PT_NOTE,
                        flags: PF_R,
                        offset: section.offset,
                        vaddr: section.addr,
                        filesz: section.size,
                        memsz: section.size,
                        align: section.align,
                    });
                }
            }
        } else if let Some(done) = group.take() {
            segments.push(done);
        }
    }
    if let Some(done) = group.take() {
        segments.push(done);
    }
    if let Some(t) = tls {
        let first = alloc
            .iter()
            .filter_map(|&i| out_sections.get(i))
            .find(|s| s.flags & SHF_TLS != 0);
        let filesz = alloc
            .iter()
            .filter_map(|&i| out_sections.get(i))
            .filter(|s| s.flags & SHF_TLS != 0 && s.sh_type != SHT_NOBITS)
            .map(|s| s.addr.saturating_add(s.size).saturating_sub(t.start))
            .max()
            .unwrap_or(0);
        segments.push(Segment {
            p_type: PT_TLS,
            flags: PF_R,
            offset: first.map_or(0, |s| s.offset),
            vaddr: t.start,
            filesz,
            memsz: t.memsz,
            align: t.align,
        });
    }
    let mut synthetic_places = Vec::new();
    for section in &out_sections {
        for placed in &section.members {
            if let Member::Synthetic(kind) = placed.member {
                synthetic_places.push((
                    kind,
                    section.addr.saturating_add(placed.offset),
                    section.offset.saturating_add(placed.offset),
                    placed.size,
                ));
            }
        }
    }
    if has_property
        && let Some(&(_, addr, offset, size)) = synthetic_places
            .iter()
            .find(|(k, ..)| *k == Synthetic::GnuProperty)
    {
        segments.push(Segment {
            p_type: PT_GNU_PROPERTY,
            flags: PF_R,
            offset,
            vaddr: addr,
            filesz: size,
            memsz: size,
            align: 8,
        });
    }
    if has_eh_hdr
        && let Some(&(_, addr, offset, size)) = synthetic_places
            .iter()
            .find(|(k, ..)| *k == Synthetic::EhFrameHdr)
    {
        segments.push(Segment {
            p_type: PT_GNU_EH_FRAME,
            flags: PF_R,
            offset,
            vaddr: addr,
            filesz: size,
            memsz: size,
            align: 4,
        });
    }
    if gnu_stack {
        let exec = match input.options.exec_stack {
            ExecStack::Executable => true,
            ExecStack::NonExecutable => false,
            ExecStack::FromInputs => input.exec_stack,
        };
        segments.push(Segment {
            p_type: PT_GNU_STACK,
            flags: PF_R | PF_W | if exec { PF_X } else { 0 },
            offset: 0,
            vaddr: 0,
            filesz: 0,
            memsz: input.options.stack_size.unwrap_or(0),
            align: 16,
        });
    }
    if segments.len() != phnum {
        return Err(Error::Internal(format!(
            "program header count changed during layout ({phnum} planned, {} made)",
            segments.len()
        )));
    }

    // Per input section addresses.
    let mut section_addr = vec![0u64; sections.len()];
    let mut section_shndx = vec![0u32; sections.len()];
    let mut merge_place = vec![(0u64, 0u32); input.merged.groups.len()];
    for (position, section) in out_sections.iter().enumerate() {
        let shndx = u32::try_from(position.saturating_add(1)).unwrap_or(0);
        for placed in &section.members {
            let address = section.addr.saturating_add(placed.offset);
            match placed.member {
                Member::Input(id) => {
                    if let Some(slot) = section_addr.get_mut(id.index()) {
                        *slot = address;
                    }
                    if let Some(slot) = section_shndx.get_mut(id.index()) {
                        *slot = shndx;
                    }
                }
                Member::Merge(group) => {
                    if let Some(slot) = merge_place.get_mut(group as usize) {
                        *slot = (address, shndx);
                    }
                }
                Member::Synthetic(_) => {}
            }
        }
    }
    for (output_index, placed) in &dropped {
        let address = output_places.get(*output_index).map_or(0, |p| p.0);
        for p in placed {
            if let Member::Input(id) = p.member {
                if let Some(slot) = section_addr.get_mut(id.index()) {
                    *slot = address;
                }
                if let Some(slot) = section_shndx.get_mut(id.index()) {
                    *slot = EMPTY_SHNDX;
                }
            }
        }
    }
    // Merged input sections point at their group, for symbols.
    for (id_index, shndx) in section_shndx.iter_mut().enumerate() {
        if *shndx != 0 {
            continue;
        }
        let id = SectionId::new(id_index);
        if let Some(group) = input.merged.group_of(id)
            && let Some(&(_, group_shndx)) = merge_place.get(group as usize)
        {
            *shndx = group_shndx;
        }
    }

    let bss_start = bss_start.unwrap_or(edata);
    Ok(Layout {
        sections: out_sections,
        output_places,
        section_addr,
        section_shndx,
        merge_place,
        synthetic: synthetic_places,
        segments,
        tls,
        base,
        shoff,
        file_size,
        shstrtab,
        etext,
        edata,
        bss_start,
        end,
    })
}

fn trailer(
    name: &'static [u8],
    kind: Trailer,
    sh_type: u32,
    size: u64,
    align: u64,
) -> OutSection<'static> {
    OutSection {
        name,
        output: NONE,
        trailer: kind,
        sh_type,
        flags: 0,
        addr: 0,
        offset: 0,
        size,
        align,
        entsize: 0,
        link: 0,
        info: 0,
        members: Vec::new(),
        name_offset: 0,
    }
}

fn synthetic_flags(kind: Synthetic) -> (u64, u32) {
    use crate::elf::read::consts::{SHF_INFO_LINK, SHT_RELA};
    match kind {
        Synthetic::None => (0, SHT_PROGBITS),
        Synthetic::BuildId | Synthetic::GnuProperty => (SHF_ALLOC, SHT_NOTE),
        Synthetic::RelaIplt => (SHF_ALLOC | SHF_INFO_LINK, SHT_RELA),
        Synthetic::Iplt => (SHF_ALLOC | SHF_EXECINSTR, SHT_PROGBITS),
        Synthetic::EhFrameHdr | Synthetic::EhFrameEnd => (SHF_ALLOC, SHT_PROGBITS),
        Synthetic::Got | Synthetic::IgotPlt => (SHF_ALLOC | SHF_WRITE, SHT_PROGBITS),
        Synthetic::Common => (SHF_ALLOC | SHF_WRITE, SHT_NOBITS),
        Synthetic::Comment => (
            crate::elf::read::consts::SHF_MERGE | crate::elf::read::consts::SHF_STRINGS,
            SHT_PROGBITS,
        ),
    }
}

fn member_size(input: &LayoutInput<'_, '_>, member: Member) -> Result<(u64, u64)> {
    Ok(match member {
        Member::Input(id) => {
            let (file, index) = input
                .sections
                .locate(id)
                .ok_or_else(|| Error::Internal("unknown input section".into()))?;
            let section = input
                .files
                .get(file)
                .and_then(|f| f.object.as_ref())
                .and_then(|o| o.section(index))
                .ok_or_else(|| Error::Internal("unknown input section".into()))?;
            let size = if section.kind == SectionKind::EhFrame {
                input
                    .eh_frames
                    .find(id)
                    .and_then(|i| input.eh_frames.sections.get(i))
                    .map_or(0, |s| s.size)
            } else {
                section.header.sh_size
            };
            (size, section.header.sh_addralign)
        }
        Member::Merge(group) => {
            let merged = input
                .merged
                .merged
                .group(group as usize)
                .ok_or_else(|| Error::Internal("unknown merge group".into()))?;
            (merged.size(), merged.alignment())
        }
        Member::Synthetic(kind) => input.synth.size_align(kind),
    })
}

fn entsize_of(input: &LayoutInput<'_, '_>, _output: usize, placed: &[Placed]) -> u64 {
    let mut entsize: Option<u64> = None;
    for p in placed {
        let size = match p.member {
            Member::Input(id) => input
                .sections
                .locate(id)
                .and_then(|(f, i)| input.files.get(f)?.object.as_ref()?.section(i))
                .map_or(0, |s| s.header.sh_entsize),
            Member::Merge(group) => input
                .merged
                .groups
                .get(group as usize)
                .map_or(0, |g| match g.kind {
                    crate::passes::merge::MergeKind::Strings { char_size } => u64::from(char_size),
                    crate::passes::merge::MergeKind::Fixed { entry_size } => entry_size,
                }),
            Member::Synthetic(Synthetic::RelaIplt) => 24,
            Member::Synthetic(Synthetic::Got | Synthetic::IgotPlt) => 8,
            Member::Synthetic(Synthetic::Comment) => 1,
            Member::Synthetic(_) => 0,
        };
        match entsize {
            None => entsize = Some(size),
            Some(e) if e != size => return 0,
            Some(_) => {}
        }
    }
    entsize.unwrap_or(0)
}
