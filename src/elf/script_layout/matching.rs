//! Output section assignment under a linker script (pipeline stage 10a).
//!
//! As in GNU ld, every input section goes to the first input section
//! description, in statement order, whose file and section patterns match
//! it; a match in `/DISCARD/` removes the section. Statements with
//! `ONLY_IF_RO` or `ONLY_IF_RW` are dropped first when the sections their
//! patterns match do not meet the constraint. Linker-generated sections
//! (GOT, PLT, dynamic tables, `COMMON`, ...) are matched by the names GNU ld
//! gives them.
//!
//! Sections nothing matches are *orphans*, placed like `ldelf_place_orphan`
//! does: into a statement of the same name when one exists, else into a new
//! statement after the one holding similar sections (`.text`, `.rodata`,
//! `.tdata`, `.data`, `.bss`, `.interp` for notes, `.comment` for other
//! non-allocated sections), found by name or else by flags. New statements
//! go after the anchor and the symbol assignments that follow it, but before
//! an assignment to `.` that belongs to the next output section; later
//! orphans of the same kind follow the previous one. `--orphan-handling`
//! can warn about, reject or discard them.

#![deny(clippy::arithmetic_side_effects)]

use rayon::prelude::*;

use crate::elf::inputs::ElfInput;
use crate::elf::place::{OutputSection, Placement};
use crate::elf::read::consts::{
    SHF_ALLOC, SHF_COMPRESSED, SHF_EXCLUDE, SHF_EXECINSTR, SHF_GNU_RETAIN, SHF_GROUP,
    SHF_INFO_LINK, SHF_LINK_ORDER, SHF_MERGE, SHF_OS_NONCONFORMING, SHF_STRINGS, SHF_TLS,
    SHF_WRITE, SHT_NOBITS, SHT_NOTE, SHT_PROGBITS, SHT_REL, SHT_RELA,
};
use crate::elf::rules::Synthetic;
use crate::elf::sections::{NONE, Sections, split_per_file};
use crate::ids::SectionId;
use crate::script::{InputSectionDescription, SectionConstraint, SectionFlag};

use super::plan::{Item, LayoutScript, OutputStmt, OverlayRole, Statement};

/// GNU's section flag bits, as computed from ELF headers.
pub mod sec {
    /// `SEC_ALLOC`.
    pub const ALLOC: u32 = 1;
    /// `SEC_LOAD`.
    pub const LOAD: u32 = 2;
    /// `SEC_READONLY`.
    pub const READONLY: u32 = 4;
    /// `SEC_CODE`.
    pub const CODE: u32 = 8;
    /// `SEC_DATA`.
    pub const DATA: u32 = 16;
    /// `SEC_HAS_CONTENTS`.
    pub const HAS_CONTENTS: u32 = 32;
    /// `SEC_THREAD_LOCAL`.
    pub const THREAD_LOCAL: u32 = 64;
    /// `SEC_DEBUGGING`.
    pub const DEBUGGING: u32 = 128;
}

/// The GNU section flags of an ELF section.
#[must_use]
pub fn gnu_flags(sh_flags: u64, sh_type: u32, name: &[u8]) -> u32 {
    let mut flags = 0;
    if sh_type != SHT_NOBITS {
        flags |= sec::HAS_CONTENTS;
    }
    if sh_flags & SHF_ALLOC != 0 {
        flags |= sec::ALLOC;
        if sh_type != SHT_NOBITS {
            flags |= sec::LOAD;
        }
    }
    if sh_flags & SHF_WRITE == 0 {
        flags |= sec::READONLY;
    }
    if sh_flags & SHF_EXECINSTR != 0 {
        flags |= sec::CODE;
    } else if flags & sec::LOAD != 0 {
        flags |= sec::DATA;
    }
    if sh_flags & SHF_TLS != 0 {
        flags |= sec::THREAD_LOCAL;
    }
    if crate::elf::object::is_debug_name(name) || name.starts_with(b".gnu.linkonce.wi.") {
        flags |= sec::DEBUGGING;
    }
    flags
}

/// A linker-generated section's place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SyntheticPlace {
    /// What it is.
    pub kind: Synthetic,
    /// Its output (statement index), or [`NONE`] when discarded.
    pub output: u32,
    /// The input description index within the output.
    pub sub: u16,
}

/// Where a symbol a script refers to is defined, as seen after resolution.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SymbolDef {
    /// In section `section` of `file`, at `value`.
    Section {
        /// The defining file.
        file: usize,
        /// Its section index.
        section: u32,
        /// The symbol's value.
        value: u64,
    },
    /// An absolute value.
    Absolute(u64),
    /// Defined by the linker (`__start_SEC` and friends).
    Linker,
    /// Defined some other way (common or shared symbols): its address is
    /// not available to scripts.
    Other,
    /// Not defined.
    Undefined,
}

/// Script symbol facts found after symbol resolution.
#[derive(Clone, Debug, Default)]
pub struct ResolvedSymbols {
    /// Definitions of the symbols scripts read, sorted by name.
    pub defs: Vec<(Vec<u8>, SymbolDef)>,
    /// For each of [`ScriptPlacement::symbol_names`], whether its
    /// assignments apply (always for plain assignments; for `PROVIDE`, only
    /// when the symbol is referenced and not defined by an input).
    pub needed: Vec<bool>,
    /// For each of [`ScriptPlacement::symbol_names`], whether the symbol
    /// gets hidden visibility.
    pub hidden: Vec<bool>,
}

/// The script-specific part of a [`Placement`].
#[derive(Debug, Default)]
pub struct ScriptPlacement {
    /// Every symbol a script assigns, in statement order; a symbol's index
    /// here is its slot in [`crate::elf::defined::Value::Script`].
    pub symbol_names: Vec<Vec<u8>>,
    /// For each of [`ScriptPlacement::symbol_names`]: whether every
    /// assignment is a `PROVIDE`, and whether the last one hides the symbol.
    pub symbol_kinds: Vec<(bool, bool)>,
    /// For each of [`ScriptPlacement::symbol_names`], the symbol whose type
    /// its last assignment copies ([`crate::script::Expr::type_source`]).
    pub type_sources: Vec<Option<Vec<u8>>>,
    /// The symbols scripts read.
    pub referenced: Vec<Vec<u8>>,
    /// Filled in after placement by [`crate::elf::defined::register`].
    pub resolved: std::sync::OnceLock<ResolvedSymbols>,
    /// The statement list with orphan statements inserted.
    pub statements: Vec<Statement>,
    /// Output statements created for orphans, numbered after the script's.
    pub orphans: Vec<OutputStmt>,
    /// Where linker-generated sections go.
    pub synthetic: Vec<SyntheticPlace>,
    /// Whether each output statement is used (not dropped by a constraint).
    pub enabled: Vec<bool>,
    /// `--orphan-handling` reports: message, and whether it is an error.
    pub reports: Vec<(String, bool)>,
    /// GNU flags seen per output from its input sections.
    pub input_flags: Vec<u32>,
}

impl ScriptPlacement {
    /// The statement of output `index` (script statements, then orphans).
    #[must_use]
    pub fn stmt<'s>(&'s self, script: &'s LayoutScript, index: u32) -> Option<&'s OutputStmt> {
        let index = index as usize;
        match index.checked_sub(script.outputs.len()) {
            None => script.outputs.get(index),
            Some(orphan) => self.orphans.get(orphan),
        }
    }

