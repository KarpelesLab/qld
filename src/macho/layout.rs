//! Output sections, segments and addresses.
//!
//! Input sections are grouped into output sections by segment and section
//! name (after the renames lld applies: `__TEXT,__StaticInit` into
//! `__text`, and pointer sections of `__DATA` into `__DATA_CONST`). Sections
//! and segments are ordered the way lld orders them, so that simple links
//! lay out the same way lld lays them out:
//!
//! - segments: `__PAGEZERO`, `__TEXT`, `__DATA_CONST`, `__DATA`, others in
//!   input order, `__LINKEDIT`;
//! - `__TEXT`: `__text`, `__stubs`, then input order, with `__unwind_info`
//!   and `__eh_frame` last;
//! - `__DATA` and `__DATA_CONST`: `__got`, `__const`, input order, then the
//!   thread-local sections (so dyld copies one contiguous template) and the
//!   zero-fill sections, which must end the segment.
//!
//! Literals are merged by content within each output section, as ld64 and
//! lld merge them: C strings (`S_CSTRING_LITERALS`) and 4-, 8- and 16-byte
//! literals. Duplicates take the place of the first copy in member order,
//! which is aligned to the largest alignment among them.
//!
//! Addresses follow: the header and load commands start `__TEXT`, sections
//! are aligned in both address and file offset, and every segment but
//! `__LINKEDIT` is padded to the page size.

#![deny(clippy::arithmetic_side_effects)]

use hashbrown::HashMap;

use crate::error::{Error, Result};
use crate::macho::read::consts::{
    S_4BYTE_LITERALS, S_8BYTE_LITERALS, S_16BYTE_LITERALS, S_ATTR_DEBUG, S_ATTR_EXT_RELOC,
    S_ATTR_LOC_RELOC, S_ATTR_NO_DEAD_STRIP, S_ATTR_PURE_INSTRUCTIONS, S_ATTR_SOME_INSTRUCTIONS,
    S_CSTRING_LITERALS, S_NON_LAZY_SYMBOL_POINTERS, S_REGULAR, S_SYMBOL_STUBS,
    S_THREAD_LOCAL_REGULAR, S_THREAD_LOCAL_VARIABLE_POINTERS, S_THREAD_LOCAL_VARIABLES,
    S_THREAD_LOCAL_ZEROFILL, S_ZEROFILL, SECTION_TYPE,
};

use super::buf::align_up;
use super::state::{Link, NONE};

/// `SG_READ_ONLY`: dyld makes the segment read-only after fixups.
pub const SG_READ_ONLY: u32 = 0x10;

/// What fills an output section.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SectionKind {
    /// Atoms of input sections.
    Input,
    /// `__stubs`.
    Stubs,
    /// `__got`.
    Got,
    /// `__auth_got` (arm64e): the signed pointers `__auth_stubs` load.
    AuthGot,
    /// `__thread_ptrs`.
    ThreadPtrs,
    /// `__common`: tentative definitions.
    Common,
    /// `__unwind_info`.
    UnwindInfo,
    /// `__eh_frame`.
    EhFrame,
    /// `-sectcreate` contents: index into the list.
    Sectcreate(usize),
}

/// One output section.
#[derive(Clone, Debug)]
pub struct OutSection {
    /// Segment name.
    pub segname: Vec<u8>,
    /// Section name.
    pub sectname: Vec<u8>,
    /// Type and attributes.
    pub flags: u32,
    /// Alignment, as a power of two.
    pub align: u32,
    /// Address.
    pub addr: u64,
    /// Size.
    pub size: u64,
    /// File offset (0 for zero-fill sections).
    pub offset: u64,
    /// What fills it.
    pub kind: SectionKind,
    /// Creation order, for sorting.
    pub input_order: usize,
    /// Contributing input sections, as (file, section index), in order.
    pub inputs: Vec<(u32, u32)>,
    /// `reserved1` (indirect symbol table index for stubs and pointers).
    pub reserved1: u32,
    /// `reserved2` (stub size).
    pub reserved2: u32,
    /// Index of the segment.
    pub segment: usize,
}

impl OutSection {
    /// Whether the section has no file contents.
    #[must_use]
    pub fn is_zerofill(&self) -> bool {
        matches!(
            self.flags & SECTION_TYPE,
            S_ZEROFILL | S_THREAD_LOCAL_ZEROFILL | 0x0c
        )
    }

    /// Whether the section holds code.
    #[must_use]
    pub fn has_code(&self) -> bool {
        self.flags & (S_ATTR_PURE_INSTRUCTIONS | S_ATTR_SOME_INSTRUCTIONS) != 0
    }

