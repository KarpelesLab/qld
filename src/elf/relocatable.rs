//! Relocatable output (`-r`).
//!
//! A relocatable link combines objects into one object: nothing gets an
//! address, relocations are rewritten rather than applied, and symbols stay
//! symbols. The pipeline shares input collection, resolution, COMDAT
//! deduplication and (only when asked) `--gc-sections` with final links; this
//! module then plans and writes the output.
//!
//! # Sections
//!
//! Input sections with the same name, type class (`SHT_NOBITS` joins
//! `SHT_PROGBITS`) and allocation are concatenated into one output section,
//! in order of first appearance. Members of a kept COMDAT group stay in
//! sections of their own, listed by an output `SHT_GROUP` section (all group
//! sections come first, as with GNU ld); `SHF_LINK_ORDER` sections are never
//! combined, and one whose linked-to section is gone is dropped. Sections a
//! final link consumes are kept: `SHF_EXCLUDE` sections, `.note.GNU-stack`
//! and `.gnu.warning*`. `.note.gnu.property` is merged as in final links.
//! Merged (`SHF_MERGE`) sections are concatenated, not deduplicated, and
//! keep their merge flags only when all inputs agree. `-d` allocates common
//! symbols at the end of `.bss`; otherwise they stay common.
//!
//! # Symbols
//!
//! The table holds a section symbol for every output section, then each
//! file's local symbols (`-x` and `-X` apply, but a local that a relocation
//! or a group names is always kept), then the global symbols by symbol ID:
//! definitions in kept sections, absolute and common symbols, `--defsym`
//! symbols, and undefined symbols something refers to (weak when every
//! reference is). Visibility is the most constraining one seen.
//!
//! # Relocations
//!
//! Each output section gets one `.rela` section holding its members'
//! relocations with offsets moved by the member's position. References to
//! section symbols become references to the output section's symbol with
//! the member offset added to the addend. As GNU ld does, a relocation whose
//! target lies in a discarded section becomes `R_X86_64_NONE` against symbol
//! 0, and is removed altogether from non-allocated (debug) sections.

#![deny(clippy::arithmetic_side_effects)]

use hashbrown::HashMap;
use hashbrown::hash_map::Entry;
use rayon::prelude::*;

use crate::args::{DiscardMode, LinkOptions, StripMode};
use crate::elf::read::consts::x86_64::R_X86_64_NONE;
use crate::elf::read::consts::{
    ELFOSABI_GNU, ET_REL, GRP_COMDAT, SHF_ALLOC, SHF_GROUP, SHF_INFO_LINK, SHF_LINK_ORDER,
    SHF_MERGE, SHF_STRINGS, SHF_TLS, SHF_WRITE, SHN_ABS, SHN_COMMON, SHN_LORESERVE, SHN_UNDEF,
    SHN_XINDEX, SHT_GROUP, SHT_LLVM_ADDRSIG, SHT_NOBITS, SHT_NOTE, SHT_NULL, SHT_PROGBITS, SHT_REL,
    SHT_RELA, SHT_STRTAB, SHT_SYMTAB, SHT_SYMTAB_SHNDX, STB_GLOBAL, STB_LOCAL, STB_WEAK,
    STT_NOTYPE, STT_OBJECT, STT_SECTION, STV_DEFAULT, STV_HIDDEN, STV_INTERNAL, STV_PROTECTED,
};
use crate::elf::read::{RawSymbol, Relocation, Relocations, SectionIndex};
use crate::error::{Error, Result};
use crate::ids::SymbolId;
use crate::output::{ChunkRange, FileMode, OutputFile, OutputOptions};
use crate::symbols::{DefinitionKind, SymbolFlags, SymbolName};

use super::common::Commons;
use super::inputs::{DefsymExpr, ElfInput, parse_defsym};
use super::object::{InputSection, ObjectInput, SectionKind, is_debug_name};
use super::refs::{Def, Refs};
use super::resolve::AUX_COMDAT;
use super::sections::{NONE, Sections};
use super::synth::plan_property_note;

/// Kept COMDAT group copies by signature: `(file, group index)`.
type KeptGroups<'a> = HashMap<&'a [u8], (u32, u32), foldhash::fast::FixedState>;

/// Size of a symbol table entry.
const SYM_SIZE: u64 = 24;
/// Size of an `Elf64_Rela`.
const RELA_SIZE: u64 = 24;
/// Size of a section header.
const SHDR_SIZE: u64 = 64;
/// Size of the ELF header.
const EHDR_SIZE: u64 = 64;

/// Whether relocatable output keeps a section that final links consume
/// ([`SectionKind::Ignored`] sections other than the tables a relocatable
/// link rebuilds).
fn kept_when_ignored(section: &InputSection<'_>, strip_debug: bool) -> bool {
    !matches!(
        section.header.sh_type,
        SHT_NULL
            | SHT_SYMTAB
            | SHT_STRTAB
            | SHT_REL
            | SHT_RELA
            | SHT_GROUP
            | SHT_SYMTAB_SHNDX
            | SHT_LLVM_ADDRSIG
    ) && section.name != b".note.gnu.property"
        && !(strip_debug && !section.is_alloc() && is_debug_name(section.name))
}

/// Marks live the sections only relocatable output copies: `SHF_EXCLUDE`
/// sections, `.note.GNU-stack` and `.gnu.warning*`. Call it before COMDAT
/// deduplication, which may kill some of them again.
pub fn revive_sections(files: &[ElfInput<'_>], sections: &mut Sections, options: &LinkOptions) {
    let strip_debug = options.strip >= StripMode::Debug;
    for (file_index, file) in files.iter().enumerate() {
        let Some(object) = &file.object else {
            continue;
        };
        for (index, section) in object.sections.iter().enumerate() {
            if section.kind != SectionKind::Ignored || !kept_when_ignored(section, strip_debug) {
                continue;
            }
            let Ok(index) = u32::try_from(index) else {
                break;
            };
            if let Some(id) = sections.id(file_index, index)
                && let Some(live) = sections.live.get_mut(id.index())
            {
                *live = true;
            }
        }
    }
}

/// Marks live the ignored sections named `name` (for `--emit-relocs`,
/// which keeps `.note.GNU-stack` as GNU ld does).
pub fn revive_named(files: &[ElfInput<'_>], sections: &mut Sections, name: &[u8]) {
    for (file_index, file) in files.iter().enumerate() {
        let Some(object) = &file.object else {
            continue;
        };
        for (index, section) in object.sections.iter().enumerate() {
            if section.kind != SectionKind::Ignored || section.name != name {
                continue;
            }
            let Ok(index) = u32::try_from(index) else {
                break;
            };
            if let Some(id) = sections.id(file_index, index)
                && let Some(live) = sections.live.get_mut(id.index())
            {
                *live = true;
            }
        }
    }
}

/// What a relocatable link writes.
pub struct RelocatableInput<'r, 'a> {
    /// Options.
    pub options: &'r LinkOptions,
    /// Inputs, symbols and section liveness.
    pub refs: Refs<'r, 'a>,
    /// The common block, when `-d` allocates common symbols.
    pub commons: Option<&'r Commons>,
}

/// What an output section is made of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OutKind {
    /// Input sections (and, for `.bss` with `-d`, the common block).
    Content,
    /// A COMDAT group of `file`: its index in [`ObjectInput::groups`] and the
    /// signature symbol index.
    Group { file: u32, group: u32, symbol: u32 },
    /// The merged `.note.gnu.property`.
    Property,
}

/// How input sections map to output sections.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Key<'a> {
    Named {
        name: &'a [u8],
        sh_type: u32,
        flags: u64,
    },
    Group {
        file: u32,
        group: u32,
        name: &'a [u8],
        sh_type: u32,
    },
    Unique(u32),
}

/// One input section of an output section.
#[derive(Clone, Copy, Debug)]
struct Member {
    file: u32,
    section: u32,
    offset: u64,
    /// Relocations written for it.
    relocs: u64,
    /// Offset of its relocations in the output `.rela` section.
    rela_offset: u64,
}

#[derive(Debug)]
struct OutSection<'a> {
    name: &'a [u8],
    kind: OutKind,
    sh_type: u32,
    flags: u64,
    entsize: u64,
    align: u64,
    /// Merge flags and entry size of the first member, and whether a later
    /// member disagreed.
    merge: (u64, u64, bool),
    /// For `SHF_LINK_ORDER`, the linked-to input section `(file, section)`.
    link_source: Option<(u32, u32)>,
    members: Vec<Member>,
    /// Offset of the common block, for `.bss` with `-d`.
    commons: Option<u64>,
    /// Index (in the output section list) of the group this section is in.
    group: u32,
    size: u64,
    relocs: u64,
    index: u32,
    rela_index: u32,
    offset: u64,
    rela_offset: u64,
    name_offset: u32,
    rela_name_offset: u32,
    /// For group sections, the header indices of the members.
    group_members: Vec<u32>,
}