    /// The place of a linker-generated section.
    #[must_use]
    pub fn synthetic_place(&self, kind: Synthetic) -> Option<SyntheticPlace> {
        self.synthetic
            .iter()
            .find(|p| p.kind == kind)
            .copied()
            .filter(|p| p.output != NONE)
    }
}

/// The names GNU ld gives a linker-generated section. Without dynamic
/// linking, the PLT, its GOT and its relocations hold only IFUNC entries,
/// which GNU ld names `.iplt`, `.igot.plt` and `.rela.iplt`.
#[must_use]
pub fn synthetic_names(kind: Synthetic, dynamic: bool) -> &'static [&'static [u8]] {
    match kind {
        Synthetic::RelaPlt if !dynamic => &[b".rela.iplt"],
        Synthetic::Plt if !dynamic => &[b".iplt"],
        Synthetic::GotPlt if !dynamic => &[b".igot.plt"],
        Synthetic::None | Synthetic::EhFrameEnd | Synthetic::Comment => &[],
        Synthetic::BuildId => &[b".note.gnu.build-id"],
        Synthetic::Interp => &[b".interp"],
        Synthetic::Hash => &[b".hash"],
        Synthetic::GnuHash => &[b".gnu.hash"],
        Synthetic::DynSym => &[b".dynsym"],
        Synthetic::DynStr => &[b".dynstr"],
        Synthetic::VerSym => &[b".gnu.version"],
        Synthetic::VerDef => &[b".gnu.version_d"],
        Synthetic::VerNeed => &[b".gnu.version_r"],
        Synthetic::RelaDyn => &[b".rela.dyn"],
        Synthetic::RelaPlt => &[b".rela.plt"],
        Synthetic::RelrDyn => &[b".relr.dyn"],
        Synthetic::Plt => &[b".plt"],
        Synthetic::PltGot => &[b".plt.got"],
        Synthetic::PltSec => &[b".plt.sec"],
        Synthetic::EhFrameHdr => &[b".eh_frame_hdr"],
        Synthetic::GnuProperty => &[b".note.gnu.property"],
        Synthetic::DynRelro => &[b".data.rel.ro"],
        Synthetic::Dynamic => &[b".dynamic"],
        Synthetic::Got => &[b".got", b".igot"],
        Synthetic::GotPlt => &[b".got.plt"],
        Synthetic::DynBss => &[b".dynbss"],
        Synthetic::Common => &[b"COMMON"],
    }
}

/// Every linker-generated kind placed by name.
pub const SYNTHETIC_KINDS: &[Synthetic] = &[
    Synthetic::BuildId,
    Synthetic::Interp,
    Synthetic::Hash,
    Synthetic::GnuHash,
    Synthetic::DynSym,
    Synthetic::DynStr,
    Synthetic::VerSym,
    Synthetic::VerDef,
    Synthetic::VerNeed,
    Synthetic::RelaDyn,
    Synthetic::RelaPlt,
    Synthetic::RelrDyn,
    Synthetic::Plt,
    Synthetic::PltGot,
    Synthetic::PltSec,
    Synthetic::EhFrameHdr,
    Synthetic::GnuProperty,
    Synthetic::DynRelro,
    Synthetic::Dynamic,
    Synthetic::Got,
    Synthetic::GotPlt,
    Synthetic::DynBss,
    Synthetic::Common,
];

/// Whether a section's flags meet `INPUT_SECTION_FLAGS`.
fn flags_match(required: &[SectionFlag], sh_flags: u64) -> bool {
    required.iter().all(|flag| {
        let bit = match flag.name.as_slice() {
            b"SHF_WRITE" => SHF_WRITE,
            b"SHF_ALLOC" => SHF_ALLOC,
            b"SHF_EXECINSTR" => SHF_EXECINSTR,
            b"SHF_MERGE" => SHF_MERGE,
            b"SHF_STRINGS" => SHF_STRINGS,
            b"SHF_INFO_LINK" => SHF_INFO_LINK,
            b"SHF_LINK_ORDER" => SHF_LINK_ORDER,
            b"SHF_OS_NONCONFORMING" => SHF_OS_NONCONFORMING,
            b"SHF_GROUP" => SHF_GROUP,
            b"SHF_TLS" => SHF_TLS,
            b"SHF_COMPRESSED" => SHF_COMPRESSED,
            b"SHF_GNU_RETAIN" => SHF_GNU_RETAIN,
            b"SHF_EXCLUDE" => SHF_EXCLUDE,
            other => std::str::from_utf8(other)
                .ok()
                .and_then(crate::elf::inputs::parse_number)
                .unwrap_or(0),
        };
        let set = sh_flags & bit == bit && bit != 0;
        set != flag.negated
    })
}

/// One input section description in match order.
struct Desc<'s> {
    output: u32,
    sub: u16,
    keep: bool,
    description: &'s InputSectionDescription,
}

/// The descriptions in match order, indexed for lookup: descriptions that
/// take literal section names from every file (most of a default script)
/// are found by name; the others are tested in order.
struct DescIndex<'s> {
    /// Literal section name to the first such description that names it.
    literal: hashbrown::HashMap<&'s [u8], usize, foldhash::fast::FixedState>,
    /// Positions of the other descriptions, ascending.
    general: Vec<usize>,
}

impl<'s> DescIndex<'s> {
    fn new(descs: &[Desc<'s>]) -> Self {
        let mut literal: hashbrown::HashMap<&'s [u8], usize, _> =
            hashbrown::HashMap::with_hasher(foldhash::fast::FixedState::with_seed(0x6465_7363));
        let mut general = Vec::new();
        for (position, desc) in descs.iter().enumerate() {
            let d = desc.description;
            let indexable = d.flags.is_empty()
                && d.file.exclude.is_empty()
                && d.file.pattern.matches_every_file()
                && d.sections.as_ref().is_some_and(|specs| {
                    specs
                        .iter()
                        .all(|s| s.pattern.is_literal() && s.exclude_files.is_empty())
                });
            match (&d.sections, indexable) {
                (Some(specs), true) => {
                    for spec in specs {
                        literal.entry(spec.pattern.as_bytes()).or_insert(position);
                    }
                }
                _ => general.push(position),
            }
        }
        Self { literal, general }
    }

    /// The first description that `accept`s and matches the section, as a
    /// linear scan of `descs` would find it.
    fn find<'d>(
        &self,
        descs: &'d [Desc<'s>],
        name: &[u8],
        matches: impl Fn(&Desc<'s>) -> bool,
    ) -> Option<&'d Desc<'s>> {
        let literal = self.literal.get(name).copied();
        let general = self
            .general
            .iter()
            .copied()
            .take_while(|&p| literal.is_none_or(|l| p < l))
            .find(|&p| descs.get(p).is_some_and(&matches));
        descs.get(general.or(literal)?)
    }
}

/// The name a description sees for a file: the member name for archive
/// members (with the archive path), else the path.
fn file_names<'a, F: crate::elf::read::ElfFormat>(
    file: &ElfInput<'a, F>,
) -> (&'a [u8], Option<&'a [u8]>) {
    match file.file {
        Some(f) => match f.member() {
            Some(member) => (
                member.as_bytes(),
                Some(f.path().as_os_str().as_encoded_bytes()),
            ),
            None => (f.path().as_os_str().as_encoded_bytes(), None),
        },
        None => (b"", None),
    }
}