    /// The end address.
    #[must_use]
    pub fn end(&self) -> u64 {
        self.addr.saturating_add(self.size)
    }
}

/// One output segment.
#[derive(Clone, Debug, Default)]
pub struct OutSegment {
    /// Name.
    pub name: Vec<u8>,
    /// Address.
    pub vmaddr: u64,
    /// Size in memory.
    pub vmsize: u64,
    /// File offset.
    pub fileoff: u64,
    /// Size in the file.
    pub filesize: u64,
    /// Maximum protection.
    pub maxprot: u32,
    /// Initial protection.
    pub initprot: u32,
    /// `SG_*` flags.
    pub flags: u32,
    /// Its sections, in address order.
    pub sections: Vec<usize>,
}

/// The layout of everything except `__LINKEDIT`'s contents.
#[derive(Clone, Debug, Default)]
pub struct Layout {
    /// Output sections, in address order.
    pub sections: Vec<OutSection>,
    /// Segments, in address order (`__LINKEDIT` last, with no size yet).
    pub segments: Vec<OutSegment>,
    /// Output section of each atom (global numbering), or [`NONE`].
    pub atom_section: Vec<u32>,
    /// Offset of each atom within its output section.
    pub atom_offset: Vec<u64>,
    /// Offset of each common symbol (in [`super::scan::Synthetic::commons`]
    /// order) within `__common`.
    pub common_offset: Vec<u64>,
    /// Size of the Mach-O header and load commands.
    pub header_size: u64,
    /// The atoms of each output section, in placement order (global
    /// numbering).
    pub members: Vec<Vec<usize>>,
    /// Range-extension thunk islands: space reserved inside code sections.
    pub islands: Vec<Island>,
}

/// Space for range-extension thunks inside a code section.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Island {
    /// Output section index.
    pub section: usize,
    /// Offset within the section.
    pub offset: u64,
    /// Size in bytes.
    pub size: u64,
}

/// Sizes of the synthetic sections.
#[derive(Clone, Copy, Debug, Default)]
pub struct SyntheticSizes {
    /// `__stubs` entries.
    pub stubs: u64,
    /// `__got` entries.
    pub got: u64,
    /// `__auth_got` entries (arm64e).
    pub auth_got: u64,
    /// `__thread_ptrs` entries.
    pub thread_ptrs: u64,
    /// `__unwind_info` bytes.
    pub unwind_info: u64,
    /// `__eh_frame` bytes.
    pub eh_frame: u64,
}

/// The output segment and section names of an input section.
#[must_use]
pub fn output_names<'s>(
    segname: &'s [u8],
    sectname: &'s [u8],
    data_const: bool,
) -> (&'s [u8], &'s [u8]) {
    match (segname, sectname) {
        (b"__TEXT", b"__StaticInit" | b"__textcoal_nt") => (b"__TEXT", b"__text"),
        (b"__TEXT", b"__const_coal") => (b"__TEXT", b"__const"),
        (b"__DATA", b"__datacoal_nt") => (b"__DATA", b"__data"),
        (
            b"__DATA",
            b"__got" | b"__const" | b"__cfstring" | b"__mod_init_func" | b"__mod_term_func"
            | b"__nl_symbol_ptr" | b"__objc_classlist" | b"__objc_nlclslist" | b"__objc_catlist"
            | b"__objc_nlcatlist" | b"__objc_protolist" | b"__objc_imageinfo",
        ) if data_const => (b"__DATA_CONST", sectname),
        _ => (segname, sectname),
    }
}

/// An atom placed in an output section: its sort key (order file position,
/// cold), file and atom index.
pub(super) type Member = ((usize, bool), usize, usize);

/// Whether an output section with `flags` holds literals that are merged
/// by content: C strings and 4-, 8- and 16-byte literals. (Literal pointer
/// sections carry relocations and are kept as they are.)
pub(super) fn is_literal_section(flags: u32) -> bool {
    matches!(
        flags & SECTION_TYPE,
        S_CSTRING_LITERALS | S_4BYTE_LITERALS | S_8BYTE_LITERALS | S_16BYTE_LITERALS
    )
}

/// The outcome of literal deduplication over one output section's members.
#[derive(Debug, Default)]
pub(super) struct Literals {
    /// For each member position, the position of the first member with the
    /// same contents (itself when it is the first). Empty when nothing was
    /// merged.
    first: Vec<usize>,
    /// For each first copy, the largest alignment among its duplicates, so
    /// that every reference keeps the alignment it was assembled with.
    align: Vec<u32>,
}