impl<'a> OutSection<'a> {
    fn new(name: &'a [u8], kind: OutKind, sh_type: u32) -> Self {
        Self {
            name,
            kind,
            sh_type,
            flags: 0,
            entsize: 0,
            align: 1,
            merge: (0, 0, false),
            link_source: None,
            members: Vec::new(),
            commons: None,
            group: NONE,
            size: 0,
            relocs: 0,
            index: 0,
            rela_index: 0,
            offset: 0,
            rela_offset: 0,
            name_offset: 0,
            rela_name_offset: 0,
            group_members: Vec::new(),
        }
    }

    fn has_file_bytes(&self) -> bool {
        self.sh_type != SHT_NOBITS
    }
}

/// Where a symbol of the output table is defined.
#[derive(Clone, Copy, Debug)]
enum Place {
    /// In the output section with this list index.
    Out(u32),
    Absolute,
    Common,
    Undefined,
}

/// A planned global symbol.
#[derive(Clone, Copy, Debug)]
struct Global {
    id: SymbolId,
    place: Place,
    value: u64,
    size: u64,
    info: u8,
    other: u8,
    /// The `@@VERSION` of a default-versioned definition.
    default_version: bool,
}

/// A rewritten relocation's symbol.
#[derive(Clone, Copy, Debug)]
enum SymRef {
    Null,
    Local(u32),
    Section(u32),
    Global(SymbolId),
}

/// What happens to one input relocation.
#[derive(Clone, Copy, Debug)]
enum Rewritten {
    Drop,
    Keep {
        symbol: SymRef,
        r_type: u32,
        addend: i64,
    },
}

/// Per-file plan of local symbols and relocation counts.
#[derive(Debug, Default)]
struct FilePlan {
    /// Kept local symbol indices, ascending.
    locals: Vec<u32>,
    /// Output symbol index of the first kept local.
    base: u32,
    /// String table offset of the first kept local's name.
    names: u64,
    /// Total name bytes, terminators included.
    names_size: u64,
    /// Input section index and relocation count of each copied section
    /// with relocations.
    counts: Vec<(u32, u64)>,
    /// Input group section index to output group section list index.
    groups: Vec<(u32, u32)>,
}

/// The whole output plan.
struct Plan<'a> {
    outs: Vec<OutSection<'a>>,
    kept: KeptGroups<'a>,
    /// Output section list index of each input section ([`NONE`] if not
    /// copied), by section ID.
    assign: Vec<u32>,
    /// Offset of each input section in its output section, by section ID.
    offsets: Vec<u64>,
    files: Vec<FilePlan>,
    globals: Vec<Global>,
    /// Output symbol index of each global symbol, 0 if not written.
    global_index: Vec<u32>,
    first_global: u32,
    symbol_count: u64,
    strtab_size: u64,
    global_names: u64,
    property: Option<Vec<u8>>,
    shstrtab: Vec<u8>,
    /// Name offsets of `.symtab`, `.symtab_shndx`, `.strtab`, `.shstrtab`.
    trailer_names: [u32; 4],
    symtab_index: u32,
    shndx_index: u32,
    strtab_index: u32,
    shstrtab_index: u32,
    section_count: u32,
    symtab_offset: u64,
    shndx_offset: u64,
    strtab_offset: u64,
    shstrtab_offset: u64,
    shoff: u64,
    file_size: u64,
    os_abi: u8,
    /// `e_machine` of the output, taken from the inputs.
    machine: u16,
}

fn align_to(value: u64, align: u64) -> Result<u64> {
    let align = align.max(1);
    value
        .checked_next_multiple_of(align)
        .ok_or_else(|| Error::Limit("relocatable output larger than 2^64 bytes".into()))
}

fn add(a: u64, b: u64) -> Result<u64> {
    a.checked_add(b)
        .ok_or_else(|| Error::Limit("relocatable output larger than 2^64 bytes".into()))
}

fn mul(a: u64, b: u64) -> Result<u64> {
    a.checked_mul(b)
        .ok_or_else(|| Error::Limit("relocatable output larger than 2^64 bytes".into()))
}

fn index_u32(value: usize) -> Result<u32> {
    u32::try_from(value).map_err(|_| Error::Limit("too many sections or symbols".into()))
}

fn slot<T: Copy + Default>(table: &[T], index: usize) -> T {
    table.get(index).copied().unwrap_or_default()
}

/// Writes the relocatable output file.
///
/// # Errors
///
/// Returns I/O errors, [`Error::Malformed`] for broken relocations,
/// [`Error::Unimplemented`] for `SHT_REL` inputs, and [`Error::Limit`] when
/// the output does not fit the ELF format.
pub fn write(input: &RelocatableInput<'_, '_>) -> Result<()> {
    let plan = plan(input)?;
    write_file(input, &plan)
}