fn matches_desc(
    desc: &InputSectionDescription,
    file: &[u8],
    archive: Option<&[u8]>,
    name: &[u8],
    sh_flags: u64,
) -> bool {
    (desc.flags.is_empty() || flags_match(&desc.flags, sh_flags))
        && desc.matches(file, archive, name).is_some()
}

/// The GNU flags an output with no input sections gets from its statement.
fn statement_flags(stmt: &OutputStmt) -> u32 {
    let mut flags = 0;
    for item in &stmt.items {
        if matches!(
            item,
            Item::Data { .. } | Item::Asciz(_) | Item::LinkerVersion
        ) {
            flags |= sec::HAS_CONTENTS | sec::ALLOC | sec::LOAD | sec::READONLY;
        }
    }
    flags
}

/// Where a class of orphans goes: GNU's `orphan_save` table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Hold {
    Text,
    Rodata,
    Tdata,
    Data,
    Bss,
    Rel,
    Interp,
    NonAlloc,
}

impl Hold {
    const ALL: [Hold; 8] = [
        Hold::Text,
        Hold::Rodata,
        Hold::Tdata,
        Hold::Data,
        Hold::Bss,
        Hold::Rel,
        Hold::Interp,
        Hold::NonAlloc,
    ];

    fn name(self) -> Option<&'static [u8]> {
        Some(match self {
            Hold::Text => b".text",
            Hold::Rodata => b".rodata",
            Hold::Tdata => b".tdata",
            Hold::Data => b".data",
            Hold::Bss => b".bss",
            Hold::Rel => return None,
            Hold::Interp => b".interp",
            Hold::NonAlloc => b".comment",
        })
    }

    fn flags(self) -> u32 {
        use sec::*;
        match self {
            Hold::Text => HAS_CONTENTS | ALLOC | LOAD | READONLY | CODE,
            Hold::Rodata | Hold::Rel | Hold::Interp => {
                HAS_CONTENTS | ALLOC | LOAD | READONLY | DATA
            }
            Hold::Tdata => HAS_CONTENTS | ALLOC | LOAD | DATA | THREAD_LOCAL,
            Hold::Data => HAS_CONTENTS | ALLOC | LOAD | DATA,
            Hold::Bss => ALLOC,
            Hold::NonAlloc => HAS_CONTENTS,
        }
    }

    fn index(self) -> usize {
        Hold::ALL.iter().position(|h| *h == self).unwrap_or(0)
    }
}

/// An orphan waiting for placement.
struct Orphan<'a> {
    name: &'a [u8],
    sh_type: u32,
    flags: u32,
    align: u64,
    /// A linker-generated section that may turn out empty: it gets a
    /// statement but does not steer later orphans.
    tentative: bool,
    /// A COMDAT group member of a relocatable link: always a statement of
    /// its own (GNU ld's `SPECIAL` constraint).
    special: bool,
    what: OrphanWhat,
    /// The input file, for `--orphan-handling` messages (`None` for
    /// linker-generated sections).
    file: Option<usize>,
}

#[derive(Clone, Copy)]
enum OrphanWhat {
    Section(SectionId),
    Synthetic(Synthetic),
}

struct Placer<'p, 'a> {
    script: &'p LayoutScript,
    statements: Vec<Statement>,
    /// The outputs of `statements` by name (orphan lookups by name would
    /// otherwise scan every statement for every orphan section).
    by_name: hashbrown::HashMap<Vec<u8>, Vec<u32>, foldhash::fast::FixedState>,
    orphans: Vec<OutputStmt>,
    orphan_names: Vec<&'a [u8]>,
    /// GNU flags of each output: from its inputs (0 when it has none).
    flags: Vec<u32>,
    /// Whether each output has input sections.
    has_input: Vec<bool>,
    /// The ELF type of each output's first input section.
    types: Vec<Option<u32>>,
    enabled: Vec<bool>,
    /// The last statement created for each hold class.
    last: [Option<u32>; 8],
    /// Orphans added to existing statements get this `sub`.
    appended_sub: Vec<u16>,
    executable: bool,
    /// A relocatable link (`-r`).
    relocatable: bool,
    /// Largest input alignment of each output.
    aligns: Vec<u64>,
    /// GNU's `first_orphan_note`.
    first_orphan_note: Option<u32>,
}