impl Literals {
    pub(super) fn canonical(&self, position: usize) -> usize {
        self.first.get(position).copied().unwrap_or(position)
    }

    pub(super) fn align(&self, position: usize) -> Option<u32> {
        self.align.get(position).copied()
    }
}

/// Merges the literals of `list` (the sorted members of one output section)
/// with equal contents into their first occurrence, as ld64 and lld do.
/// The result depends only on the order of `list`.
pub(super) fn dedup_literals(link: &Link<'_>, list: &[Member]) -> Result<Literals> {
    let mut seen: HashMap<&[u8], usize> = HashMap::with_capacity(list.len());
    let mut first = Vec::with_capacity(list.len());
    let mut align = vec![0u32; list.len()];
    let mut merged = false;
    for (position, &(_, file, atom)) in list.iter().enumerate() {
        let Some(object) = link.object(file) else {
            first.push(position);
            continue;
        };
        let Some(info) = object.atoms.atoms().get(atom) else {
            first.push(position);
            continue;
        };
        let data = object
            .file
            .section_data(usize::try_from(info.section).unwrap_or(usize::MAX))?;
        let Some(bytes) = usize::try_from(info.offset)
            .ok()
            .zip(usize::try_from(info.range().end).ok())
            .and_then(|(start, end)| data.get(start..end))
        else {
            first.push(position);
            continue;
        };
        let canonical = *seen.entry(bytes).or_insert(position);
        merged |= canonical != position;
        first.push(canonical);
        if let Some(slot) = align.get_mut(canonical) {
            *slot = (*slot).max(info.align);
        }
    }
    if !merged {
        return Ok(Literals::default());
    }
    Ok(Literals { first, align })
}

/// The file and file-local index of global atom `atom`.
#[must_use]
pub fn atom_location(link: &Link<'_>, atom: usize) -> (usize, usize) {
    let atom32 = u32::try_from(atom).unwrap_or(u32::MAX);
    // `atom_base` is sorted: the file is the last one starting at or before
    // the atom.
    let file = link
        .atom_base
        .partition_point(|&base| base <= atom32)
        .saturating_sub(1);
    // Files without atoms share their base with the next file; step to the
    // one that owns atoms.
    let mut owner = file;
    while owner > 0
        && link.atom_base.get(owner) == link.atom_base.get(owner.saturating_sub(1))
        && link.object(owner).is_none()
    {
        owner = owner.saturating_sub(1);
    }
    let base = link.atom_base.get(owner).copied().unwrap_or(0);
    (
        owner,
        usize::try_from(atom32.saturating_sub(base)).unwrap_or(0),
    )
}

/// Reads an `-order_file`: one symbol per line, optionally prefixed with an
/// architecture (`arm64:`) and an object file (`foo.o:`); `#` starts a
/// comment. Returns each symbol's position.
///
/// # Errors
///
/// [`Error::Io`] when the file cannot be read.
pub fn read_order_file(path: &std::path::Path, arch: &str) -> Result<HashMap<Vec<u8>, usize>> {
    let text = std::fs::read(path).map_err(|error| Error::io(path, error))?;
    let mut order = HashMap::new();
    for line in text.split(|&b| b == b'\n') {
        let line = match line.iter().position(|&b| b == b'#') {
            Some(hash) => line.get(..hash).unwrap_or(&[]),
            None => line,
        };
        let mut symbol = line.trim_ascii();
        for known in [b"arm64:".as_slice(), b"x86_64:", b"arm64e:", b"x86_64h:"] {
            if let Some(rest) = symbol.strip_prefix(known) {
                if !known.starts_with(arch.as_bytes())
                    || known.len() != arch.len().saturating_add(1)
                {
                    symbol = b"";
                } else {
                    symbol = rest;
                }
                break;
            }
        }
        if let Some(position) = symbol.windows(3).position(|w| w == b".o:") {
            symbol = symbol.get(position.saturating_add(3)..).unwrap_or(&[]);
        }
        if symbol.is_empty() {
            continue;
        }
        let next = order.len();
        order.entry(symbol.to_vec()).or_insert(next);
    }
    Ok(order)
}

/// Whether an input section is consumed by the linker rather than copied.
#[must_use]
pub fn is_consumed(segname: &[u8], sectname: &[u8], flags: u32) -> bool {
    flags & S_ATTR_DEBUG != 0
        || segname == b"__LLVM"
        || (segname == b"__LD" && sectname == b"__compact_unwind")
        || (segname == b"__TEXT" && sectname == b"__eh_frame")
}