#[allow(clippy::too_many_lines)]
fn plan<'a>(input: &RelocatableInput<'_, 'a>) -> Result<Plan<'a>> {
    let refs = &input.refs;
    let files = refs.files;
    let sections = refs.sections;
    let property = plan_property_note(files, input.options);

    // Group sections first.
    let mut outs: Vec<OutSection<'a>> = Vec::new();
    let mut file_groups: Vec<Vec<u32>> = vec![Vec::new(); files.len()];
    let mut file_plans: Vec<FilePlan> = (0..files.len()).map(|_| FilePlan::default()).collect();
    let mut os_abi = 0u8;
    let machine = crate::elf::arch::Arch::of_files(files)
        .unwrap_or_default()
        .machine();
    let mut kept: KeptGroups<'a> =
        HashMap::with_hasher(foldhash::fast::FixedState::with_seed(0x6b65_7074));
    for (file_index, file) in files.iter().enumerate() {
        let Some(object) = &file.object else {
            continue;
        };
        if slot(&sections.base, file_index) == NONE {
            continue;
        }
        if object.elf.elf().header().os_abi == ELFOSABI_GNU {
            os_abi = ELFOSABI_GNU;
        }
        let file_u32 = index_u32(file_index)?;
        let mut comdat = object.elf.groups().filter(|g| match g {
            Ok(group) => group.is_comdat(),
            Err(_) => true,
        });
        let mut outs_of_file = Vec::with_capacity(object.groups.len());
        for (group_index, group) in object.groups.iter().enumerate() {
            let header = comdat
                .next()
                .transpose()?
                .ok_or_else(|| Error::Internal("COMDAT group list mismatch".into()))?;
            let live = group
                .members
                .iter()
                .any(|&m| sections.is_live_in(file_index, m));
            if !live {
                outs_of_file.push(NONE);
                continue;
            }
            let name = object.elf.section_name(&header.header).unwrap_or(b".group");
            let out = index_u32(outs.len())?;
            let mut section = OutSection::new(
                name,
                OutKind::Group {
                    file: file_u32,
                    group: index_u32(group_index)?,
                    symbol: header.signature_symbol,
                },
                SHT_GROUP,
            );
            section.align = 4;
            section.entsize = 4;
            outs.push(section);
            outs_of_file.push(out);
            if let Some(plan) = file_plans.get_mut(file_index) {
                plan.groups.push((header.index, out));
            }
            kept.entry(group.signature)
                .or_insert((file_u32, index_u32(group_index)?));
        }
        if let Some(slot) = file_groups.get_mut(file_index) {
            *slot = outs_of_file;
        }
    }

    // Content sections, in order of first appearance.
    let mut keys: HashMap<Key<'a>, u32, foldhash::fast::FixedState> =
        HashMap::with_hasher(foldhash::fast::FixedState::with_seed(0x0072_656c_6f63));
    let mut assign = vec![NONE; sections.len()];
    let mut property_out = None;
    for (file_index, file) in files.iter().enumerate() {
        let Some(object) = &file.object else {
            continue;
        };
        if slot(&sections.base, file_index) == NONE {
            continue;
        }
        let file_u32 = index_u32(file_index)?;
        for (index, section) in object.sections.iter().enumerate() {
            let index = index_u32(index)?;
            if property.is_some() && property_out.is_none() && section.name == b".note.gnu.property"
            {
                let mut note = OutSection::new(b".note.gnu.property", OutKind::Property, SHT_NOTE);
                note.flags = SHF_ALLOC;
                note.align = 8;
                property_out = Some(index_u32(outs.len())?);
                outs.push(note);
            }
            let Some(id) = sections.id(file_index, index) else {
                continue;
            };
            if !sections.is_live(id) {
                continue;
            }
            let header = &section.header;
            if header.sh_flags & SHF_LINK_ORDER != 0
                && !sections.is_live_in(file_index, header.sh_link)
            {
                continue;
            }
            let group = match section.group.checked_sub(1) {
                Some(g) => file_groups
                    .get(file_index)
                    .and_then(|groups| groups.get(g as usize))
                    .copied()
                    .unwrap_or(NONE),
                None => NONE,
            };
            let type_class = if header.sh_type == SHT_NOBITS {
                SHT_PROGBITS
            } else {
                header.sh_type
            };
            let key = if header.sh_flags & SHF_LINK_ORDER != 0 {
                Key::Unique(id.as_u32())
            } else if group != NONE {
                Key::Group {
                    file: file_u32,
                    group: section.group,
                    name: section.name,
                    sh_type: type_class,
                }
            } else {
                Key::Named {
                    name: section.name,
                    sh_type: type_class,
                    flags: header.sh_flags & (SHF_ALLOC | SHF_TLS),
                }
            };
            let out_index = match keys.entry(key) {
                Entry::Occupied(entry) => *entry.get(),
                Entry::Vacant(entry) => {
                    let out = index_u32(outs.len())?;
                    let mut new = OutSection::new(section.name, OutKind::Content, header.sh_type);
                    new.merge = (
                        header.sh_flags & (SHF_MERGE | SHF_STRINGS),
                        header.sh_entsize,
                        false,
                    );
                    new.group = group;
                    if header.sh_flags & SHF_LINK_ORDER != 0 {
                        new.link_source = Some((file_u32, header.sh_link));
                    }
                    outs.push(new);
                    entry.insert(out);
                    out
                }
            };
            let Some(out) = outs.get_mut(out_index as usize) else {
                continue;
            };
            out.flags |= header.sh_flags & !(SHF_MERGE | SHF_STRINGS | SHF_GROUP);
            if (
                header.sh_flags & (SHF_MERGE | SHF_STRINGS),
                header.sh_entsize,
            ) != (out.merge.0, out.merge.1)
            {
                out.merge.2 = true;
            }
            if out.sh_type == SHT_NOBITS && header.sh_type != SHT_NOBITS {
                out.sh_type = SHT_PROGBITS;
            }
            out.align = out.align.max(header.sh_addralign.max(1));
            out.members.push(Member {
                file: file_u32,
                section: index,
                offset: 0,
                relocs: 0,
                rela_offset: 0,
            });
            if let Some(slot) = assign.get_mut(id.index()) {
                *slot = out_index;
            }
        }
    }
    let mut property = property;
    if property.is_some() && property_out.is_none() {
        let mut note = OutSection::new(b".note.gnu.property", OutKind::Property, SHT_NOTE);
        note.flags = SHF_ALLOC;
        note.align = 8;
        outs.push(note);
    }
    if let Some(note) = outs.iter_mut().find(|o| o.kind == OutKind::Property) {
        note.size = property.as_ref().map_or(0, |p| p.len() as u64);
    } else {
        property = None;
    }

    // Common symbols with `-d`: at the end of `.bss`.
    let mut commons_place = None;
    if let Some(commons) = input.commons.filter(|c| !c.entries.is_empty()) {
        let key = Key::Named {
            name: b".bss",
            sh_type: SHT_PROGBITS,
            flags: SHF_ALLOC,
        };
        let out_index = match keys.get(&key) {
            Some(&out) => out,
            None => {
                let out = index_u32(outs.len())?;
                let mut bss = OutSection::new(b".bss", OutKind::Content, SHT_NOBITS);
                bss.flags = SHF_ALLOC | SHF_WRITE;
                outs.push(bss);
                out
            }
        };
        if let Some(out) = outs.get_mut(out_index as usize) {
            out.commons = Some(0);
            out.align = out.align.max(commons.align.max(1));
        }
        commons_place = Some(out_index);
    }

    // Member offsets and section sizes.
    let mut offsets = vec![0u64; sections.len()];
    for out in &mut outs {
        if out.kind != OutKind::Content {
            continue;
        }
        let (merge_flags, merge_entsize, mixed) = out.merge;
        if !mixed {
            out.flags |= merge_flags;
            out.entsize = merge_entsize;
        }
        if out.group != NONE {
            out.flags |= SHF_GROUP;
        }
        let mut size = 0u64;
        for member in &mut out.members {
            let Some(section) = files
                .get(member.file as usize)
                .and_then(|f| f.object.as_ref())
                .and_then(|o| o.section(member.section))
            else {
                continue;
            };
            let offset = align_to(size, section.header.sh_addralign)?;
            member.offset = offset;
            size = add(offset, section.header.sh_size)?;
            if let Some(id) = sections.id(member.file as usize, member.section)
                && let Some(slot) = offsets.get_mut(id.index())
            {
                *slot = offset;
            }
        }
        if out.commons.is_some()
            && let Some(commons) = input.commons
        {
            let offset = align_to(size, commons.align)?;
            out.commons = Some(offset);
            size = add(offset, commons.size)?;
        }
        out.size = size;
    }
    let commons = match commons_place {
        Some(out) => outs
            .get(out as usize)
            .and_then(|o| o.commons)
            .map(|offset| (out, offset)),
        None => None,
    };

    // Relocations and referenced locals, per file.
    let context = Context {
        refs,
        assign: &assign,
        offsets: &offsets,
        kept: &kept,
    };
    let scanned: Vec<Result<ScanOutput>> = files
        .par_iter()
        .enumerate()
        .map(|(file_index, file)| scan_file(&context, file_index, file, &file_plans))
        .collect();
    for (plan, result) in file_plans.iter_mut().zip(scanned) {
        let (referenced, counts) = result?;
        plan.locals = referenced;
        plan.counts = counts;
    }
    for out in &mut outs {
        let mut total = 0u64;
        for member in &mut out.members {
            let count = file_plans
                .get(member.file as usize)
                .and_then(|p| {
                    p.counts
                        .binary_search_by_key(&member.section, |(s, _)| *s)
                        .ok()
                        .and_then(|at| p.counts.get(at))
                })
                .map_or(0, |(_, c)| *c);
            member.relocs = count;
            member.rela_offset = mul(total, RELA_SIZE)?;
            total = add(total, count)?;
        }
        out.relocs = total;
    }

    // Local symbols.
    let discard = if input.options.strip == StripMode::All {
        DiscardMode::All
    } else {
        input.options.discard
    };
    let kept_locals: Vec<Result<(Vec<u32>, u64)>> = files
        .par_iter()
        .enumerate()
        .zip(&file_plans)
        .map(|((file_index, file), plan)| keep_locals(&context, file_index, file, plan, discard))
        .collect();
    let mut next_symbol = add(1, outs.len() as u64)?;
    let mut names = 1u64;
    for (plan, result) in file_plans.iter_mut().zip(kept_locals) {
        let (locals, names_size) = result?;
        plan.base = u32::try_from(next_symbol)
            .map_err(|_| Error::Limit("more than 2^32 symbols".into()))?;
        plan.names = names;
        plan.names_size = names_size;
        next_symbol = add(next_symbol, locals.len() as u64)?;
        names = add(names, names_size)?;
        plan.locals = locals;
    }
    let first_global =
        u32::try_from(next_symbol).map_err(|_| Error::Limit("more than 2^32 symbols".into()))?;

    // Global symbols.
    let globals = plan_globals(input, &context, commons)?;
    let mut global_index = vec![0u32; refs.symbols.len()];
    let global_names = names;
    for global in &globals {
        let index = u32::try_from(next_symbol)
            .map_err(|_| Error::Limit("more than 2^32 symbols".into()))?;
        if let Some(slot) = global_index.get_mut(global.id.index()) {
            *slot = index;
        }
        next_symbol = add(next_symbol, 1)?;
        names = add(names, global_name_len(refs, global))?;
    }

    // Section header indices.
    let mut next = 1u64;
    for out in &mut outs {
        out.index = index_u32(next as usize)?;
        next = add(next, 1)?;
        if out.relocs > 0 {
            out.rela_index = index_u32(next as usize)?;
            next = add(next, 1)?;
        }
    }
    let symtab_index = index_u32(next as usize)?;
    // symtab, strtab, shstrtab, and maybe symtab_shndx.
    let extended = add(next, 3)? >= u64::from(SHN_LORESERVE);
    let shndx_index = if extended {
        next = add(next, 1)?;
        index_u32(next as usize)?
    } else {
        0
    };
    let strtab_index = index_u32(add(next, 1)? as usize)?;
    let shstrtab_index = index_u32(add(next, 2)? as usize)?;
    let section_count = index_u32(add(next, 3)? as usize)?;

    // Group member lists.
    let header_of: Vec<(u32, u32)> = outs
        .iter()
        .map(|o| (o.index, if o.relocs > 0 { o.rela_index } else { 0 }))
        .collect();
    for out in &mut outs {
        let OutKind::Group { file, group, .. } = out.kind else {
            continue;
        };
        let Some(members) = files
            .get(file as usize)
            .and_then(|f| f.object.as_ref())
            .and_then(|o| o.groups.get(group as usize))
            .map(|g| &g.members)
        else {
            continue;
        };
        for &member in members {
            let Some(id) = sections.id(file as usize, member) else {
                continue;
            };
            let target = slot(&assign, id.index());
            if target == NONE {
                continue;
            }
            let (index, rela) = slot(&header_of, target as usize);
            if !out.group_members.contains(&index) {
                out.group_members.push(index);
                if rela != 0 {
                    out.group_members.push(rela);
                }
            }
        }
        out.size = mul(add(out.group_members.len() as u64, 1)?, 4)?;
    }

    // Section names.
    let mut shstrtab = vec![0u8];
    let mut put_name = |parts: &[&[u8]]| -> Result<u32> {
        let offset = index_u32(shstrtab.len())?;
        for part in parts {
            shstrtab.extend_from_slice(part);
        }
        shstrtab.push(0);
        Ok(offset)
    };
    for out in &mut outs {
        out.name_offset = put_name(&[out.name])?;
        if out.relocs > 0 {
            out.rela_name_offset = put_name(&[b".rela", out.name])?;
        }
    }
    let symtab_name = put_name(&[b".symtab"])?;
    let shndx_name = if extended {
        put_name(&[b".symtab_shndx"])?
    } else {
        0
    };
    let strtab_name = put_name(&[b".strtab"])?;
    let shstrtab_name = put_name(&[b".shstrtab"])?;
    let trailer_names = [symtab_name, shndx_name, strtab_name, shstrtab_name];

    // File offsets.
    let mut offset = EHDR_SIZE;
    for out in &mut outs {
        if out.has_file_bytes() {
            offset = align_to(offset, out.align)?;
            out.offset = offset;
            offset = add(offset, out.size)?;
        } else {
            out.offset = offset;
        }
        if out.relocs > 0 {
            offset = align_to(offset, 8)?;
            out.rela_offset = offset;
            offset = add(offset, mul(out.relocs, RELA_SIZE)?)?;
        }
    }
    let symtab_offset = align_to(offset, 8)?;
    offset = add(symtab_offset, mul(next_symbol, SYM_SIZE)?)?;
    let shndx_offset = align_to(offset, 4)?;
    if extended {
        offset = add(shndx_offset, mul(next_symbol, 4)?)?;
    }
    let strtab_offset = offset;
    offset = add(offset, names)?;
    let shstrtab_offset = offset;
    offset = add(offset, shstrtab.len() as u64)?;
    let shoff = align_to(offset, 8)?;
    let file_size = add(shoff, mul(u64::from(section_count), SHDR_SIZE)?)?;

    Ok(Plan {
        outs,
        kept,
        assign,
        offsets,
        files: file_plans,
        globals,
        global_index,
        first_global,
        symbol_count: next_symbol,
        strtab_size: names,
        global_names,
        property,
        shstrtab,
        trailer_names,
        symtab_index,
        shndx_index,
        strtab_index,
        shstrtab_index,
        section_count,
        symtab_offset,
        shndx_offset,
        strtab_offset,
        shstrtab_offset,
        shoff,
        file_size,
        os_abi,
        machine,
    })
}