impl<'a> Placer<'_, 'a> {
    fn stmt(&self, index: u32) -> Option<&OutputStmt> {
        let index = index as usize;
        match index.checked_sub(self.script.outputs.len()) {
            None => self.script.outputs.get(index),
            Some(orphan) => self.orphans.get(orphan),
        }
    }

    fn name(&self, index: u32) -> &[u8] {
        self.stmt(index).map_or(&[][..], |s| s.name.as_slice())
    }

    /// The first enabled output statement named `name`, in statement order.
    fn find(&self, name: &[u8]) -> Option<u32> {
        self.statements.iter().find_map(|s| match s {
            Statement::Output(i)
                if self.enabled.get(*i as usize).copied().unwrap_or(false)
                    && self.name(*i) == name =>
            {
                Some(*i)
            }
            _ => None,
        })
    }

    fn position_of(&self, output: u32) -> Option<usize> {
        self.statements
            .iter()
            .position(|s| matches!(s, Statement::Output(i) if *i == output))
    }

    /// GNU's `lang_output_section_find_by_flags`, with ELF section type
    /// matching (`bfd_elf_match_sections_by_type`) and its retry without.
    fn find_by_flags(&self, flags: u32, sh_type: u32) -> Option<u32> {
        self.find_by_flags_typed(flags, Some(sh_type))
            .or_else(|| self.find_by_flags_typed(flags, None))
    }

    fn find_by_flags_typed(&self, flags: u32, sh_type: Option<u32>) -> Option<u32> {
        use sec::*;
        let candidates: Vec<(u32, u32, Option<u32>)> = self
            .statements
            .iter()
            .filter_map(|s| match s {
                Statement::Output(i)
                    if self.enabled.get(*i as usize).copied().unwrap_or(false)
                        && self.stmt(*i).is_some_and(|o| !o.is_discard()) =>
                {
                    let f = self.flags.get(*i as usize).copied().unwrap_or(0);
                    let t = self.types.get(*i as usize).copied().flatten();
                    Some((*i, f, t))
                }
                _ => None,
            })
            .collect();
        let type_ok = |t: Option<u32>| match (sh_type, t) {
            (Some(want), Some(have)) => want == have,
            _ => true,
        };
        let mut found = None;
        for &(i, f, t) in &candidates {
            if !type_ok(t) {
                continue;
            }
            if (f ^ flags) & (HAS_CONTENTS | ALLOC | LOAD | READONLY | CODE | THREAD_LOCAL) == 0 {
                found = Some(i);
            }
        }
        if found.is_some() {
            return found;
        }
        if flags & CODE != 0 && flags & ALLOC != 0 {
            for &(i, f, t) in &candidates {
                if type_ok(t)
                    && (f ^ flags) & (HAS_CONTENTS | ALLOC | LOAD | CODE | THREAD_LOCAL) == 0
                {
                    found = Some(i);
                }
            }
        } else if flags & READONLY != 0 && flags & ALLOC != 0 {
            for &(i, f, t) in &candidates {
                if type_ok(t) && (f ^ flags) & (HAS_CONTENTS | ALLOC | LOAD | READONLY) == 0 {
                    found = Some(i);
                }
            }
        } else if flags & THREAD_LOCAL != 0 && flags & ALLOC != 0 {
            let mut seen = false;
            for &(i, f, _) in &candidates {
                let differ = f ^ (flags | LOAD | HAS_CONTENTS);
                if differ & (THREAD_LOCAL | ALLOC) == 0 {
                    if f & LOAD == 0 && flags & LOAD != 0 {
                        break;
                    }
                    found = Some(i);
                    seen = true;
                } else if seen {
                    break;
                } else if differ & (HAS_CONTENTS | ALLOC | LOAD) == 0 {
                    found = Some(i);
                }
            }
            return found;
        } else if flags & HAS_CONTENTS != 0 && flags & ALLOC != 0 {
            for &(i, f, t) in &candidates {
                if type_ok(t) && (f ^ flags) & (HAS_CONTENTS | ALLOC | LOAD | THREAD_LOCAL) == 0 {
                    found = Some(i);
                }
            }
        } else if flags & ALLOC != 0 {
            for &(i, f, t) in &candidates {
                if type_ok(t) && (f ^ flags) & ALLOC == 0 {
                    found = Some(i);
                }
            }
        } else {
            for &(i, f, _) in &candidates {
                if (f ^ flags) & DEBUGGING == 0 {
                    found = Some(i);
                }
            }
            return found;
        }
        found
    }

    /// GNU's `insert_os_after`: the statement position a new output section
    /// statement goes to after output `after`.
    fn insert_position(&self, after: u32) -> usize {
        let Some(start) = self.position_of(after) else {
            return self.statements.len();
        };
        let mut assign: Option<usize> = None;
        let mut position = start.saturating_add(1);
        while let Some(statement) = self.statements.get(position) {
            match statement {
                Statement::Assign { assignment, .. } => {
                    if assign.is_none() && assignment.is_dot() {
                        assign = Some(position);
                    }
                }
                Statement::Assert { .. } => {}
                Statement::Output(next) => {
                    if let Some(at) = assign {
                        let next = *next as usize;
                        let has_input = self.has_input.get(next).copied().unwrap_or(false);
                        let alloc = self.flags.get(next).copied().unwrap_or(0) & sec::ALLOC != 0;
                        if !has_input || alloc {
                            return at;
                        }
                    }
                    return position;
                }
            }
            position = position.saturating_add(1);
        }
        self.statements.len()
    }

    /// Places one orphan; returns its output and `sub`.
    fn place(&mut self, orphan: &Orphan<'a>) -> (u32, u16) {
        use sec::*;
        let mut name = orphan.name;
        if !self.relocatable
            && orphan.flags & ALLOC != 0
            && matches!(orphan.sh_type, SHT_RELA | SHT_REL)
        {
            name = if orphan.sh_type == SHT_RELA {
                b".rela.dyn"
            } else {
                b".rel.dyn"
            };
        }
        // An existing statement of this name with compatible flags, in
        // statement order.
        let mut same_name: Vec<u32> = if orphan.special {
            Vec::new()
        } else {
            self.by_name
                .get(name)
                .map(|list| {
                    list.iter()
                        .copied()
                        .filter(|&i| self.enabled.get(i as usize).copied().unwrap_or(false))
                        .collect()
                })
                .unwrap_or_default()
        };
        if same_name.len() > 1 {
            same_name.sort_by_cached_key(|&i| self.position_of(i));
        }
        for &output in &same_name {
            let f = self.flags.get(output as usize).copied().unwrap_or(0);
            let has_input = self
                .has_input
                .get(output as usize)
                .copied()
                .unwrap_or(false);
            if !has_input || (f ^ orphan.flags) & (LOAD | ALLOC) == 0 {
                return self.append(output, orphan.flags, orphan.tentative);
            }
        }
        // .gnu.warning.SYMBOL goes into .text.
        if self.executable
            && orphan.name.starts_with(b".gnu.warning.")
            && let Some(text) = self.find(b".text")
        {
            return self.append(text, orphan.flags, orphan.tentative);
        }
        let flags = orphan.flags;
        let hold = if flags & (ALLOC | DEBUGGING) == 0 {
            Some(Hold::NonAlloc)
        } else if flags & ALLOC == 0 {
            None
        } else if flags & LOAD != 0 && orphan.sh_type == SHT_NOTE {
            if self.find(b".interp").is_some() {
                Some(Hold::Interp)
            } else if self.find(b".rodata").is_some() {
                Some(Hold::Rodata)
            } else {
                Some(Hold::Text)
            }
        } else if flags & (LOAD | HAS_CONTENTS | THREAD_LOCAL) == 0 {
            Some(Hold::Bss)
        } else if flags & THREAD_LOCAL != 0 {
            Some(Hold::Tdata)
        } else if flags & READONLY == 0 {
            Some(Hold::Data)
        } else if flags & LOAD != 0 && matches!(orphan.sh_type, SHT_RELA | SHT_REL) {
            Some(Hold::Rel)
        } else if flags & CODE == 0 {
            Some(Hold::Rodata)
        } else {
            Some(Hold::Text)
        };
        let mut at = match hold {
            None => self.statements.len(),
            Some(hold) => {
                let slot = hold.index();
                match self.last.get(slot).copied().flatten() {
                    Some(previous) => self
                        .position_of(previous)
                        .map_or(self.statements.len(), |p| p.saturating_add(1)),
                    None => {
                        let anchor = hold
                            .name()
                            .and_then(|n| self.find(n))
                            .or_else(|| match hold {
                                Hold::Rel => self.find_rel(orphan.sh_type == SHT_RELA),
                                _ => None,
                            })
                            .or_else(|| {
                                self.find_by_flags(hold_flags(hold, flags), orphan.sh_type)
                            });
                        match anchor {
                            Some(anchor) => self.insert_position(anchor),
                            None => self.statements.len(),
                        }
                    }
                }
            }
        };
        if hold.is_some() && flags & LOAD != 0 && !orphan.tentative {
            at = self.note_position(orphan, at);
        }
        // The statement: region and program headers follow the anchor.
        let anchor = self.statements.get(..at).and_then(|before| {
            before.iter().rev().find_map(|s| match s {
                Statement::Output(i) => Some(*i),
                _ => None,
            })
        });
        let alloc = flags & (ALLOC | LOAD) != 0;
        let (region, load_region, phdrs) = match anchor.and_then(|a| self.stmt(a)) {
            Some(anchor) if alloc && hold.is_some() => (
                anchor.region.clone(),
                anchor.load_region.clone(),
                anchor.phdrs.clone(),
            ),
            _ => (None, None, Vec::new()),
        };
        let stmt = OutputStmt {
            name: name.to_vec(),
            span: crate::script::Span::default(),
            address: (!alloc).then_some(crate::script::Expr::Number(0)),
            section_type: crate::script::OutputSectionType::Normal,
            load_address: None,
            align: None,
            align_with_input: false,
            subalign: None,
            constraint: SectionConstraint::None,
            items: Vec::new(),
            region,
            load_region,
            phdrs,
            fill: None,
            overlay: OverlayRole::None,
            update_dot: None,
        };
        let index = u32::try_from(self.script.outputs.len().saturating_add(self.orphans.len()))
            .unwrap_or(NONE);
        self.orphans.push(stmt);
        self.orphan_names.push(name_static_or(orphan.name, name));
        self.flags.push(if orphan.tentative { 0 } else { flags });
        self.types
            .push((!orphan.tentative).then_some(orphan.sh_type));
        self.aligns.push(orphan.align);
        self.has_input.push(!orphan.tentative);
        self.enabled.push(true);
        self.appended_sub.push(0);
        let at = at.min(self.statements.len());
        self.statements.insert(at, Statement::Output(index));
        self.by_name.entry(name.to_vec()).or_default().push(index);
        if let Some(hold) = hold
            && let Some(slot) = self.last.get_mut(hold.index())
        {
            *slot = Some(index);
        }
        (index, 0)
    }

    fn is_note(&self, output: u32) -> bool {
        let i = output as usize;
        self.has_input.get(i).copied().unwrap_or(false)
            && self.types.get(i).copied().flatten() == Some(SHT_NOTE)
            && self.flags.get(i).copied().unwrap_or(0) & sec::LOAD != 0
    }

    /// GNU's grouping of loaded note sections in `lang_insert_orphan`:
    /// notes are kept together and sorted by alignment, and other orphans
    /// are not placed among them.
    fn note_position(&mut self, orphan: &Orphan<'_>, at: usize) -> usize {
        let outputs: Vec<(usize, u32)> = self
            .statements
            .iter()
            .enumerate()
            .filter_map(|(p, s)| match s {
                Statement::Output(i)
                    if self.has_input.get(*i as usize).copied().unwrap_or(false) =>
                {
                    Some((p, *i))
                }
                _ => None,
            })
            .collect();
        if orphan.sh_type == SHT_NOTE {
            self.first_orphan_note = None;
            let mut after = None;
            for &(position, output) in &outputs {
                if self.is_note(output) {
                    if self.first_orphan_note.is_none() {
                        self.first_orphan_note = Some(output);
                    }
                    if self.aligns.get(output as usize).copied().unwrap_or(1) >= orphan.align {
                        after = Some((position, output));
                    }
                } else if self.first_orphan_note.is_some() {
                    break;
                }
            }
            return match (after, self.first_orphan_note) {
                (Some((_, output)), _) => self.insert_position(output),
                (None, Some(first)) => self.position_of(first).unwrap_or(at),
                (None, None) => at,
            };
        }
        if self.first_orphan_note.is_none() {
            return at;
        }
        // After the section at the insertion point, or the last note after it.
        let Some(&(_, next)) = outputs.iter().find(|(p, _)| *p >= at) else {
            return at;
        };
        let mut after = next;
        let mut seen = false;
        for &(_, output) in &outputs {
            if output == next {
                seen = true;
                continue;
            }
            if seen && self.is_note(output) {
                after = output;
            }
        }
        self.insert_position(after)
    }

    fn find_rel(&self, rela: bool) -> Option<u32> {
        let prefix: &[u8] = if rela { b".rela" } else { b".rel" };
        let mut found = None;
        for s in &self.statements {
            if let Statement::Output(i) = s
                && self.enabled.get(*i as usize).copied().unwrap_or(false)
                && self.name(*i).starts_with(prefix)
                && (rela || !self.name(*i).starts_with(b".rela"))
            {
                found = Some(*i);
            }
        }
        found
    }

    fn append(&mut self, output: u32, flags: u32, tentative: bool) -> (u32, u16) {
        let sub = self.appended_sub.get(output as usize).copied().unwrap_or(0);
        if tentative {
            return (output, sub);
        }
        if let Some(f) = self.flags.get_mut(output as usize) {
            *f |= flags;
        }
        if let Some(h) = self.has_input.get_mut(output as usize) {
            *h = true;
        }
        (output, sub)
    }
}