fn section_order(section: &OutSection) -> (i64, usize) {
    let big = i64::MAX;
    let order = match section.segname.as_slice() {
        b"__TEXT" => match section.sectname.as_slice() {
            b"__text" => -5,
            b"__stubs" | b"__auth_stubs" => -4,
            b"__stub_helper" => -3,
            b"__unwind_info" => big.saturating_sub(1),
            b"__eh_frame" => big,
            _ => 0,
        },
        b"__DATA" | b"__DATA_CONST" => match section.flags & SECTION_TYPE {
            S_THREAD_LOCAL_VARIABLES => big.saturating_sub(3),
            S_THREAD_LOCAL_REGULAR => big.saturating_sub(2),
            S_THREAD_LOCAL_ZEROFILL => big.saturating_sub(1),
            S_ZEROFILL => big,
            _ => match section.sectname.as_slice() {
                b"__got" => -3,
                b"__auth_got" => -2,
                b"__la_symbol_ptr" => -2,
                b"__const" => -1,
                _ => 0,
            },
        },
        _ => 0,
    };
    (order, section.input_order)
}

fn segment_order(name: &[u8]) -> i64 {
    match name {
        b"__PAGEZERO" => -4,
        b"__TEXT" => -3,
        b"__DATA_CONST" => -2,
        b"__DATA" => -1,
        b"__LLVM" => i64::MAX.saturating_sub(1),
        b"__LINKEDIT" => i64::MAX,
        _ => 0,
    }
}

fn protection(name: &[u8]) -> u32 {
    match name {
        b"__PAGEZERO" => 0,
        b"__TEXT" => 5,
        b"__LINKEDIT" => 1,
        _ => 3,
    }
}

#[allow(clippy::too_many_arguments)]
fn synthetic_section(
    builder: &mut Builder,
    input_sections: usize,
    segname: &[u8],
    sectname: &[u8],
    flags: u32,
    kind: SectionKind,
    size: u64,
    align: u32,
) -> usize {
    let index = builder.get(segname, sectname, flags, kind);
    if let Some(section) = builder.sections.get_mut(index) {
        section.kind = kind;
        section.size = size;
        section.align = section.align.max(align);
        section.input_order = section.input_order.saturating_add(input_sections);
    }
    index
}

struct Builder {
    sections: Vec<OutSection>,
    by_name: HashMap<(Vec<u8>, Vec<u8>), usize>,
}

impl Builder {
    fn get(&mut self, segname: &[u8], sectname: &[u8], flags: u32, kind: SectionKind) -> usize {
        let key = (segname.to_vec(), sectname.to_vec());
        if let Some(&index) = self.by_name.get(&key) {
            return index;
        }
        let index = self.sections.len();
        self.sections.push(OutSection {
            segname: key.0.clone(),
            sectname: key.1.clone(),
            flags: flags & !(S_ATTR_EXT_RELOC | S_ATTR_LOC_RELOC),
            align: 0,
            addr: 0,
            size: 0,
            offset: 0,
            kind,
            input_order: index,
            inputs: Vec::new(),
            reserved1: 0,
            reserved2: 0,
            segment: 0,
        });
        self.by_name.insert(key, index);
        index
    }
}