/// What relocation rewriting needs.
struct Context<'c, 'r, 'a> {
    refs: &'c Refs<'r, 'a>,
    assign: &'c [u32],
    offsets: &'c [u64],
    kept: &'c KeptGroups<'a>,
}

impl Context<'_, '_, '_> {
    /// The output section list index and member offset of an input section.
    fn placed(&self, file: usize, section: u32) -> Option<(u32, u64)> {
        let id = self.refs.sections.id(file, section)?;
        let out = slot(self.assign, id.index());
        (out != NONE).then(|| (out, slot(self.offsets, id.index())))
    }

    /// For a section of a discarded COMDAT group copy, the placement of the
    /// same-named, same-sized member of the kept copy (GNU ld's
    /// `_bfd_elf_check_kept_section`).
    fn kept_copy(&self, file: usize, section: u32) -> Option<(u32, u64)> {
        let object = self.refs.files.get(file)?.object.as_ref()?;
        let input = object.section(section)?;
        let group = object.groups.get(input.group.checked_sub(1)? as usize)?;
        let &(kept_file, kept_group) = self.kept.get(group.signature)?;
        let kept_object = self.refs.files.get(kept_file as usize)?.object.as_ref()?;
        let kept_group = kept_object.groups.get(kept_group as usize)?;
        kept_group.members.iter().find_map(|&member| {
            let candidate = kept_object.section(member)?;
            (candidate.name == input.name && candidate.header.sh_size == input.header.sh_size)
                .then(|| self.placed(kept_file as usize, member))
                .flatten()
        })
    }
}

/// Rewrites one relocation of a copied section of `file`.
fn rewrite(
    context: &Context<'_, '_, '_>,
    file: usize,
    object: &ObjectInput<'_>,
    rel: &Relocation,
    relocated: &InputSection<'_>,
) -> Result<Rewritten> {
    let discarded = if !relocated.is_alloc() {
        Rewritten::Drop
    } else {
        Rewritten::Keep {
            symbol: SymRef::Null,
            r_type: R_X86_64_NONE,
            addend: 0,
        }
    };
    let index = rel.symbol as usize;
    if index == 0 {
        return Ok(Rewritten::Keep {
            symbol: SymRef::Null,
            r_type: rel.r_type,
            addend: rel.addend,
        });
    }
    if index >= object.first_global {
        let Some(id) = context.refs.global_id(file, index) else {
            return Err(object.malformed(0, format!("relocation symbol index {index}")));
        };
        return Ok(Rewritten::Keep {
            symbol: SymRef::Global(id),
            r_type: rel.r_type,
            addend: rel.addend,
        });
    }
    let symbols = object.elf.symbols();
    let raw = symbols
        .get_raw(index)
        .ok_or_else(|| object.malformed(0, format!("relocation symbol index {index}")))?;
    let (symbol, addend) = match symbols.section(index, &raw)? {
        SectionIndex::Section(section) => match context.placed(file, section) {
            Some((out, offset)) if raw.kind() == STT_SECTION => {
                (SymRef::Section(out), rel.addend.wrapping_add(offset as i64))
            }
            Some(_) => (SymRef::Local(rel.symbol), rel.addend),
            None => {
                // GNU ld points references to a discarded COMDAT copy at the
                // kept copy, except from the unwind tables.
                let pretend = !matches!(relocated.name, b".eh_frame" | b".gcc_except_table");
                match context.kept_copy(file, section).filter(|_| pretend) {
                    Some((out, offset)) => {
                        let value = if raw.kind() == STT_SECTION {
                            0
                        } else {
                            raw.st_value
                        };
                        (
                            SymRef::Section(out),
                            rel.addend
                                .wrapping_add(offset as i64)
                                .wrapping_add(value as i64),
                        )
                    }
                    None => return Ok(discarded),
                }
            }
        },
        SectionIndex::Absolute => (SymRef::Local(rel.symbol), rel.addend),
        _ => (SymRef::Null, rel.addend),
    };
    Ok(Rewritten::Keep {
        symbol,
        r_type: rel.r_type,
        addend,
    })
}

/// The relocations of a copied section.
fn relocations_of<'a>(
    object: &ObjectInput<'a>,
    section: &InputSection<'a>,
) -> Result<Option<crate::elf::read::RelaSlice<'a, crate::elf::read::Elf64Le>>> {
    if section.relocs == 0 {
        return Ok(None);
    }
    let Some(header) = object.section(section.relocs).map(|r| r.header) else {
        return Ok(None);
    };
    match object.elf.relocation_section(section.relocs, &header)? {
        Some(r) => match r.relocations {
            Relocations::Rela(rela) => Ok(Some(rela)),
            Relocations::Rel(_) => Err(Error::Unimplemented(format!(
                "SHT_REL relocations in relocatable output from {}",
                object.source().path.display()
            ))),
        },
        None => Ok(None),
    }
}

type ScanOutput = (Vec<u32>, Vec<(u32, u64)>);