fn hold_flags(hold: Hold, flags: u32) -> u32 {
    match hold {
        Hold::Rel | Hold::NonAlloc => hold.flags(),
        _ => flags,
    }
}

/// Orphan names for `.rela.dyn`/`.rel.dyn` are static; others borrow the
/// input section's name.
fn name_static_or<'a>(original: &'a [u8], chosen: &[u8]) -> &'a [u8] {
    if chosen == b".rela.dyn" {
        b".rela.dyn"
    } else if chosen == b".rel.dyn" {
        b".rel.dyn"
    } else {
        original
    }
}

/// Assigns output sections under a script; see the [module
/// documentation](self).
#[allow(clippy::too_many_lines)]
pub fn place<'a, F: crate::elf::read::ElfFormat>(
    script: &'a LayoutScript,
    files: &[ElfInput<'a, F>],
    sections: &Sections,
    options: &crate::args::LinkOptions,
) -> Placement<'a> {
    let output_count = script.outputs.len();
    let total = sections.len();
    let relocatable = options.kind == crate::args::OutputKind::Relocatable;

    // 1. Constraints: a statement whose patterns match a writable section
    // fails ONLY_IF_RO, one matching only read-only sections fails
    // ONLY_IF_RW. Sections claimed earlier count, as in GNU ld.
    let mut enabled = vec![false; output_count];
    for statement in &script.statements {
        if let Statement::Output(i) = statement
            && let Some(slot) = enabled.get_mut(*i as usize)
        {
            *slot = true;
        }
    }
    let constrained: Vec<u32> = script
        .statements
        .iter()
        .filter_map(|s| match s {
            Statement::Output(i)
                if script.outputs.get(*i as usize).is_some_and(|o| {
                    matches!(
                        o.constraint,
                        SectionConstraint::OnlyIfRo | SectionConstraint::OnlyIfRw
                    )
                }) =>
            {
                Some(*i)
            }
            _ => None,
        })
        .collect();
    if !constrained.is_empty() {
        let writable: Vec<bool> = constrained
            .par_iter()
            .map(|&output| {
                let Some(stmt) = script.outputs.get(output as usize) else {
                    return false;
                };
                // Statements that take literal section names from every file
                // (the built-in scripts' `.eh_frame` and friends) only need
                // the names compared.
                let literal: Option<Vec<&[u8]>> = stmt
                    .items
                    .iter()
                    .filter_map(|item| match item {
                        Item::Input { description, .. } => Some(description),
                        _ => None,
                    })
                    .map(|d| {
                        let plain = d.flags.is_empty()
                            && d.file.exclude.is_empty()
                            && d.file.pattern.matches_every_file();
                        let specs = d.sections.as_ref().filter(|_| plain)?;
                        specs
                            .iter()
                            .map(|s| {
                                (s.pattern.is_literal() && s.exclude_files.is_empty())
                                    .then(|| s.pattern.as_bytes())
                            })
                            .collect::<Option<Vec<&[u8]>>>()
                    })
                    .collect::<Option<Vec<Vec<&[u8]>>>>()
                    .map(|names| names.concat());
                files.iter().enumerate().any(|(file_index, file)| {
                    let Some(object) = &file.object else {
                        return false;
                    };
                    if let Some(names) = &literal {
                        return object.sections.iter().enumerate().any(|(index, section)| {
                            section.header.sh_flags & SHF_WRITE != 0
                                && names.contains(&section.name)
                                && sections
                                    .is_live_in(file_index, u32::try_from(index).unwrap_or(NONE))
                        });
                    }
                    let (fname, archive) = file_names(file);
                    object.sections.iter().enumerate().any(|(index, section)| {
                        section.header.sh_flags & SHF_WRITE != 0
                            && sections.is_live_in(file_index, u32::try_from(index).unwrap_or(NONE))
                            && stmt.items.iter().any(|item| match item {
                                Item::Input { description, .. } => matches_desc(
                                    description,
                                    fname,
                                    archive,
                                    section.name,
                                    section.header.sh_flags,
                                ),
                                _ => false,
                            })
                    })
                })
            })
            .collect();
        for (&output, writable) in constrained.iter().zip(writable) {
            let Some(stmt) = script.outputs.get(output as usize) else {
                continue;
            };
            let keep = match stmt.constraint {
                SectionConstraint::OnlyIfRo => !writable,
                SectionConstraint::OnlyIfRw => writable,
                _ => true,
            };
            if !keep && let Some(slot) = enabled.get_mut(output as usize) {
                *slot = false;
            }
        }
    }

    // 2. Descriptions in match order.
    let mut descs: Vec<Desc<'a>> = Vec::new();
    for statement in &script.statements {
        let Statement::Output(output) = statement else {
            continue;
        };
        if !enabled.get(*output as usize).copied().unwrap_or(false) {
            continue;
        }
        let Some(stmt) = script.outputs.get(*output as usize) else {
            continue;
        };
        for item in &stmt.items {
            if let Item::Input { description, index } = item {
                descs.push(Desc {
                    output: *output,
                    sub: *index,
                    keep: description.keep,
                    description,
                });
            }
        }
    }
    let discard_output = |output: u32| {
        script
            .outputs
            .get(output as usize)
            .is_some_and(OutputStmt::is_discard)
    };

    // 3. Match input sections, per file in parallel.
    let desc_index = DescIndex::new(&descs);
    let mut out = vec![NONE; total];
    let mut sub = vec![0u16; total];
    let mut keep = vec![false; total];
    type FileResult = (Vec<u32>, Vec<SectionId>);
    let per_file: Vec<FileResult> = {
        let outs = split_per_file(&sections.count, &mut out);
        let subs = split_per_file(&sections.count, &mut sub);
        let keeps = split_per_file(&sections.count, &mut keep);
        outs.into_par_iter()
            .zip(subs)
            .zip(keeps)
            .enumerate()
            .map(|(file_index, ((out, sub), keep))| {
                let mut orphans = Vec::new();
                let mut discarded = Vec::new();
                let Some(file) = files.get(file_index) else {
                    return (orphans, discarded);
                };
                let Some(object) = &file.object else {
                    return (orphans, discarded);
                };
                let (fname, archive) = file_names(file);
                for (index, section) in object.sections.iter().enumerate() {
                    let index32 = u32::try_from(index).unwrap_or(NONE);
                    if !sections.is_live_in(file_index, index32) {
                        continue;
                    }
                    let header = &section.header;
                    // In a relocatable link, COMDAT group members only match
                    // `/DISCARD/` (GNU ld's `unique_section_p`).
                    let unique = relocatable && section.group != 0;
                    let found = if unique {
                        descs.iter().find(|d| {
                            discard_output(d.output)
                                && matches_desc(
                                    d.description,
                                    fname,
                                    archive,
                                    section.name,
                                    header.sh_flags,
                                )
                        })
                    } else {
                        desc_index.find(&descs, section.name, |d| {
                            matches_desc(
                                d.description,
                                fname,
                                archive,
                                section.name,
                                header.sh_flags,
                            )
                        })
                    };
                    let flags_keep = header.sh_flags & SHF_GNU_RETAIN != 0
                        || header.sh_flags & SHF_ALLOC == 0
                        || header.sh_type == SHT_NOTE;
                    match found {
                        Some(desc) if discard_output(desc.output) => {
                            if let Some(id) = sections.id(file_index, index32) {
                                discarded.push(id);
                            }
                        }
                        Some(desc) => {
                            if let Some(slot) = out.get_mut(index) {
                                *slot = desc.output;
                            }
                            if let Some(slot) = sub.get_mut(index) {
                                *slot = desc.sub;
                            }
                            if let Some(slot) = keep.get_mut(index) {
                                *slot = desc.keep || flags_keep;
                            }
                        }
                        None => {
                            if let Some(slot) = keep.get_mut(index) {
                                *slot = flags_keep;
                            }
                            orphans.push(index32);
                        }
                    }
                }
                (orphans, discarded)
            })
            .collect()
    };

    // Flags and input presence per output, from the matches.
    let mut flags = vec![0u32; output_count];
    let mut has_input = vec![false; output_count];
    let mut types: Vec<Option<u32>> = vec![None; output_count];
    let mut aligns: Vec<u64> = vec![1; output_count];
    for (file_index, file) in files.iter().enumerate() {
        let Some(object) = &file.object else {
            continue;
        };
        let base = sections.base.get(file_index).copied().unwrap_or(NONE);
        if base == NONE {
            continue;
        }
        for (index, section) in object.sections.iter().enumerate() {
            let Some(&output) = (base as usize)
                .checked_add(index)
                .and_then(|at| out.get(at))
            else {
                continue;
            };
            if output == NONE {
                continue;
            }
            let o = output as usize;
            if let Some(slot) = flags.get_mut(o) {
                let f = gnu_flags(
                    section.header.sh_flags,
                    section.header.sh_type,
                    section.name,
                );
                if has_input.get(o).copied().unwrap_or(false) {
                    // READONLY only while every input is read-only.
                    *slot = (*slot | (f & !sec::READONLY)) & (f | !sec::READONLY);
                } else {
                    *slot = f;
                }
            }
            if let Some(slot) = has_input.get_mut(o) {
                *slot = true;
            }
            if let Some(slot) = aligns.get_mut(o) {
                *slot = (*slot).max(section.header.sh_addralign);
            }
            if let Some(slot) = types.get_mut(o)
                && slot.is_none()
            {
                *slot = Some(section.header.sh_type);
            }
        }
    }
    for (index, stmt) in script.outputs.iter().enumerate() {
        if let Some(slot) = flags.get_mut(index)
            && !has_input.get(index).copied().unwrap_or(false)
        {
            *slot = statement_flags(stmt);
            // A statement that makes a section without input sections (data
            // commands, assignments, `FILL`) has an output section of ELF
            // type 0 when orphans are placed, which matches no input's type
            // (`bfd_elf_match_sections_by_type`).
            let makes_section = stmt.items.iter().any(|item| {
                matches!(
                    item,
                    Item::Data { .. }
                        | Item::Asciz(_)
                        | Item::LinkerVersion
                        | Item::Assign { .. }
                        | Item::Fill(_)
                )
            });
            if makes_section && let Some(slot) = types.get_mut(index) {
                *slot = Some(0);
            }
        }
    }

    // 4. Linker-generated sections.
    let mut synthetic = Vec::new();
    let mut synthetic_orphans = Vec::new();
    let dynamic =
        crate::elf::export::Mode::new(options, files.iter().any(|f| f.shared.is_some())).dynamic;
    for &kind in SYNTHETIC_KINDS {
        let names = synthetic_names(kind, dynamic);
        let (sh_flags, sh_type) = crate::elf::layout::synthetic_flags(kind);
        let found = descs.iter().find(|d| {
            names
                .iter()
                .any(|name| matches_desc(d.description, b"", None, name, sh_flags))
        });
        match found {
            Some(desc) if discard_output(desc.output) => synthetic.push(SyntheticPlace {
                kind,
                output: NONE,
                sub: 0,
            }),
            Some(desc) => synthetic.push(SyntheticPlace {
                kind,
                output: desc.output,
                sub: desc.sub,
            }),
            None => synthetic_orphans.push((kind, sh_flags, sh_type)),
        }
    }

    // 5. Orphans, in input order.
    let executable = !matches!(
        options.kind,
        crate::args::OutputKind::Shared | crate::args::OutputKind::Relocatable
    );
    let mut by_name: hashbrown::HashMap<Vec<u8>, Vec<u32>, _> =
        hashbrown::HashMap::with_hasher(foldhash::fast::FixedState::with_seed(0x6e61_6d65));
    for statement in &script.statements {
        if let Statement::Output(i) = statement
            && let Some(stmt) = script.outputs.get(*i as usize)
        {
            by_name.entry(stmt.name.clone()).or_default().push(*i);
        }
    }
    let mut placer = Placer {
        script,
        statements: script.statements.clone(),
        by_name,
        orphans: Vec::new(),
        orphan_names: Vec::new(),
        flags,
        has_input,
        types,
        enabled,
        last: [None; 8],
        appended_sub: script
            .outputs
            .iter()
            .map(|o| u16::try_from(o.input_count()).unwrap_or(u16::MAX))
            .collect(),
        executable,
        relocatable,
        aligns,
        first_orphan_note: None,
    };
    let handling = options.orphan_handling;
    let mut reports = Vec::new();
    let mut discarded_all: Vec<SectionId> = Vec::new();
    let mut orphan_list: Vec<Orphan<'a>> = Vec::new();
    let mut synthetic_pending = Some(synthetic_orphans);
    let mode = crate::elf::export::Mode::new(options, files.iter().any(|f| f.shared.is_some()));
    let has_properties = files
        .iter()
        .filter_map(|f| f.object.as_ref())
        .any(|o| o.properties.is_some());
    // Which linker-generated sections are known to exist before their
    // sizes are planned.
    let synthetic_exists = |kind: Synthetic| match kind {
        Synthetic::BuildId => options.build_id != crate::args::BuildId::None,
        Synthetic::Interp => mode.interp,
        Synthetic::GnuProperty => {
            has_properties
                && !options
                    .output_format
                    .as_ref()
                    .is_some_and(crate::args::OutputFormat::is_raw)
        }
        Synthetic::Hash
        | Synthetic::GnuHash
        | Synthetic::DynSym
        | Synthetic::DynStr
        | Synthetic::Dynamic => mode.dynamic,
        _ => false,
    };
    for (file_index, (orphans, discarded)) in per_file.into_iter().enumerate() {
        discarded_all.extend(discarded);
        let Some(file) = files.get(file_index) else {
            continue;
        };
        let Some(object) = &file.object else {
            continue;
        };
        let first_object = synthetic_pending.is_some();
        for index in orphans {
            let Some(section) = object.section(index) else {
                continue;
            };
            let Some(id) = sections.id(file_index, index) else {
                continue;
            };
            orphan_list.push(Orphan {
                name: section.name,
                sh_type: section.header.sh_type,
                flags: gnu_flags(
                    section.header.sh_flags,
                    section.header.sh_type,
                    section.name,
                ),
                align: section.header.sh_addralign.max(1),
                tentative: false,
                special: relocatable && section.group != 0,
                what: OrphanWhat::Section(id),
                file: Some(file_index),
            });
        }
        // Linker-generated sections belong to the first input object.
        if first_object && let Some(list) = synthetic_pending.take() {
            push_synthetic_orphans(&mut orphan_list, list, dynamic, &synthetic_exists);
        }
    }
    if let Some(list) = synthetic_pending.take() {
        push_synthetic_orphans(&mut orphan_list, list, dynamic, &synthetic_exists);
    }
    let mut orphan_ids: Vec<(SectionId, u32, u16)> = Vec::new();
    for orphan in &orphan_list {
        let synthetic_kind = match orphan.what {
            OrphanWhat::Synthetic(kind) => Some(kind),
            OrphanWhat::Section(_) => None,
        };
        if handling == crate::args::OrphanHandling::Discard {
            match orphan.what {
                OrphanWhat::Section(id) => discarded_all.push(id),
                OrphanWhat::Synthetic(kind) => synthetic.push(SyntheticPlace {
                    kind,
                    output: NONE,
                    sub: 0,
                }),
            }
            continue;
        }
        // COMMON goes to .bss.
        let (output, orphan_sub) = if synthetic_kind == Some(Synthetic::Common) {
            match placer.find(b".bss") {
                Some(bss) => placer.append(bss, orphan.flags, orphan.tentative),
                None => placer.place(&Orphan {
                    name: b".bss",
                    sh_type: SHT_NOBITS,
                    flags: orphan.flags,
                    align: 8,
                    tentative: orphan.tentative,
                    special: false,
                    what: orphan.what,
                    file: None,
                }),
            }
        } else {
            placer.place(orphan)
        };
        if synthetic_kind.is_none() || synthetic_kind == Some(Synthetic::Common) {
            // Linker-generated sections are reported at layout, where their
            // sizes are known; COMMON is never an orphan in GNU ld.
        }
        if synthetic_kind.is_none() && handling != crate::args::OrphanHandling::Place {
            let output_name = String::from_utf8_lossy(placer.name(output)).into_owned();
            let section = String::from_utf8_lossy(orphan.name).into_owned();
            let error = handling == crate::args::OrphanHandling::Error;
            let display = orphan
                .file
                .and_then(|f| files.get(f))
                .map_or_else(|| "<internal>".to_string(), ElfInput::display);
            let message = if error {
                format!("unplaced orphan section `{section}' from `{}'", display)
            } else {
                format!(
                    "orphan section `{section}' from `{}' being placed in section `{output_name}'",
                    display
                )
            };
            reports.push((message, error));
        }
        match orphan.what {
            OrphanWhat::Section(id) => orphan_ids.push((id, output, orphan_sub)),
            OrphanWhat::Synthetic(kind) => synthetic.push(SyntheticPlace {
                kind,
                output,
                sub: orphan_sub,
            }),
        }
    }
    for (id, output, orphan_sub) in orphan_ids {
        if let Some(slot) = out.get_mut(id.index()) {
            *slot = output;
        }
        if let Some(slot) = sub.get_mut(id.index()) {
            *slot = orphan_sub;
        }
    }
    discarded_all.sort_unstable();

    // 6. Output sections, indexed like statements.
    let all_count = output_count.saturating_add(placer.orphans.len());
    let mut outputs: Vec<OutputSection<'a>> = Vec::with_capacity(all_count);
    for (index, stmt) in script.outputs.iter().enumerate() {
        outputs.push(OutputSection {
            name: &stmt.name,
            rule: u16::MAX,
            rank: (0, u32::try_from(index).unwrap_or(NONE)),
            sh_type: SHT_PROGBITS,
            flags: 0,
            synthetic: Synthetic::None,
            keep: false,
            stmt: u32::try_from(index).unwrap_or(NONE),
        });
    }
    for (offset, name) in placer.orphan_names.iter().enumerate() {
        let index = output_count.saturating_add(offset);
        outputs.push(OutputSection {
            name,
            rule: u16::MAX,
            rank: (0, u32::try_from(index).unwrap_or(NONE)),
            sh_type: SHT_PROGBITS,
            flags: 0,
            synthetic: Synthetic::None,
            keep: false,
            stmt: u32::try_from(index).unwrap_or(NONE),
        });
    }
    // Ranks follow the final statement order.
    for (position, statement) in placer.statements.iter().enumerate() {
        if let Statement::Output(i) = statement
            && let Some(output) = outputs.get_mut(*i as usize)
        {
            output.rank = (0, u32::try_from(position).unwrap_or(NONE));
        }
    }
    let input_flags = placer.flags.clone();
    let symbol_names = assigned_names(script, &placer.statements);
    let symbol_kinds = symbol_kinds(script, &placer.statements, &symbol_names);
    let type_sources = type_sources(script, &placer.statements, &symbol_names);
    let script_placement = ScriptPlacement {
        symbol_names,
        symbol_kinds,
        type_sources,
        referenced: script.referenced.iter().map(|(n, _)| n.clone()).collect(),
        resolved: std::sync::OnceLock::new(),
        statements: placer.statements,
        orphans: placer.orphans,
        synthetic,
        enabled: placer.enabled,
        reports,
        input_flags,
    };
    let mut placement = Placement {
        outputs,
        out,
        sub,
        keep,
        discarded: discarded_all,
        script: Some(Box::new(script_placement)),
    };
    placement.compute_flags(files, sections);
    placement
}