/// Assigns atoms to output sections and computes section sizes.
///
/// `commons` lists (size, alignment) of each common symbol.
///
/// # Errors
///
/// [`Error::Limit`] when a section grows past 4 GiB of alignment padding.
pub fn plan(
    link: &Link<'_>,
    synthetic: &SyntheticSizes,
    commons: &[(u64, u32)],
    sectcreate: &[(Vec<u8>, Vec<u8>, u64)],
    order: &HashMap<Vec<u8>, usize>,
) -> Result<Layout> {
    let config = link.config;
    let mut builder = Builder {
        sections: Vec::new(),
        by_name: HashMap::new(),
    };
    let mut atom_section = vec![NONE; link.atom_count];
    let mut atom_offset = vec![0u64; link.atom_count];
    let data_const = config.data_const;

    // Atoms of each output section, with their sort keys: symbols listed in
    // the order file first (in its order), then everything else in input
    // order, with cold functions (`N_COLD_FUNC`) last, as ld64 and lld do.
    let mut members: Vec<Vec<Member>> = Vec::new();
    for (file_index, _) in link.files.iter().enumerate() {
        let Some(object) = link.object(file_index) else {
            continue;
        };
        for (section_index, section) in object.file.sections().iter().enumerate() {
            if is_consumed(section.segname, section.sectname, section.flags) {
                continue;
            }
            let Some(range) = object.atoms.section_range(section_index) else {
                continue;
            };
            let atoms: Vec<usize> = range
                .filter(|&atom| link.is_live(file_index, atom))
                .collect();
            if atoms.is_empty() {
                continue;
            }
            let (segname, sectname) = output_names(section.segname, section.sectname, data_const);
            // Relative method lists move to `__TEXT,__objc_methlist`.
            let (moved, atoms): (Vec<usize>, Vec<usize>) = atoms.into_iter().partition(|&atom| {
                link.objc
                    .rewrite(link.atom_id(file_index, atom))
                    .is_some_and(|r| r.method_list)
            });
            let mut groups = Vec::with_capacity(2);
            if !atoms.is_empty() {
                let out = builder.get(segname, sectname, section.flags, SectionKind::Input);
                // `__objc_imageinfo` is one record, whatever the input count.
                if sectname == b"__objc_imageinfo"
                    && builder
                        .sections
                        .get(out)
                        .is_some_and(|s| !s.inputs.is_empty())
                {
                    continue;
                }
                groups.push((out, atoms));
            }
            if !moved.is_empty() {
                let out = builder.get(
                    b"__TEXT",
                    b"__objc_methlist",
                    S_REGULAR | S_ATTR_NO_DEAD_STRIP,
                    SectionKind::Input,
                );
                groups.push((out, moved));
            }
            for (out, atoms) in groups {
                let Some(out_section) = builder.sections.get_mut(out) else {
                    continue;
                };
                out_section.inputs.push((
                    u32::try_from(file_index).unwrap_or(NONE),
                    u32::try_from(section_index).unwrap_or(NONE),
                ));
                if members.len() <= out {
                    members.resize_with(out.saturating_add(1), Vec::new);
                }
                let Some(list) = members.get_mut(out) else {
                    continue;
                };
                for atom in atoms {
                    let mut listed = usize::MAX;
                    let mut cold = false;
                    for &symbol in object.atoms.atom_symbols(atom) {
                        let Ok(entry) = object.file.symbols().get(symbol) else {
                            continue;
                        };
                        cold |= entry.is_cold_func();
                        if let Some(&position) = order.get(entry.name) {
                            listed = listed.min(position);
                        }
                    }
                    let key = if listed == usize::MAX {
                        (usize::MAX, cold)
                    } else {
                        (listed, false)
                    };
                    list.push((key, file_index, atom));
                }
            }
        }
    }
    let mut ordered: Vec<Vec<usize>> = vec![Vec::new(); builder.sections.len()];
    for (out, list) in members.iter_mut().enumerate() {
        list.sort_by_key(|&(key, _, _)| key);
        let Some(out_section) = builder.sections.get_mut(out) else {
            continue;
        };
        let literals = if is_literal_section(out_section.flags) {
            dedup_literals(link, list)?
        } else {
            Literals::default()
        };
        if let Some(slot) = ordered.get_mut(out) {
            *slot = list
                .iter()
                .enumerate()
                .filter(|&(position, _)| literals.canonical(position) == position)
                .map(|(_, &(_, file, atom))| link.atom_id(file, atom))
                .collect();
        }
        for (position, &(_, file_index, atom)) in list.iter().enumerate() {
            let Some(info) = link
                .object(file_index)
                .and_then(|object| object.atoms.atoms().get(atom))
            else {
                continue;
            };
            let id = link.atom_id(file_index, atom);
            let canonical = literals.canonical(position);
            if canonical != position {
                // A duplicate literal shares the first copy's place, which
                // comes earlier in the list and is already placed.
                let first = list
                    .get(canonical)
                    .map(|&(_, file, atom)| link.atom_id(file, atom));
                if let Some(first) = first {
                    let offset = atom_offset.get(first).copied().unwrap_or(0);
                    if let Some(slot) = atom_section.get_mut(id) {
                        *slot = u32::try_from(out).unwrap_or(NONE);
                    }
                    if let Some(slot) = atom_offset.get_mut(id) {
                        *slot = offset;
                    }
                }
                continue;
            }
            let rewrite = link.objc.rewrite(id);
            let align = literals
                .align(position)
                .or(rewrite.map(|r| r.align))
                .unwrap_or(info.align);
            let size = rewrite.map_or(info.size, |r| {
                u64::try_from(r.bytes.len()).unwrap_or(u64::MAX)
            });
            out_section.align = out_section.align.max(align);
            let alignment = 1u64.checked_shl(align).unwrap_or(1);
            let offset = align_up(out_section.size, alignment);
            out_section.size = offset
                .checked_add(size)
                .ok_or_else(|| Error::Limit("output section larger than 2^64".into()))?;
            if let Some(slot) = atom_section.get_mut(id) {
                *slot = u32::try_from(out).unwrap_or(NONE);
            }
            if let Some(slot) = atom_offset.get_mut(id) {
                *slot = offset;
            }
        }
    }

    let input_sections = builder.sections.len();
    let got_segment: &[u8] = if data_const {
        b"__DATA_CONST"
    } else {
        b"__DATA"
    };
    if synthetic.stubs > 0 {
        let index = synthetic_section(
            &mut builder,
            input_sections,
            b"__TEXT",
            if config.is_arm64e() {
                b"__auth_stubs"
            } else {
                b"__stubs"
            },
            S_SYMBOL_STUBS | S_ATTR_PURE_INSTRUCTIONS | S_ATTR_SOME_INSTRUCTIONS,
            SectionKind::Stubs,
            synthetic.stubs.saturating_mul(config.stub_size()),
            if config.is_arm64() { 2 } else { 0 },
        );
        if let Some(section) = builder.sections.get_mut(index) {
            section.reserved2 = u32::try_from(config.stub_size()).unwrap_or(0);
        }
    }
    if synthetic.got > 0 {
        synthetic_section(
            &mut builder,
            input_sections,
            got_segment,
            b"__got",
            S_NON_LAZY_SYMBOL_POINTERS,
            SectionKind::Got,
            synthetic.got.saturating_mul(8),
            3,
        );
    }
    if synthetic.auth_got > 0 {
        synthetic_section(
            &mut builder,
            input_sections,
            got_segment,
            b"__auth_got",
            S_NON_LAZY_SYMBOL_POINTERS,
            SectionKind::AuthGot,
            synthetic.auth_got.saturating_mul(8),
            3,
        );
    }
    if synthetic.thread_ptrs > 0 {
        synthetic_section(
            &mut builder,
            input_sections,
            b"__DATA",
            b"__thread_ptrs",
            S_THREAD_LOCAL_VARIABLE_POINTERS,
            SectionKind::ThreadPtrs,
            synthetic.thread_ptrs.saturating_mul(8),
            3,
        );
    }
    if !commons.is_empty() {
        let index = synthetic_section(
            &mut builder,
            input_sections,
            b"__DATA",
            b"__common",
            S_ZEROFILL,
            SectionKind::Common,
            0,
            0,
        );
        if let Some(section) = builder.sections.get_mut(index) {
            let mut size = section.size;
            for &(symbol_size, align) in commons {
                let offset = align_up(size, 1u64.checked_shl(align).unwrap_or(1));
                size = offset.saturating_add(symbol_size);
                section.align = section.align.max(align);
            }
            section.size = size;
        }
    }
    for (index, (segname, sectname, size)) in sectcreate.iter().enumerate() {
        synthetic_section(
            &mut builder,
            input_sections,
            segname,
            sectname,
            S_REGULAR,
            SectionKind::Sectcreate(index),
            *size,
            0,
        );
    }
    if synthetic.unwind_info > 0 {
        synthetic_section(
            &mut builder,
            input_sections,
            b"__TEXT",
            b"__unwind_info",
            S_REGULAR,
            SectionKind::UnwindInfo,
            synthetic.unwind_info,
            2,
        );
    }
    if synthetic.eh_frame > 0 {
        synthetic_section(
            &mut builder,
            input_sections,
            b"__TEXT",
            b"__eh_frame",
            // S_COALESCED | S_ATTR_NO_TOC | S_ATTR_STRIP_STATIC_SYMS, as the
            // compilers mark it.
            0x6000_000b,
            SectionKind::EhFrame,
            synthetic.eh_frame,
            3,
        );
    }

    // Common symbol offsets.
    let mut common_offset = Vec::with_capacity(commons.len());
    {
        let mut size = 0u64;
        for &(symbol_size, align) in commons {
            let offset = align_up(size, 1u64.checked_shl(align).unwrap_or(1));
            common_offset.push(offset);
            size = offset.saturating_add(symbol_size);
        }
    }

    // Segments.
    let mut segment_names: Vec<Vec<u8>> = Vec::new();
    if config.pagezero > 0 {
        segment_names.push(b"__PAGEZERO".to_vec());
    }
    segment_names.push(b"__TEXT".to_vec());
    for section in &builder.sections {
        if !segment_names.contains(&section.segname) {
            segment_names.push(section.segname.clone());
        }
    }
    segment_names.push(b"__LINKEDIT".to_vec());
    let mut indexed: Vec<(usize, Vec<u8>)> = segment_names.into_iter().enumerate().collect();
    indexed.sort_by_key(|(index, name)| (segment_order(name), *index));
    let segments: Vec<OutSegment> = indexed
        .into_iter()
        .map(|(_, name)| OutSegment {
            maxprot: protection(&name),
            initprot: protection(&name),
            flags: if name == b"__DATA_CONST" {
                SG_READ_ONLY
            } else {
                0
            },
            name,
            ..OutSegment::default()
        })
        .collect();

    // Sort sections by segment, then by section order.
    let mut order: Vec<usize> = (0..builder.sections.len()).collect();
    order.sort_by_key(|&index| {
        let section = &builder.sections[index];
        let segment = segments
            .iter()
            .position(|s| s.name == section.segname)
            .unwrap_or(usize::MAX);
        (segment, section_order(section))
    });
    let mut remap = vec![0usize; builder.sections.len()];
    for (new, &old) in order.iter().enumerate() {
        if let Some(slot) = remap.get_mut(old) {
            *slot = new;
        }
    }
    let mut sections: Vec<OutSection> = order
        .iter()
        .filter_map(|&old| builder.sections.get(old).cloned())
        .collect();
    let mut segments = segments;
    for (index, section) in sections.iter_mut().enumerate() {
        let segment = segments
            .iter()
            .position(|s| s.name == section.segname)
            .unwrap_or(0);
        section.segment = segment;
        if let Some(segment) = segments.get_mut(segment) {
            segment.sections.push(index);
        }
    }
    for slot in &mut atom_section {
        if *slot != NONE
            && let Some(&new) = remap.get(usize::try_from(*slot).unwrap_or(usize::MAX))
        {
            *slot = u32::try_from(new).unwrap_or(NONE);
        }
    }

    let members = order
        .iter()
        .map(|&old| ordered.get(old).cloned().unwrap_or_default())
        .collect();

    Ok(Layout {
        sections,
        segments,
        atom_section,
        atom_offset,
        common_offset,
        header_size: 0,
        members,
        islands: Vec::new(),
    })
}