/// Counts the relocations each copied section of `file` keeps, and lists
/// the local symbols relocations and groups refer to.
fn scan_file(
    context: &Context<'_, '_, '_>,
    file_index: usize,
    file: &ElfInput<'_>,
    plans: &[FilePlan],
) -> Result<ScanOutput> {
    let mut referenced = Vec::new();
    let mut counts = Vec::new();
    let Some(object) = &file.object else {
        return Ok((referenced, counts));
    };
    for (index, section) in object.sections.iter().enumerate() {
        let index = index_u32(index)?;
        if context.placed(file_index, index).is_none() {
            continue;
        }
        let Some(relas) = relocations_of(object, section)? else {
            continue;
        };
        let mut count = 0u64;
        for rel in relas.iter() {
            match rewrite(context, file_index, object, &rel, section)? {
                Rewritten::Drop => {}
                Rewritten::Keep { symbol, .. } => {
                    count = count.saturating_add(1);
                    if let SymRef::Local(local) = symbol {
                        referenced.push(local);
                    }
                }
            }
        }
        counts.push((index, count));
    }
    // Local group signatures.
    if let Some(plan) = plans.get(file_index) {
        for &(group_section, _) in &plan.groups {
            let Ok(header) = object.elf.section_header(group_section) else {
                continue;
            };
            if (header.sh_info as usize) < object.first_global && header.sh_info != 0 {
                referenced.push(header.sh_info);
            }
        }
    }
    referenced.sort_unstable();
    referenced.dedup();
    Ok((referenced, counts))
}

/// Where a local symbol of `file` goes, or `None` if its section is not in
/// the output: `(section list index or special, value)`.
fn local_place(
    context: &Context<'_, '_, '_>,
    plan: &FilePlan,
    file_index: usize,
    object: &ObjectInput<'_>,
    index: usize,
    raw: &RawSymbol,
) -> Option<(Place, u64)> {
    match object.elf.symbols().section(index, raw).ok()? {
        SectionIndex::Section(section) => {
            if let Some((out, offset)) = context.placed(file_index, section) {
                return Some((Place::Out(out), raw.st_value.wrapping_add(offset)));
            }
            let at = plan
                .groups
                .binary_search_by_key(&section, |(s, _)| *s)
                .ok()?;
            let (_, out) = plan.groups.get(at)?;
            Some((Place::Out(*out), 0))
        }
        SectionIndex::Absolute => Some((Place::Absolute, raw.st_value)),
        _ => None,
    }
}

/// Picks the local symbols of `file` to write. Returns them with the size
/// of their names.
fn keep_locals(
    context: &Context<'_, '_, '_>,
    file_index: usize,
    file: &ElfInput<'_>,
    plan: &FilePlan,
    discard: DiscardMode,
) -> Result<(Vec<u32>, u64)> {
    let mut kept = Vec::new();
    let mut names = 0u64;
    let Some(object) = &file.object else {
        return Ok((kept, names));
    };
    if slot(&context.refs.sections.base, file_index) == NONE {
        return Ok((kept, names));
    }
    let symbols = object.elf.symbols();
    for index in 1..object.first_global {
        let Some(raw) = symbols.get_raw(index) else {
            break;
        };
        if raw.kind() == STT_SECTION {
            continue;
        }
        let index_u = index_u32(index)?;
        let name = symbols.name(index, &raw)?;
        let referenced = plan.locals.binary_search(&index_u).is_ok();
        let wanted = referenced
            || match discard {
                DiscardMode::All => false,
                DiscardMode::None => true,
                DiscardMode::Default | DiscardMode::Locals => !name.starts_with(b".L"),
            };
        if !wanted || local_place(context, plan, file_index, object, index, &raw).is_none() {
            continue;
        }
        kept.push(index_u);
        names = names.saturating_add(name.len() as u64).saturating_add(1);
    }
    Ok((kept, names))
}

const fn visibility_rank(visibility: u8) -> u8 {
    match visibility {
        STV_INTERNAL => 3,
        STV_HIDDEN => 2,
        STV_PROTECTED => 1,
        _ => 0,
    }
}

/// Plans the global symbols, by symbol ID.
#[allow(clippy::too_many_lines)]
fn plan_globals(
    input: &RelocatableInput<'_, '_>,
    context: &Context<'_, '_, '_>,
    commons: Option<(u32, u64)>,
) -> Result<Vec<Global>> {
    let refs = context.refs;
    let symbols = refs.symbols;

    // The most constraining visibility, and the type of undefined
    // references (the first non-NOTYPE one, by file).
    let per_file: Vec<Vec<(u32, u8, u8)>> = refs
        .files
        .par_iter()
        .enumerate()
        .map(|(file_index, file)| {
            let mut out = Vec::new();
            let Some(object) = &file.object else {
                return out;
            };
            if slot(&refs.sections.base, file_index) == NONE {
                return out;
            }
            let ids = refs
                .resolution
                .symbol_ids(crate::ids::FileId::new(file_index));
            let table = object.elf.symbols();
            for (local, &id) in ids.iter().enumerate() {
                let Some(raw) = local
                    .checked_add(object.first_global)
                    .and_then(|i| table.get_raw(i))
                else {
                    break;
                };
                let undefined_kind = if raw.st_shndx == SHN_UNDEF {
                    raw.kind()
                } else {
                    STT_NOTYPE
                };
                if raw.visibility() != STV_DEFAULT || undefined_kind != STT_NOTYPE {
                    out.push((id.as_u32(), raw.visibility(), undefined_kind));
                }
            }
            out
        })
        .collect();
    let mut visibility = vec![0u8; symbols.len()];
    let mut undefined_kind = vec![STT_NOTYPE; symbols.len()];
    for (id, vis, kind) in per_file.into_iter().flatten() {
        if let Some(slot) = visibility.get_mut(id as usize)
            && visibility_rank(vis) > visibility_rank(*slot)
        {
            *slot = vis;
        }
        if let Some(slot) = undefined_kind.get_mut(id as usize)
            && *slot == STT_NOTYPE
        {
            *slot = kind;
        }
    }

    let defsyms: Vec<(SymbolId, DefsymExpr)> = input
        .options
        .defsym
        .iter()
        .filter_map(|(name, expr)| {
            let id = symbols.lookup(&SymbolName::new(name.as_bytes()))?;
            Some((id, parse_defsym(expr)?))
        })
        .collect();

    let globals: Vec<Option<Global>> = (0..symbols.len())
        .into_par_iter()
        .map(|index| {
            let id = SymbolId::new(index);
            let flags = symbols.flags(id);
            let referenced = flags.contains(SymbolFlags::REFERENCED)
                || flags.contains(SymbolFlags::WEAK_REFERENCED);
            let vis = slot(&visibility, index);
            let undefined = Global {
                id,
                place: Place::Undefined,
                value: 0,
                size: 0,
                info: (if flags.contains(SymbolFlags::REFERENCED) {
                    STB_GLOBAL
                } else {
                    STB_WEAK
                } << 4)
                    | slot(&undefined_kind, index),
                other: vis,
                default_version: false,
            };
            let def = symbols.definition(id);
            match def.kind {
                DefinitionKind::Undefined | DefinitionKind::Lazy => referenced.then_some(undefined),
                DefinitionKind::Shared => None,
                DefinitionKind::Common => {
                    let object = refs.files.get(def.file.index())?.object.as_ref()?;
                    let raw = object
                        .elf
                        .symbols()
                        .get_raw((def.index as usize).checked_add(object.first_global)?)?;
                    let size = def.aux & !AUX_COMDAT;
                    let (place, value, info) = match (commons, input.commons) {
                        (Some((out, base)), Some(block)) => (
                            Place::Out(out),
                            base.wrapping_add(block.offset(id).unwrap_or(0)),
                            (STB_GLOBAL << 4) | STT_OBJECT,
                        ),
                        _ => (Place::Common, raw.st_value, raw.st_info),
                    };
                    Some(Global {
                        id,
                        place,
                        value,
                        size,
                        info,
                        other: (raw.st_other & !3) | vis,
                        default_version: false,
                    })
                }
                DefinitionKind::Regular | DefinitionKind::Weak => {
                    let target = refs.global_target(id, false);
                    let default_version = refs
                        .files
                        .get(def.file.index())
                        .and_then(|f| f.object.as_ref())
                        .is_some_and(|o| o.default_version(def.index as usize).is_some());
                    let raw = target.raw.unwrap_or_default();
                    let defined = |place: Place, value: u64| Global {
                        id,
                        place,
                        value,
                        size: raw.st_size,
                        info: raw.st_info,
                        other: (raw.st_other & !3) | vis,
                        default_version,
                    };
                    match target.def {
                        Def::Section {
                            file,
                            section,
                            value,
                        } => {
                            let (out, offset) = context.placed(file, section)?;
                            Some(defined(Place::Out(out), value.wrapping_add(offset)))
                        }
                        Def::Absolute(value) => Some(defined(Place::Absolute, value)),
                        Def::Linker(_) => {
                            let (_, expr) = defsyms.iter().find(|(d, _)| *d == id)?;
                            let (place, value, kind) = defsym_value(context, expr);
                            Some(Global {
                                id,
                                place,
                                value,
                                size: 0,
                                info: (STB_GLOBAL << 4) | kind,
                                other: vis,
                                default_version: false,
                            })
                        }
                        Def::Undefined { .. } => referenced.then_some(undefined),
                        Def::Common(_) | Def::Shared(_) => None,
                    }
                }
            }
        })
        .collect();
    Ok(globals.into_iter().flatten().collect())
}