fn push_synthetic_orphans<'a>(
    list: &mut Vec<Orphan<'a>>,
    kinds: Vec<(Synthetic, u64, u32)>,
    dynamic: bool,
    exists: &dyn Fn(Synthetic) -> bool,
) {
    for (kind, sh_flags, sh_type) in kinds {
        let Some(&name) = synthetic_names(kind, dynamic).first() else {
            continue;
        };
        let align = match kind {
            Synthetic::BuildId => 4,
            Synthetic::Interp | Synthetic::DynStr => 1,
            Synthetic::VerSym => 2,
            _ => 8,
        };
        list.push(Orphan {
            name,
            sh_type,
            flags: gnu_flags(sh_flags, sh_type, name),
            align,
            tentative: !exists(kind),
            special: false,
            what: OrphanWhat::Synthetic(kind),
            file: None,
        });
    }
}

/// The symbols the statements assign, in order of first assignment.
fn assigned_names(script: &LayoutScript, statements: &[Statement]) -> Vec<Vec<u8>> {
    let mut names: Vec<Vec<u8>> = Vec::new();
    let mut seen: hashbrown::HashSet<Vec<u8>, foldhash::fast::FixedState> =
        hashbrown::HashSet::with_hasher(foldhash::fast::FixedState::with_seed(0x6e61_6d65));
    let mut add = |target: &[u8]| {
        if target != b"." && seen.insert(target.to_vec()) {
            names.push(target.to_vec());
        }
    };
    for statement in statements {
        match statement {
            Statement::Assign { assignment, .. } => add(&assignment.target),
            Statement::Assert { .. } => {}
            Statement::Output(index) => {
                if let Some(stmt) = script.outputs.get(*index as usize) {
                    for item in &stmt.items {
                        if let Item::Assign { assignment, .. } = item {
                            add(&assignment.target);
                        }
                    }
                }
            }
        }
    }
    names
}