impl Layout {
    /// Places the atoms of code section `section` again, leaving an island
    /// of `size` bytes after member `after` for each `(after, size)` of
    /// `islands` (sorted by `after`). Replaces the section's islands.
    ///
    /// # Errors
    ///
    /// [`Error::Limit`] when the section grows past 2^64.
    pub fn place_islands(
        &mut self,
        link: &Link<'_>,
        section: usize,
        islands: &[(usize, u64)],
    ) -> Result<()> {
        let Some(members) = self.members.get(section).cloned() else {
            return Ok(());
        };
        self.islands.retain(|i| i.section != section);
        let mut size = 0u64;
        let mut next_island = islands.iter().peekable();
        for (position, &atom) in members.iter().enumerate() {
            let (file, local) = atom_location(link, atom);
            let Some(info) = link.object(file).and_then(|o| o.atoms.atoms().get(local)) else {
                continue;
            };
            let alignment = 1u64.checked_shl(info.align).unwrap_or(1);
            let offset = align_up(size, alignment);
            if let Some(slot) = self.atom_offset.get_mut(atom) {
                *slot = offset;
            }
            size = offset
                .checked_add(info.size)
                .ok_or_else(|| Error::Limit("output section larger than 2^64".into()))?;
            while let Some(&&(after, island_size)) = next_island.peek() {
                if after != position {
                    break;
                }
                next_island.next();
                let offset = align_up(size, 4);
                self.islands.push(Island {
                    section,
                    offset,
                    size: island_size,
                });
                size = offset
                    .checked_add(island_size)
                    .ok_or_else(|| Error::Limit("output section larger than 2^64".into()))?;
            }
        }
        if let Some(out) = self.sections.get_mut(section) {
            out.size = size;
            out.align = out.align.max(2);
        }
        Ok(())
    }