/// The place, value and type of a `--defsym` symbol: absolute for a
/// number, next to its target (and of its type) for `symbol+offset`.
fn defsym_value(context: &Context<'_, '_, '_>, expr: &DefsymExpr) -> (Place, u64, u8) {
    let refs = context.refs;
    match expr {
        DefsymExpr::Absolute(value) => (Place::Absolute, *value, STT_NOTYPE),
        DefsymExpr::Symbol(name, offset) => {
            let Some(other) = refs.symbols.lookup(&SymbolName::new(name.as_bytes())) else {
                return (Place::Absolute, 0, STT_NOTYPE);
            };
            let target = refs.global_target(other, false);
            let kind = target.raw.map_or(STT_NOTYPE, |raw| raw.kind());
            match target.def {
                Def::Section {
                    file,
                    section,
                    value,
                } => match context.placed(file, section) {
                    Some((out, base)) => (
                        Place::Out(out),
                        value.wrapping_add(base).wrapping_add_signed(*offset),
                        kind,
                    ),
                    None => (Place::Absolute, 0, kind),
                },
                Def::Absolute(value) => (Place::Absolute, value.wrapping_add_signed(*offset), kind),
                _ => (Place::Absolute, 0, STT_NOTYPE),
            }
        }
    }
}

fn global_name_len(refs: &Refs<'_, '_>, global: &Global) -> u64 {
    let name = refs.symbols.name(global.id);
    let mut len = name.bytes().len();
    if let Some(version) = name.version() {
        len = len.saturating_add(1).saturating_add(version.len());
    } else if global.default_version
        && let Some(version) = default_version(refs, global.id)
    {
        len = len.saturating_add(2).saturating_add(version.len());
    }
    (len as u64).saturating_add(1)
}

fn default_version<'a>(refs: &Refs<'_, 'a>, id: SymbolId) -> Option<&'a [u8]> {
    let def = refs.symbols.definition(id);
    refs.files
        .get(def.file.index())?
        .object
        .as_ref()?
        .default_version(def.index as usize)
}

/// One chunk of the output file.
#[derive(Clone, Copy, Debug)]
enum Chunk {
    Header,
    Group(u32),
    Property,
    Member(u32, u32),
    Rela(u32, u32),
    Symtab,
    Shndx,
    Strtab,
    Shstrtab,
    SectionHeaders,
}

fn write_file<'a>(input: &RelocatableInput<'_, 'a>, plan: &Plan<'a>) -> Result<()> {
    let mut chunks: Vec<(ChunkRange, Chunk)> = vec![(ChunkRange::new(0, EHDR_SIZE), Chunk::Header)];
    for (out_index, out) in plan.outs.iter().enumerate() {
        let out_u32 = index_u32(out_index)?;
        match out.kind {
            OutKind::Group { .. } => {
                chunks.push((ChunkRange::new(out.offset, out.size), Chunk::Group(out_u32)));
            }
            OutKind::Property => {
                chunks.push((ChunkRange::new(out.offset, out.size), Chunk::Property));
            }
            OutKind::Content => {
                for (member_index, member) in out.members.iter().enumerate() {
                    let member_u32 = index_u32(member_index)?;
                    if out.has_file_bytes() {
                        let section = input
                            .refs
                            .files
                            .get(member.file as usize)
                            .and_then(|f| f.object.as_ref())
                            .and_then(|o| o.section(member.section));
                        if let Some(section) = section
                            && !section.is_nobits()
                            && section.header.sh_size > 0
                        {
                            chunks.push((
                                ChunkRange::new(
                                    add(out.offset, member.offset)?,
                                    section.header.sh_size,
                                ),
                                Chunk::Member(out_u32, member_u32),
                            ));
                        }
                    }
                    if member.relocs > 0 {
                        chunks.push((
                            ChunkRange::new(
                                add(out.rela_offset, member.rela_offset)?,
                                mul(member.relocs, RELA_SIZE)?,
                            ),
                            Chunk::Rela(out_u32, member_u32),
                        ));
                    }
                }
            }
        }
    }
    chunks.push((
        ChunkRange::new(plan.symtab_offset, mul(plan.symbol_count, SYM_SIZE)?),
        Chunk::Symtab,
    ));
    if plan.shndx_index != 0 {
        chunks.push((
            ChunkRange::new(plan.shndx_offset, mul(plan.symbol_count, 4)?),
            Chunk::Shndx,
        ));
    }
    chunks.push((
        ChunkRange::new(plan.strtab_offset, plan.strtab_size),
        Chunk::Strtab,
    ));
    chunks.push((
        ChunkRange::new(plan.shstrtab_offset, plan.shstrtab.len() as u64),
        Chunk::Shstrtab,
    ));
    chunks.push((
        ChunkRange::new(plan.shoff, mul(u64::from(plan.section_count), SHDR_SIZE)?),
        Chunk::SectionHeaders,
    ));
    chunks.sort_by_key(|(range, _)| range.offset);
    let ranges: Vec<ChunkRange> = chunks.iter().map(|(range, _)| *range).collect();

    let path = input.options.output_path();
    let options = OutputOptions {
        mode: FileMode::Regular,
        ..OutputOptions::default()
    };
    let mut file = OutputFile::create(&path, plan.file_size, &options)?;
    file.write_chunks(&ranges, |index, out| {
        let Some(&(_, chunk)) = chunks.get(index) else {
            return Err(Error::Internal("chunk index out of range".into()));
        };
        write_chunk(input, plan, chunk, out)
    })?;
    file.finish()?;
    Ok(())
}

fn write_chunk<'a>(
    input: &RelocatableInput<'_, 'a>,
    plan: &Plan<'a>,
    chunk: Chunk,
    out: &mut [u8],
) -> Result<()> {
    match chunk {
        Chunk::Header => {
            write_header(plan, out);
            Ok(())
        }
        Chunk::Group(index) => {
            let Some(section) = plan.outs.get(index as usize) else {
                return Ok(());
            };
            let words = std::iter::once(GRP_COMDAT).chain(section.group_members.iter().copied());
            for (word, dest) in words.zip(out.as_chunks_mut::<4>().0.iter_mut()) {
                *dest = word.to_le_bytes();
            }
            Ok(())
        }
        Chunk::Property => {
            if let Some(note) = &plan.property
                && let Some(dest) = out.get_mut(..note.len())
            {
                dest.copy_from_slice(note);
            }
            Ok(())
        }
        Chunk::Member(out_index, member) => {
            let Some(member) = plan
                .outs
                .get(out_index as usize)
                .and_then(|o| o.members.get(member as usize))
            else {
                return Ok(());
            };
            let Some(object) = input
                .refs
                .files
                .get(member.file as usize)
                .and_then(|f| f.object.as_ref())
            else {
                return Ok(());
            };
            let Some(section) = object.section(member.section) else {
                return Ok(());
            };
            let data = object.section_data(section)?;
            if let Some(dest) = out.get_mut(..data.len()) {
                dest.copy_from_slice(data);
            }
            Ok(())
        }
        Chunk::Rela(out_index, member) => write_rela(input, plan, out_index, member, out),
        Chunk::Symtab => write_symtab(input, plan, out),
        Chunk::Shndx => {
            write_shndx(input, plan, out);
            Ok(())
        }
        Chunk::Strtab => {
            write_strtab(input, plan, out);
            Ok(())
        }
        Chunk::Shstrtab => {
            if let Some(dest) = out.get_mut(..plan.shstrtab.len()) {
                dest.copy_from_slice(&plan.shstrtab);
            }
            Ok(())
        }
        Chunk::SectionHeaders => {
            write_section_headers(input, plan, out);
            Ok(())
        }
    }
}