/// For each assigned name: whether all its assignments are `PROVIDE`s, and
/// whether the last one hides it.
/// For each assigned symbol, the symbol whose type its last assignment
/// copies.
fn type_sources(
    script: &LayoutScript,
    statements: &[Statement],
    names: &[Vec<u8>],
) -> Vec<Option<Vec<u8>>> {
    let mut sources = vec![None; names.len()];
    let mut note = |assignment: &crate::script::Assignment| {
        if let Some(slot) = names.iter().position(|n| *n == assignment.target)
            && let Some(source) = sources.get_mut(slot)
        {
            *source = assignment
                .op
                .binary()
                .is_none()
                .then(|| assignment.expr.type_source().map(<[u8]>::to_vec))
                .flatten();
        }
    };
    for statement in statements {
        match statement {
            Statement::Assign { assignment, .. } => note(assignment),
            Statement::Assert { .. } => {}
            Statement::Output(index) => {
                if let Some(stmt) = script.outputs.get(*index as usize) {
                    for item in &stmt.items {
                        if let Item::Assign { assignment, .. } = item {
                            note(assignment);
                        }
                    }
                }
            }
        }
    }
    sources
}

fn symbol_kinds(
    script: &LayoutScript,
    statements: &[Statement],
    names: &[Vec<u8>],
) -> Vec<(bool, bool)> {
    use crate::script::AssignKind;
    let mut kinds = vec![(true, false); names.len()];
    let mut note = |assignment: &crate::script::Assignment| {
        if let Some(slot) = names.iter().position(|n| *n == assignment.target)
            && let Some(kind) = kinds.get_mut(slot)
        {
            let provide = matches!(
                assignment.kind,
                AssignKind::Provide | AssignKind::ProvideHidden
            );
            kind.0 &= provide;
            kind.1 = matches!(
                assignment.kind,
                AssignKind::Hidden | AssignKind::ProvideHidden
            );
        }
    };
    for statement in statements {
        match statement {
            Statement::Assign { assignment, .. } => note(assignment),
            Statement::Assert { .. } => {}
            Statement::Output(index) => {
                if let Some(stmt) = script.outputs.get(*index as usize) {
                    for item in &stmt.items {
                        if let Item::Assign { assignment, .. } = item {
                            note(assignment);
                        }
                    }
                }
            }
        }
    }
    kinds
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gnu_flags_follow_bfd() {
        let text = gnu_flags(SHF_ALLOC | SHF_EXECINSTR, 1, b".text");
        assert_eq!(
            text,
            sec::HAS_CONTENTS | sec::ALLOC | sec::LOAD | sec::READONLY | sec::CODE
        );
        let bss = gnu_flags(SHF_ALLOC | SHF_WRITE, SHT_NOBITS, b".bss");
        assert_eq!(bss, sec::ALLOC);
        let debug = gnu_flags(0, 1, b".debug_info");
        assert_eq!(debug, sec::HAS_CONTENTS | sec::READONLY | sec::DEBUGGING);
    }

    #[test]
    fn input_section_flags() {
        let flag = |name: &str, negated| SectionFlag {
            name: name.as_bytes().to_vec(),
            negated,
        };
        assert!(flags_match(&[flag("SHF_ALLOC", false)], SHF_ALLOC));
        assert!(!flags_match(&[flag("SHF_WRITE", false)], SHF_ALLOC));
        assert!(flags_match(&[flag("SHF_WRITE", true)], SHF_ALLOC));
        assert!(!flags_match(&[flag("SHF_NOSUCH", false)], SHF_ALLOC));
    }
}