    /// Assigns addresses and file offsets. `header_size` is the size of the
    /// Mach-O header, the load commands and the header padding.
    ///
    /// # Errors
    ///
    /// [`Error::Limit`] when the image does not fit the address space.
    pub fn assign_addresses(&mut self, link: &Link<'_>, header_size: u64) -> Result<()> {
        let config = link.config;
        let page = config.page_size;
        self.header_size = header_size;
        let overflow = || Error::Limit("the image does not fit the address space".into());
        let mut addr = 0u64;
        let mut file = 0u64;
        for segment_index in 0..self.segments.len() {
            let Some(segment) = self.segments.get_mut(segment_index) else {
                continue;
            };
            if segment.name == b"__PAGEZERO" {
                segment.vmaddr = 0;
                segment.vmsize = config.pagezero;
                addr = config.image_base.max(config.pagezero);
                continue;
            }
            if segment.name == b"__LINKEDIT" {
                segment.vmaddr = addr;
                segment.fileoff = file;
                continue;
            }
            segment.vmaddr = addr;
            segment.fileoff = file;
            let (mut seg_addr, mut seg_file) = (addr, file);
            if segment.name == b"__TEXT" {
                seg_addr = seg_addr.checked_add(header_size).ok_or_else(overflow)?;
                seg_file = seg_file.checked_add(header_size).ok_or_else(overflow)?;
            }
            let section_list = segment.sections.clone();
            for &section_index in &section_list {
                let Some(section) = self.sections.get_mut(section_index) else {
                    continue;
                };
                let alignment = 1u64.checked_shl(section.align).unwrap_or(1);
                seg_addr = align_up(seg_addr, alignment);
                section.addr = seg_addr;
                seg_addr = seg_addr.checked_add(section.size).ok_or_else(overflow)?;
                if section.is_zerofill() {
                    section.offset = 0;
                } else {
                    seg_file = align_up(seg_file, alignment);
                    section.offset = seg_file;
                    seg_file = seg_file.checked_add(section.size).ok_or_else(overflow)?;
                }
            }
            let Some(segment) = self.segments.get_mut(segment_index) else {
                continue;
            };
            segment.vmsize = align_up(seg_addr.saturating_sub(segment.vmaddr), page);
            segment.filesize = align_up(seg_file.saturating_sub(segment.fileoff), page);
            // A segment with no file contents (only zero-fill sections)
            // takes no file space.
            if seg_file == segment.fileoff {
                segment.filesize = 0;
            }
            addr = segment
                .vmaddr
                .checked_add(segment.vmsize)
                .ok_or_else(overflow)?;
            file = segment
                .fileoff
                .checked_add(segment.filesize)
                .ok_or_else(overflow)?;
        }
        Ok(())
    }