fn write_header(plan: &Plan<'_>, out: &mut [u8]) {
    let Some(header) = out.first_chunk_mut::<64>() else {
        return;
    };
    header.fill(0);
    header[..4].copy_from_slice(b"\x7fELF");
    header[4] = 2; // ELFCLASS64
    header[5] = 1; // ELFDATA2LSB
    header[6] = 1; // EV_CURRENT
    header[7] = plan.os_abi;
    header[16..18].copy_from_slice(&ET_REL.to_le_bytes());
    header[18..20].copy_from_slice(&plan.machine.to_le_bytes());
    header[20..24].copy_from_slice(&1u32.to_le_bytes());
    header[40..48].copy_from_slice(&plan.shoff.to_le_bytes());
    header[52..54].copy_from_slice(&64u16.to_le_bytes());
    header[58..60].copy_from_slice(&64u16.to_le_bytes());
    let (shnum, shstrndx) = if plan.section_count >= u32::from(SHN_LORESERVE) {
        (0, SHN_XINDEX)
    } else {
        (
            u16::try_from(plan.section_count).unwrap_or(0),
            u16::try_from(plan.shstrtab_index).unwrap_or(SHN_XINDEX),
        )
    };
    header[60..62].copy_from_slice(&shnum.to_le_bytes());
    header[62..64].copy_from_slice(&shstrndx.to_le_bytes());
}

#[allow(clippy::too_many_arguments)]
fn put_shdr(
    out: &mut [u8],
    name: u32,
    sh_type: u32,
    flags: u64,
    offset: u64,
    size: u64,
    link: u32,
    info: u32,
    align: u64,
    entsize: u64,
) {
    let Some(entry) = out.first_chunk_mut::<64>() else {
        return;
    };
    entry[0..4].copy_from_slice(&name.to_le_bytes());
    entry[4..8].copy_from_slice(&sh_type.to_le_bytes());
    entry[8..16].copy_from_slice(&flags.to_le_bytes());
    entry[16..24].fill(0);
    entry[24..32].copy_from_slice(&offset.to_le_bytes());
    entry[32..40].copy_from_slice(&size.to_le_bytes());
    entry[40..44].copy_from_slice(&link.to_le_bytes());
    entry[44..48].copy_from_slice(&info.to_le_bytes());
    entry[48..56].copy_from_slice(&align.to_le_bytes());
    entry[56..64].copy_from_slice(&entsize.to_le_bytes());
}

/// The output symbol index of a group's signature.
fn group_signature_index<'a>(
    input: &RelocatableInput<'_, 'a>,
    plan: &Plan<'a>,
    file: u32,
    symbol: u32,
) -> u32 {
    let refs = &input.refs;
    let Some(object) = refs
        .files
        .get(file as usize)
        .and_then(|f| f.object.as_ref())
    else {
        return 0;
    };
    if symbol as usize >= object.first_global {
        return refs
            .global_id(file as usize, symbol as usize)
            .map_or(0, |id| slot(&plan.global_index, id.index()));
    }
    let symbols = object.elf.symbols();
    let Some(raw) = symbols.get_raw(symbol as usize) else {
        return 0;
    };
    if raw.kind() == STT_SECTION {
        if let Ok(SectionIndex::Section(section)) = symbols.section(symbol as usize, &raw)
            && let Some(id) = refs.sections.id(file as usize, section)
        {
            let out = slot(&plan.assign, id.index());
            if out != NONE {
                return out.saturating_add(1);
            }
        }
        return 0;
    }
    local_index(plan, file as usize, symbol)
}

fn local_index(plan: &Plan<'_>, file: usize, symbol: u32) -> u32 {
    let Some(file_plan) = plan.files.get(file) else {
        return 0;
    };
    match file_plan.locals.binary_search(&symbol) {
        Ok(at) => u32::try_from(at)
            .ok()
            .and_then(|at| file_plan.base.checked_add(at))
            .unwrap_or(0),
        Err(_) => 0,
    }
}

fn write_section_headers<'a>(input: &RelocatableInput<'_, 'a>, plan: &Plan<'a>, out: &mut [u8]) {
    out.fill(0);
    let mut entries = out.as_chunks_mut::<64>().0.iter_mut();
    if let Some(first) = entries.next()
        && plan.section_count >= u32::from(SHN_LORESERVE)
    {
        first[32..40].copy_from_slice(&u64::from(plan.section_count).to_le_bytes());
        first[40..44].copy_from_slice(&plan.shstrtab_index.to_le_bytes());
    }
    for out_section in &plan.outs {
        let Some(entry) = entries.next() else {
            return;
        };
        let (link, info) = match out_section.kind {
            OutKind::Group { file, symbol, .. } => (
                plan.symtab_index,
                group_signature_index(input, plan, file, symbol),
            ),
            _ => {
                let link = out_section
                    .link_source
                    .and_then(|(file, section)| input.refs.sections.id(file as usize, section))
                    .map(|id| slot(&plan.assign, id.index()))
                    .filter(|&o| o != NONE)
                    .and_then(|o| plan.outs.get(o as usize))
                    .map_or(0, |o| o.index);
                (link, 0)
            }
        };
        put_shdr(
            entry,
            out_section.name_offset,
            out_section.sh_type,
            out_section.flags,
            out_section.offset,
            out_section.size,
            link,
            info,
            out_section.align,
            out_section.entsize,
        );
        if out_section.relocs > 0 {
            let Some(entry) = entries.next() else {
                return;
            };
            let group = if out_section.group == NONE {
                0
            } else {
                SHF_GROUP
            };
            put_shdr(
                entry,
                out_section.rela_name_offset,
                SHT_RELA,
                SHF_INFO_LINK | group,
                out_section.rela_offset,
                out_section.relocs.saturating_mul(RELA_SIZE),
                plan.symtab_index,
                out_section.index,
                8,
                RELA_SIZE,
            );
        }
    }
    let [symtab_name, shndx_name, strtab_name, shstrtab_name] = plan.trailer_names;
    if let Some(entry) = entries.next() {
        put_shdr(
            entry,
            symtab_name,
            SHT_SYMTAB,
            0,
            plan.symtab_offset,
            plan.symbol_count.saturating_mul(SYM_SIZE),
            plan.strtab_index,
            plan.first_global,
            8,
            SYM_SIZE,
        );
    }
    if plan.shndx_index != 0
        && let Some(entry) = entries.next()
    {
        put_shdr(
            entry,
            shndx_name,
            SHT_SYMTAB_SHNDX,
            0,
            plan.shndx_offset,
            plan.symbol_count.saturating_mul(4),
            plan.symtab_index,
            0,
            4,
            4,
        );
    }
    if let Some(entry) = entries.next() {
        put_shdr(
            entry,
            strtab_name,
            SHT_STRTAB,
            0,
            plan.strtab_offset,
            plan.strtab_size,
            0,
            0,
            1,
            0,
        );
    }
    if let Some(entry) = entries.next() {
        put_shdr(
            entry,
            shstrtab_name,
            SHT_STRTAB,
            0,
            plan.shstrtab_offset,
            plan.shstrtab.len() as u64,
            0,
            0,
            1,
            0,
        );
    }
}

fn write_rela<'a>(
    input: &RelocatableInput<'_, 'a>,
    plan: &Plan<'a>,
    out_index: u32,
    member_index: u32,
    out: &mut [u8],
) -> Result<()> {
    let Some(member) = plan
        .outs
        .get(out_index as usize)
        .and_then(|o| o.members.get(member_index as usize))
    else {
        return Ok(());
    };
    let refs = &input.refs;
    let file = member.file as usize;
    let Some(object) = refs.files.get(file).and_then(|f| f.object.as_ref()) else {
        return Ok(());
    };
    let Some(section) = object.section(member.section) else {
        return Ok(());
    };
    let Some(relas) = relocations_of(object, section)? else {
        return Ok(());
    };
    let context = Context {
        refs,
        assign: &plan.assign,
        offsets: &plan.offsets,
        kept: &plan.kept,
    };
    let mut entries = out.as_chunks_mut::<24>().0.iter_mut();
    let mut written = 0u64;
    for rel in relas.iter() {
        let Rewritten::Keep {
            symbol,
            r_type,
            addend,
        } = rewrite(&context, file, object, &rel, section)?
        else {
            continue;
        };
        let index = match symbol {
            SymRef::Null => Some(0),
            SymRef::Local(local) => Some(local_index(plan, file, local)).filter(|&i| i != 0),
            SymRef::Section(out) => Some(out.saturating_add(1)),
            SymRef::Global(id) => Some(slot(&plan.global_index, id.index())).filter(|&i| i != 0),
        };
        let (index, r_type, addend) = match index {
            Some(index) => (index, r_type, addend),
            None => (0, R_X86_64_NONE, 0),
        };
        let Some(entry) = entries.next() else {
            return Err(Error::Internal(
                "relocatable output: more relocations than planned".into(),
            ));
        };
        let offset = rel.offset.wrapping_add(member.offset);
        let info = (u64::from(index) << 32) | u64::from(r_type);
        entry[0..8].copy_from_slice(&offset.to_le_bytes());
        entry[8..16].copy_from_slice(&info.to_le_bytes());
        entry[16..24].copy_from_slice(&addend.to_le_bytes());
        written = written.saturating_add(1);
    }
    if written != member.relocs {
        return Err(Error::Internal(
            "relocatable output: relocation count changed after planning".into(),
        ));
    }
    Ok(())
}