    /// The address of atom `atom` (global numbering), if placed.
    #[must_use]
    pub fn atom_address(&self, atom: usize) -> Option<u64> {
        let section = *self.atom_section.get(atom)?;
        if section == NONE {
            return None;
        }
        let section = self.sections.get(usize::try_from(section).ok()?)?;
        Some(section.addr.saturating_add(*self.atom_offset.get(atom)?))
    }

    /// The first section of `kind`.
    #[must_use]
    pub fn find(&self, kind: SectionKind) -> Option<&OutSection> {
        self.sections.iter().find(|s| s.kind == kind)
    }

    /// The section named `segname,sectname`.
    #[must_use]
    pub fn by_name(&self, segname: &[u8], sectname: &[u8]) -> Option<&OutSection> {
        self.sections
            .iter()
            .find(|s| s.segname == segname && s.sectname == sectname)
    }

    /// The segment named `name`.
    #[must_use]
    pub fn segment(&self, name: &[u8]) -> Option<&OutSegment> {
        self.segments.iter().find(|s| s.name == name)
    }

    /// Start of the thread-local template (`__thread_data` or
    /// `__thread_bss`, whichever comes first).
    #[must_use]
    pub fn tlv_template_start(&self) -> Option<u64> {
        self.sections
            .iter()
            .filter(|s| {
                matches!(
                    s.flags & SECTION_TYPE,
                    S_THREAD_LOCAL_REGULAR | S_THREAD_LOCAL_ZEROFILL
                )
            })
            .map(|s| s.addr)
            .min()
    }

    /// The segment index and offset within it of address `addr`, for fixup
    /// encodings.
    #[must_use]
    pub fn segment_of(&self, addr: u64) -> Option<(usize, u64)> {
        self.segments.iter().enumerate().find_map(|(index, s)| {
            (addr >= s.vmaddr
                && addr < s.vmaddr.saturating_add(s.vmsize)
                && s.name != b"__PAGEZERO")
                .then(|| (index, addr.saturating_sub(s.vmaddr)))
        })
    }

    /// The file offset of address `addr` in a section with contents.
    #[must_use]
    pub fn file_offset(&self, addr: u64) -> Option<u64> {
        let segment = self.segments.iter().find(|s| {
            s.filesize > 0 && addr >= s.vmaddr && addr < s.vmaddr.saturating_add(s.filesize)
        })?;
        Some(
            segment
                .fileoff
                .saturating_add(addr.saturating_sub(segment.vmaddr)),
        )
    }
}