/// The 16-bit section index field for output section list index `out`.
fn shndx_of(plan: &Plan<'_>, place: Place) -> (u16, u32) {
    match place {
        Place::Out(out) => {
            let index = plan.outs.get(out as usize).map_or(0, |o| o.index);
            match u16::try_from(index) {
                Ok(i) if i < SHN_LORESERVE => (i, 0),
                _ => (SHN_XINDEX, index),
            }
        }
        Place::Absolute => (SHN_ABS, 0),
        Place::Common => (SHN_COMMON, 0),
        Place::Undefined => (SHN_UNDEF, 0),
    }
}

fn put_sym(out: &mut [u8], name: u64, info: u8, other: u8, shndx: u16, value: u64, size: u64) {
    let Some(entry) = out.first_chunk_mut::<24>() else {
        return;
    };
    let name = u32::try_from(name).unwrap_or(0);
    entry[0..4].copy_from_slice(&name.to_le_bytes());
    entry[4] = info;
    entry[5] = other;
    entry[6..8].copy_from_slice(&shndx.to_le_bytes());
    entry[8..16].copy_from_slice(&value.to_le_bytes());
    entry[16..24].copy_from_slice(&size.to_le_bytes());
}

/// Splits `out` into the null symbol and section symbols, one slice per
/// file's locals, and the globals.
fn split_symbols<'o>(
    plan: &Plan<'_>,
    out: &'o mut [u8],
    width: usize,
) -> (&'o mut [u8], Vec<&'o mut [u8]>, &'o mut [u8]) {
    let head = plan.outs.len().saturating_add(1).saturating_mul(width);
    let (first, mut rest) = out.split_at_mut(head.min(out.len()));
    let mut files = Vec::with_capacity(plan.files.len());
    for file in &plan.files {
        let len = file.locals.len().saturating_mul(width).min(rest.len());
        let (this, tail) = std::mem::take(&mut rest).split_at_mut(len);
        files.push(this);
        rest = tail;
    }
    (first, files, rest)
}

fn write_symtab<'a>(
    input: &RelocatableInput<'_, 'a>,
    plan: &Plan<'a>,
    out: &mut [u8],
) -> Result<()> {
    let context = Context {
        refs: &input.refs,
        assign: &plan.assign,
        offsets: &plan.offsets,
        kept: &plan.kept,
    };
    let (head, files, globals) = split_symbols(plan, out, 24);
    let mut entries = head.as_chunks_mut::<24>().0.iter_mut();
    if let Some(null) = entries.next() {
        null.fill(0);
    }
    for (out_section, entry) in plan.outs.iter().zip(entries) {
        let shndx = u16::try_from(out_section.index)
            .ok()
            .filter(|&i| i < SHN_LORESERVE)
            .unwrap_or(SHN_XINDEX);
        put_sym(entry, 0, (STB_LOCAL << 4) | STT_SECTION, 0, shndx, 0, 0);
    }
    files.into_par_iter().enumerate().zip(&plan.files).for_each(
        |((file_index, out), file_plan)| {
            let Some(object) = input
                .refs
                .files
                .get(file_index)
                .and_then(|f| f.object.as_ref())
            else {
                return;
            };
            let symbols = object.elf.symbols();
            let mut name = file_plan.names;
            for (&index, entry) in file_plan.locals.iter().zip(out.as_chunks_mut::<24>().0) {
                let Some(raw) = symbols.get_raw(index as usize) else {
                    continue;
                };
                let len = symbols.name(index as usize, &raw).map_or(0, <[u8]>::len);
                let (place, value) = local_place(
                    &context,
                    file_plan,
                    file_index,
                    object,
                    index as usize,
                    &raw,
                )
                .unwrap_or((Place::Absolute, 0));
                let (shndx, _) = shndx_of(plan, place);
                put_sym(
                    entry,
                    name,
                    raw.st_info,
                    raw.st_other,
                    shndx,
                    value,
                    raw.st_size,
                );
                name = name.saturating_add(len as u64).saturating_add(1);
            }
        },
    );
    let mut name = plan.global_names;
    for (global, entry) in plan.globals.iter().zip(globals.as_chunks_mut::<24>().0) {
        let (shndx, _) = shndx_of(plan, global.place);
        put_sym(
            entry,
            name,
            global.info,
            global.other,
            shndx,
            global.value,
            global.size,
        );
        name = name.saturating_add(global_name_len(&input.refs, global));
    }
    Ok(())
}

fn write_shndx<'a>(input: &RelocatableInput<'_, 'a>, plan: &Plan<'a>, out: &mut [u8]) {
    out.fill(0);
    let context = Context {
        refs: &input.refs,
        assign: &plan.assign,
        offsets: &plan.offsets,
        kept: &plan.kept,
    };
    let (head, files, globals) = split_symbols(plan, out, 4);
    for (out_section, entry) in plan
        .outs
        .iter()
        .zip(head.as_chunks_mut::<4>().0.iter_mut().skip(1))
    {
        if out_section.index >= u32::from(SHN_LORESERVE) {
            *entry = out_section.index.to_le_bytes();
        }
    }
    files.into_par_iter().enumerate().zip(&plan.files).for_each(
        |((file_index, out), file_plan)| {
            let Some(object) = input
                .refs
                .files
                .get(file_index)
                .and_then(|f| f.object.as_ref())
            else {
                return;
            };
            let symbols = object.elf.symbols();
            for (&index, entry) in file_plan.locals.iter().zip(out.as_chunks_mut::<4>().0) {
                let Some(raw) = symbols.get_raw(index as usize) else {
                    continue;
                };
                if let Some((place, _)) = local_place(
                    &context,
                    file_plan,
                    file_index,
                    object,
                    index as usize,
                    &raw,
                ) {
                    let (_, extended) = shndx_of(plan, place);
                    *entry = extended.to_le_bytes();
                }
            }
        },
    );
    for (global, entry) in plan.globals.iter().zip(globals.as_chunks_mut::<4>().0) {
        let (_, extended) = shndx_of(plan, global.place);
        *entry = extended.to_le_bytes();
    }
}

fn put_bytes(out: &mut [u8], at: usize, bytes: &[u8]) -> usize {
    let end = at.saturating_add(bytes.len());
    if let Some(dest) = out.get_mut(at..end) {
        dest.copy_from_slice(bytes);
    }
    end
}

fn write_strtab<'a>(input: &RelocatableInput<'_, 'a>, plan: &Plan<'a>, out: &mut [u8]) {
    out.fill(0);
    let refs = &input.refs;
    // Per-file slices of the local names, then the globals.
    let first = usize::try_from(plan.files.first().map_or(1, |f| f.names)).unwrap_or(1);
    let (_, mut rest) = out.split_at_mut(first.min(out.len()));
    let mut slices = Vec::with_capacity(plan.files.len());
    for file in &plan.files {
        let len = usize::try_from(file.names_size)
            .unwrap_or(usize::MAX)
            .min(rest.len());
        let (this, tail) = std::mem::take(&mut rest).split_at_mut(len);
        slices.push(this);
        rest = tail;
    }
    slices
        .into_par_iter()
        .enumerate()
        .zip(&plan.files)
        .for_each(|((file_index, out), file_plan)| {
            let Some(object) = refs.files.get(file_index).and_then(|f| f.object.as_ref()) else {
                return;
            };
            let symbols = object.elf.symbols();
            let mut at = 0usize;
            for &index in &file_plan.locals {
                let name = symbols
                    .get_raw(index as usize)
                    .and_then(|raw| symbols.name(index as usize, &raw).ok())
                    .unwrap_or_default();
                at = put_bytes(out, at, name).saturating_add(1);
            }
        });
    let mut at = 0usize;
    for global in &plan.globals {
        let name = refs.symbols.name(global.id);
        at = put_bytes(rest, at, name.bytes());
        if let Some(version) = name.version() {
            at = put_bytes(rest, at, b"@");
            at = put_bytes(rest, at, version);
        } else if global.default_version
            && let Some(version) = default_version(refs, global.id)
        {
            at = put_bytes(rest, at, b"@@");
            at = put_bytes(rest, at, version);
        }
        at = at.saturating_add(1);
    }
}
