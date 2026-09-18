//! Output sections, grouped-section ordering and addresses.
//!
//! The placement rules reproduce the default script of GNU `ld`'s `i386pep`
//! emulation (`ld -m i386pep --verbose`), which is what MinGW expects:
//!
//! | Output | Inputs, in order |
//! | --- | --- |
//! | `.text` | `.init`, `.text`, `.text$*`, `.text.*`, `.gnu.linkonce.t.*`, `.glue_7t`, `.glue_7`, `.fini`, `.gcc_exc`, `.gcc_except_table` |
//! | `.data` | `.data`, `.data2`, `.data$*`, `.jcr`, `.data_cygwin_nocopy` |
//! | `.rdata` | `.rdata`, `.rdata$*`, the pseudo-relocation list, `.ctors*`, `.dtors*`, `.CRT$*` |
//! | `.eh_frame`, `.pdata`, `.xdata` | `.eh_frame*`, `.pdata*`, `.xdata*` |
//! | `.bss` | `.bss`, `.bss$*`, common symbols |
//! | `.edata` | `.edata` |
//! | `.idata` | `.idata$2`, `.idata$3`, a null directory entry, `.idata$4`, `.idata$5`, `.idata$6`, `.idata$7` |
//! | `.tls` | `.tls$AAA`, `.tls`, `.tls$*`, `.tls$ZZZ` |
//! | `.rsrc` | `.rsrc`, `.rsrc$*` |
//! | `.reloc` | the base relocations |
//! | `.stab`, `.debug_*` | after `.reloc`, in the script's order |
//!
//! Sections with a `$` in their name are *grouped sections*: the output name
//! is the part before the `$`, and the contributions are ordered by the whole
//! name. The `.idata$N` groups are ordered by the contributing file name
//! instead (GNU `ld`'s `SORT(*)`), which is what makes a `dlltool` import
//! library's head, symbol and tail members land in the right order.
//!
//! Anything a rule does not match is an orphan: it gets an output section
//! named after the part before its `$`, placed after the known sections.
//!
//! `i386pe`'s script differs in three places, which the recipe follows for
//! PE32 images: the constructor lists start and end with 4-byte rather than
//! 8-byte words, and `.text`, `.rdata` and `.idata` have no 8-byte
//! alignments. The i386 SafeSEH table goes at the end of `.rdata`, and an
//! ARM64 input section may be followed by its range-extension thunks.

#![deny(clippy::arithmetic_side_effects)]

use std::collections::BTreeMap;

use crate::error::{Error, Result};

use super::arm64::Thunks;
use super::inputs::CoffInput;
use super::machine::Machine;
use super::options::PeOptions;
use super::read::consts::{
    IMAGE_SCN_CNT_CODE, IMAGE_SCN_CNT_INITIALIZED_DATA, IMAGE_SCN_CNT_UNINITIALIZED_DATA,
    IMAGE_SCN_MEM_DISCARDABLE, IMAGE_SCN_MEM_EXECUTE, IMAGE_SCN_MEM_READ, IMAGE_SCN_MEM_WRITE,
};

/// How the contributions matched by a rule are ordered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sort {
    /// Input order.
    None,
    /// By section name, so `.text$a` precedes `.text$b`.
    ByName,
    /// By the contributing file's name, then input order: GNU `ld`'s
    /// `SORT(*)(.idata$N)`.
    ByFile,
}

/// How a rule matches an input section name.
#[derive(Clone, Copy, Debug)]
enum Match {
    /// The name is exactly this.
    Exact(&'static [u8]),
    /// The name starts with this.
    Prefix(&'static [u8]),
}

impl Match {
    fn matches(self, name: &[u8]) -> bool {
        match self {
            Self::Exact(text) => name == text,
            Self::Prefix(text) => name.starts_with(text),
        }
    }
}

/// One placement rule: where a set of input section names goes.
#[derive(Clone, Copy, Debug)]
struct Rule {
    output: &'static [u8],
    pattern: Match,
    sort: Sort,
}

const fn rule(output: &'static [u8], pattern: Match, sort: Sort) -> Rule {
    Rule {
        output,
        pattern,
        sort,
    }
}

/// A position in an output section the linker names, and the bytes (if any)
/// it inserts there. Most markers only record where a linker-defined symbol
/// goes; see [`super::defined`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Marker {
    /// `etext`, after the last `.text` contribution.
    Etext,
    /// `__data_start__`.
    DataStart,
    /// `__data_end__`.
    DataEnd,
    /// `__rt_psrelocs_start`, the MinGW pseudo-relocation list.
    PseudoStart,
    /// `__rt_psrelocs_end`.
    PseudoEnd,
    /// `__CTOR_LIST__`: `LONG(-1); LONG(-1);` before the `.ctors` inputs.
    CtorHead,
    /// `LONG(0); LONG(0);` after them.
    CtorTail,
    /// `__DTOR_LIST__`.
    DtorHead,
    /// The `.dtors` terminator.
    DtorTail,
    /// `___crt_xc_start__`, before the C initializers.
    CrtXcStart,
    /// `___crt_xc_end__`.
    CrtXcEnd,
    /// `___crt_xi_start__`, before the C++ initializers.
    CrtXiStart,
    /// `___crt_xi_end__`.
    CrtXiEnd,
    /// `___crt_xl_start__`, before the TLS callbacks.
    CrtXlStart,
    /// `___crt_xp_start__`, before the pre-termination routines.
    CrtXpStart,
    /// `___crt_xp_end__`.
    CrtXpEnd,
    /// `___crt_xt_start__`, before the termination routines.
    CrtXtStart,
    /// `___crt_xt_end__`.
    CrtXtEnd,
    /// `___crt_xd_start__`, before the dynamic TLS initializers.
    CrtXdStart,
    /// `___crt_xd_end__`.
    CrtXdEnd,
    /// The null import directory entry after `.idata$3`.
    IdataNull,
    /// `__IAT_start__`, before the import address table.
    IatStart,
    /// `__IAT_end__`.
    IatEnd,
    /// `___tls_start__`.
    TlsStart,
    /// `___tls_end__`.
    TlsEnd,
    /// The i386 SafeSEH handler table, `___safe_se_handler_table`, at the
    /// end of `.rdata`.
    SafeSehTable,
}

/// One step of the output section recipe: a rule, a marker or an alignment.
#[derive(Clone, Copy, Debug)]
enum Step {
    Place(Rule),
    Insert(&'static [u8], Marker),
    Align(&'static [u8], u32),
    /// An alignment only the PE32+ scripts (`i386pep`, `arm64pe`) have;
    /// `i386pe` packs these places to 4 bytes.
    AlignWide(&'static [u8], u32),
}

/// The recipe, in output order. Output sections appear in the order their
/// first step does.
const RECIPE: &[Step] = &[
    Step::Place(rule(b".text", Match::Exact(b".init"), Sort::None)),
    Step::Place(rule(b".text", Match::Exact(b".text"), Sort::None)),
    Step::Place(rule(b".text", Match::Prefix(b".text$"), Sort::ByName)),
    Step::Place(rule(b".text", Match::Prefix(b".text."), Sort::None)),
    Step::Place(rule(
        b".text",
        Match::Prefix(b".gnu.linkonce.t."),
        Sort::None,
    )),
    Step::Place(rule(b".text", Match::Exact(b".glue_7t"), Sort::None)),
    Step::Place(rule(b".text", Match::Exact(b".glue_7"), Sort::None)),
    Step::AlignWide(b".text", 8),
    Step::Place(rule(b".text", Match::Exact(b".fini"), Sort::None)),
    Step::Place(rule(b".text", Match::Exact(b".gcc_exc"), Sort::None)),
    Step::Place(rule(
        b".text",
        Match::Exact(b".gcc_except_table"),
        Sort::None,
    )),
    Step::Insert(b".text", Marker::Etext),
    Step::Insert(b".data", Marker::DataStart),
    Step::Place(rule(b".data", Match::Exact(b".data"), Sort::None)),
    Step::Place(rule(b".data", Match::Exact(b".data2"), Sort::None)),
    Step::Place(rule(b".data", Match::Prefix(b".data$"), Sort::ByName)),
    Step::Place(rule(b".data", Match::Prefix(b".data."), Sort::None)),
    Step::Place(rule(
        b".data",
        Match::Prefix(b".gnu.linkonce.d."),
        Sort::None,
    )),
    Step::Place(rule(b".data", Match::Exact(b".jcr"), Sort::None)),
    Step::Insert(b".data", Marker::DataEnd),
    Step::Place(rule(
        b".data",
        Match::Exact(b".data_cygwin_nocopy"),
        Sort::None,
    )),
    Step::Place(rule(b".rdata", Match::Exact(b".rdata"), Sort::None)),
    Step::Place(rule(b".rdata", Match::Prefix(b".rdata$"), Sort::ByName)),
    Step::Place(rule(b".rdata", Match::Prefix(b".rdata."), Sort::None)),
    Step::Place(rule(b".rdata", Match::Exact(b".rodata"), Sort::None)),
    Step::Place(rule(b".rdata", Match::Prefix(b".rodata."), Sort::None)),
    Step::Place(rule(
        b".rdata",
        Match::Prefix(b".gnu.linkonce.r."),
        Sort::None,
    )),
    Step::Align(b".rdata", 4),
    Step::Insert(b".rdata", Marker::PseudoStart),
    Step::Place(rule(
        b".rdata",
        Match::Exact(b".rdata_runtime_pseudo_reloc"),
        Sort::None,
    )),
    Step::Insert(b".rdata", Marker::PseudoEnd),
    Step::AlignWide(b".rdata", 8),
    Step::Insert(b".rdata", Marker::CtorHead),
    Step::Place(rule(b".rdata", Match::Exact(b".ctors"), Sort::None)),
    Step::Place(rule(b".rdata", Match::Exact(b".ctor"), Sort::None)),
    Step::Place(rule(b".rdata", Match::Prefix(b".ctors."), Sort::ByName)),
    Step::Insert(b".rdata", Marker::CtorTail),
    Step::Insert(b".rdata", Marker::DtorHead),
    Step::Place(rule(b".rdata", Match::Exact(b".dtors"), Sort::None)),
    Step::Place(rule(b".rdata", Match::Exact(b".dtor"), Sort::None)),
    Step::Place(rule(b".rdata", Match::Prefix(b".dtors."), Sort::ByName)),
    Step::Insert(b".rdata", Marker::DtorTail),
    Step::Insert(b".rdata", Marker::CrtXcStart),
    Step::Place(rule(b".rdata", Match::Prefix(b".CRT$XC"), Sort::ByName)),
    Step::Insert(b".rdata", Marker::CrtXcEnd),
    Step::Insert(b".rdata", Marker::CrtXiStart),
    Step::Place(rule(b".rdata", Match::Prefix(b".CRT$XI"), Sort::ByName)),
    Step::Insert(b".rdata", Marker::CrtXiEnd),
    Step::Insert(b".rdata", Marker::CrtXlStart),
    Step::Place(rule(b".rdata", Match::Prefix(b".CRT$XL"), Sort::ByName)),
    Step::Insert(b".rdata", Marker::CrtXpStart),
    Step::Place(rule(b".rdata", Match::Prefix(b".CRT$XP"), Sort::ByName)),
    Step::Insert(b".rdata", Marker::CrtXpEnd),
    Step::Insert(b".rdata", Marker::CrtXtStart),
    Step::Place(rule(b".rdata", Match::Prefix(b".CRT$XT"), Sort::ByName)),
    Step::Insert(b".rdata", Marker::CrtXtEnd),
    Step::Insert(b".rdata", Marker::CrtXdStart),
    Step::Place(rule(b".rdata", Match::Prefix(b".CRT$XD"), Sort::ByName)),
    Step::Insert(b".rdata", Marker::CrtXdEnd),
    Step::Place(rule(b".rdata", Match::Prefix(b".CRT$"), Sort::ByName)),
    Step::Align(b".rdata", 4),
    Step::Insert(b".rdata", Marker::SafeSehTable),
    Step::Place(rule(b".eh_frame", Match::Prefix(b".eh_frame"), Sort::None)),
    Step::Place(rule(b".pdata", Match::Prefix(b".pdata"), Sort::None)),
    Step::Place(rule(b".xdata", Match::Prefix(b".xdata"), Sort::None)),
    Step::Place(rule(b".bss", Match::Exact(b".bss"), Sort::None)),
    Step::Place(rule(b".bss", Match::Prefix(b".bss$"), Sort::ByName)),
    Step::Place(rule(b".bss", Match::Prefix(b".bss."), Sort::None)),
    Step::Place(rule(
        b".bss",
        Match::Prefix(b".gnu.linkonce.b."),
        Sort::None,
    )),
    Step::Place(rule(b".edata", Match::Exact(b".edata"), Sort::None)),
    Step::Place(rule(b".idata", Match::Exact(b".idata$2"), Sort::ByFile)),
    Step::Place(rule(b".idata", Match::Exact(b".idata$3"), Sort::ByFile)),
    Step::Insert(b".idata", Marker::IdataNull),
    Step::AlignWide(b".idata", 8),
    Step::Place(rule(b".idata", Match::Exact(b".idata$4"), Sort::ByFile)),
    Step::Insert(b".idata", Marker::IatStart),
    Step::Place(rule(b".idata", Match::Exact(b".idata$5"), Sort::ByFile)),
    Step::Insert(b".idata", Marker::IatEnd),
    Step::Place(rule(b".idata", Match::Exact(b".idata$6"), Sort::ByFile)),
    Step::Place(rule(b".idata", Match::Exact(b".idata$7"), Sort::ByFile)),
    Step::Insert(b".tls", Marker::TlsStart),
    Step::Place(rule(b".tls", Match::Exact(b".tls$AAA"), Sort::None)),
    Step::Place(rule(b".tls", Match::Exact(b".tls"), Sort::None)),
    Step::Place(rule(b".tls", Match::Exact(b".tls$"), Sort::None)),
    Step::Place(rule(b".tls", Match::Prefix(b".tls$"), Sort::ByName)),
    Step::Insert(b".tls", Marker::TlsEnd),
];

/// Output sections created after the orphans, in this order.
const TRAILING: &[&[u8]] = &[b".rsrc", b".reloc"];

/// One piece of an output section.
#[derive(Clone, Debug)]
pub enum Piece {
    /// An input section's contents.
    Input {
        /// Index into the input file list.
        file: u32,
        /// The 1-based COFF section number.
        section: u32,
    },
    /// Bytes the linker generates.
    Fill(Vec<u8>),
    /// Zero bytes (padding, `.bss`, common symbols).
    Zero,
    /// The ARM64 range-extension thunks for the branches of an input
    /// section, placed right after it.
    Thunks {
        /// Index into the input file list.
        file: u32,
        /// The 1-based COFF section number.
        section: u32,
    },
}

/// A placed piece of an output section.
#[derive(Clone, Debug)]
pub struct Chunk {
    /// Offset in the output section.
    pub offset: u32,
    /// Size in bytes.
    pub size: u32,
    /// Where the bytes come from.
    pub piece: Piece,
}

/// One output section of the image.
#[derive(Clone, Debug, Default)]
pub struct OutSection {
    /// The section name, as written in the header.
    pub name: Vec<u8>,
    /// `Characteristics`.
    pub characteristics: u32,
    /// RVA of the section.
    pub rva: u32,
    /// Size in memory.
    pub virtual_size: u32,
    /// Size in the file, rounded up to the file alignment.
    pub raw_size: u32,
    /// File offset of the contents, or 0 for `.bss`.
    pub file_offset: u32,
    /// The pieces, in order.
    pub chunks: Vec<Chunk>,
    /// Highest alignment any contribution requires.
    pub align: u32,
}

impl OutSection {
    /// Whether the section has no file contents.
    #[must_use]
    pub fn is_bss(&self) -> bool {
        self.characteristics & IMAGE_SCN_CNT_UNINITIALIZED_DATA != 0
            && self.characteristics & IMAGE_SCN_CNT_INITIALIZED_DATA == 0
    }
}

/// The `Characteristics` GNU `ld` gives each known output section.
#[must_use]
pub fn characteristics_for(name: &[u8]) -> u32 {
    const R: u32 = IMAGE_SCN_MEM_READ;
    const W: u32 = IMAGE_SCN_MEM_WRITE;
    const X: u32 = IMAGE_SCN_MEM_EXECUTE;
    const DATA: u32 = IMAGE_SCN_CNT_INITIALIZED_DATA;
    const BSS: u32 = IMAGE_SCN_CNT_UNINITIALIZED_DATA;
    const CODE: u32 = IMAGE_SCN_CNT_CODE;
    match name {
        b".text" => CODE | X | R,
        b".data" | b".tls" => DATA | R | W,
        b".bss" => BSS | R | W,
        b".reloc" => DATA | IMAGE_SCN_MEM_DISCARDABLE | R,
        _ if name.starts_with(b".debug") || name == b".stab" || name == b".stabstr" => {
            DATA | IMAGE_SCN_MEM_DISCARDABLE | R
        }
        _ => DATA | R,
    }
}

/// Where a live input section is placed, and how its group is ordered.
#[derive(Clone, Copy, Debug)]
struct Placed {
    step: u32,
    sort: Sort,
    file: u32,
    section: u32,
}

/// The plan for the whole image.
#[derive(Debug, Default)]
pub struct Layout {
    /// The output sections, in image order.
    pub sections: Vec<OutSection>,
    /// RVA of each input section: `rvas[file][section - 1]`.
    pub rvas: Vec<Vec<u32>>,
    /// Which output section each input section landed in, or `u32::MAX`.
    pub outputs: Vec<Vec<u32>>,
    /// Size of the headers, rounded to the file alignment.
    pub size_of_headers: u32,
    /// Size of the image, rounded to the section alignment.
    pub size_of_image: u32,
    /// Total file size.
    pub file_size: u64,
    /// Offset of each marker in its output section, by [`Marker`].
    pub markers: Vec<(Marker, u32, u32)>,
    /// The range-extension thunks laid out, as planned.
    pub thunks: Thunks,
    /// RVA of each thunk block, by `(file, section)`.
    pub thunk_rvas: BTreeMap<(u32, u32), u32>,
    /// The machine the image is for.
    pub machine: Machine,
}

impl Layout {
    /// The output section with this name.
    #[must_use]
    pub fn by_name(&self, name: &[u8]) -> Option<&OutSection> {
        self.sections.iter().find(|section| section.name == name)
    }

    /// The index of the output section with this name.
    #[must_use]
    pub fn index_of(&self, name: &[u8]) -> Option<u32> {
        self.sections
            .iter()
            .position(|section| section.name == name)
            .and_then(|index| u32::try_from(index).ok())
    }

    /// The RVA of input section `section` (1-based) of `file`.
    #[must_use]
    pub fn rva_of(&self, file: usize, section: u32) -> Option<u32> {
        let index = usize::try_from(section.checked_sub(1)?).ok()?;
        self.rvas.get(file)?.get(index).copied()
    }

    /// The RVA and section index of a marker.
    #[must_use]
    pub fn marker(&self, marker: Marker) -> Option<u32> {
        self.markers
            .iter()
            .find(|&&(kind, _, _)| kind == marker)
            .and_then(|&(_, section, offset)| {
                Some(
                    self.sections
                        .get(section as usize)?
                        .rva
                        .wrapping_add(offset),
                )
            })
    }

    /// The end RVA of an output section, aligned up to the section
    /// alignment.
    #[must_use]
    pub fn end_of(&self, name: &[u8], alignment: u32) -> Option<u32> {
        let section = self.by_name(name)?;
        Some(align_up32(
            section.rva.wrapping_add(section.virtual_size),
            alignment,
        ))
    }
}

/// Rounds `value` up to a multiple of `align` (a power of two).
#[must_use]
pub fn align_up32(value: u32, align: u32) -> u32 {
    if align <= 1 {
        return value;
    }
    let mask = align.wrapping_sub(1);
    value.checked_add(mask).map_or(value, |sum| sum & !mask)
}

/// Rounds `value` up to a multiple of `align` (a power of two).
#[must_use]
pub fn align_up64(value: u64, align: u64) -> u64 {
    if align <= 1 {
        return value;
    }
    let mask = align.wrapping_sub(1);
    value.checked_add(mask).map_or(value, |sum| sum & !mask)
}

/// A common symbol to allocate in `.bss`.
#[derive(Clone, Copy, Debug)]
pub struct CommonSymbol {
    /// The symbol's ID.
    pub symbol: crate::ids::SymbolId,
    /// Size in bytes.
    pub size: u32,
    /// Alignment in bytes.
    pub align: u32,
}

/// Everything the layout pass needs besides the inputs.
#[derive(Debug)]
pub struct LayoutInput<'i, 'a> {
    /// The inputs.
    pub files: &'i [CoffInput<'a>],
    /// The PE options.
    pub options: &'i PeOptions,
    /// Common symbols, in symbol ID order.
    pub commons: &'i [CommonSymbol],
    /// Extra sections the linker generates, as `(name, size, align)`; their
    /// contents are filled in later.
    pub synthetic: &'i [(Vec<u8>, u32, u32)],
    /// Bytes to reserve for the MinGW runtime pseudo-relocation list, which
    /// sits between the `__RUNTIME_PSEUDO_RELOC_LIST__` bounds in `.rdata`.
    pub pseudo_reloc_size: u32,
    /// ARM64 range-extension thunks, each block placed after the input
    /// section whose branches need it.
    pub thunks: &'i Thunks,
    /// Bytes to reserve for the i386 SafeSEH handler table.
    pub safe_seh_size: u32,
}

/// Assigns every live input section to an output section and gives each one
/// an RVA and a file offset.
///
/// # Errors
///
/// Returns [`Error::Limit`] if the image does not fit in 32 bits.
pub fn layout(input: &LayoutInput<'_, '_>) -> Result<Layout> {
    let options = input.options;
    let mut build = Builder::default();

    // Reserve the known output sections in recipe order, so the image keeps
    // GNU ld's section order even when a rule matches nothing.
    let mut step_output: Vec<usize> = Vec::with_capacity(RECIPE.len());
    for (index, step) in RECIPE.iter().enumerate() {
        let name = match step {
            Step::Place(rule) => rule.output,
            Step::Insert(name, _) | Step::Align(name, _) | Step::AlignWide(name, _) => name,
        };
        let rank = u32::try_from(index).unwrap_or(0);
        step_output.push(build.section(name, rank));
    }
    for (offset, name) in TRAILING.iter().enumerate() {
        let rank = RANK_TRAILING.saturating_add(u32::try_from(offset).unwrap_or(0));
        build.section(name, rank);
    }

    // Assign each live input section to a step, or to an orphan output.
    let mut placements: Vec<Vec<u32>> = Vec::with_capacity(input.files.len());
    for (file_index, file) in input.files.iter().enumerate() {
        let file_index = u32::try_from(file_index).unwrap_or(u32::MAX);
        let Some(parsed) = file.object() else {
            placements.push(Vec::new());
            continue;
        };
        let mut per_section = vec![u32::MAX; parsed.sections.len()];
        for section in &parsed.sections {
            if !section.is_live() {
                continue;
            }
            let name: &[u8] = &section.name;
            let step = RECIPE.iter().position(|step| match step {
                Step::Place(rule) => rule.pattern.matches(name),
                Step::Insert(..) | Step::Align(..) | Step::AlignWide(..) => false,
            });
            let (at, sort, step_index) = match step {
                Some(step) => {
                    let Some(Step::Place(rule)) = RECIPE.get(step) else {
                        continue;
                    };
                    (
                        step_output.get(step).copied().unwrap_or(0),
                        rule.sort,
                        u32::try_from(step).unwrap_or(u32::MAX),
                    )
                }
                None => {
                    let base = orphan_name(name);
                    let at = build.section(base, orphan_rank(base));
                    let sort = if base.len() == name.len() {
                        Sort::None
                    } else {
                        Sort::ByName
                    };
                    (at, sort, u32::MAX)
                }
            };
            if let Some(slot) = per_section.get_mut(section.number.wrapping_sub(1) as usize) {
                *slot = u32::try_from(at).unwrap_or(u32::MAX);
            }
            if let Some(list) = build.order.get_mut(at) {
                list.push(Placed {
                    step: step_index,
                    sort,
                    file: file_index,
                    section: section.number,
                });
            }
            if let Some(out) = build.sections.get_mut(at) {
                out.align = out.align.max(section.align);
                if section.header.is_code() {
                    out.characteristics |= IMAGE_SCN_CNT_CODE | IMAGE_SCN_MEM_EXECUTE;
                }
                // GNU ld's output flags are the union of the inputs': code
                // that also claims initialized data (some i386 runtime
                // objects) makes `.text` count as both.
                if out.characteristics & IMAGE_SCN_CNT_CODE != 0
                    && section.header.is_initialized_data()
                {
                    out.characteristics |= IMAGE_SCN_CNT_INITIALIZED_DATA;
                }
            }
        }
        placements.push(per_section);
    }

    // Sections the linker generates (.reloc, .edata, .rsrc, the import
    // tables); their bytes are filled in after layout.
    for (name, size, align) in input.synthetic {
        let rank = if TRAILING.contains(&name.as_slice()) {
            RANK_TRAILING
        } else {
            RANK_ORPHAN
        };
        let at = build.section(name, rank);
        if let Some(out) = build.sections.get_mut(at) {
            out.align = out.align.max(*align);
            build.pending[at].push(*size);
        }
    }

    // Order the contributions of each output section and place them.
    let pointer = options.target().pointer_size();
    let wide = !options.target().is_pe32();
    let mut markers = Vec::new();
    for at in 0..build.sections.len() {
        let mut list = std::mem::take(&mut build.order[at]);
        sort_contributions(&mut list, input.files);
        let pending = std::mem::take(&mut build.pending[at]);
        let mut chunks: Vec<Chunk> = Vec::with_capacity(list.len());
        let name = build.sections[at].name.clone();
        let align = build.sections[at].align;
        let mut offset = 0u32;
        let mut cursor = 0usize;
        for (step_index, step) in RECIPE.iter().enumerate() {
            let step_index = u32::try_from(step_index).unwrap_or(u32::MAX);
            match step {
                Step::Align(step_name, step_align) if *step_name == name.as_slice() => {
                    offset = align_up32(offset, *step_align);
                }
                Step::AlignWide(step_name, step_align) if wide && *step_name == name.as_slice() => {
                    offset = align_up32(offset, *step_align);
                }
                Step::Insert(step_name, marker) if *step_name == name.as_slice() => {
                    let bytes = marker_bytes(*marker, pointer);
                    markers.push((*marker, u32::try_from(at).unwrap_or(u32::MAX), offset));
                    if !bytes.is_empty() {
                        let size = u32::try_from(bytes.len()).unwrap_or(0);
                        chunks.push(Chunk {
                            offset,
                            size,
                            piece: Piece::Fill(bytes),
                        });
                        offset = offset.saturating_add(size);
                    }
                    // The pseudo-relocation list the linker generates goes
                    // where GNU ld's script puts the input sections of the
                    // same name: inside the `__RUNTIME_PSEUDO_RELOC_LIST__`
                    // bounds. Its bytes are patched in after layout.
                    let reserved = match marker {
                        Marker::PseudoStart => input.pseudo_reloc_size,
                        Marker::SafeSehTable => input.safe_seh_size,
                        _ => 0,
                    };
                    if reserved > 0 {
                        chunks.push(Chunk {
                            offset,
                            size: reserved,
                            piece: Piece::Zero,
                        });
                        offset = offset.saturating_add(reserved);
                    }
                }
                Step::Place(_) => {
                    while let Some(placed) = list.get(cursor).copied() {
                        if placed.step != step_index {
                            break;
                        }
                        cursor = cursor.saturating_add(1);
                        offset = push_input(&mut chunks, input, placed, offset);
                    }
                }
                Step::Align(..) | Step::AlignWide(..) | Step::Insert(..) => {}
            }
        }
        while let Some(placed) = list.get(cursor).copied() {
            cursor = cursor.saturating_add(1);
            offset = push_input(&mut chunks, input, placed, offset);
        }
        if name == b".bss" {
            for common in input.commons {
                let common_align = common.align.max(1);
                offset = align_up32(offset, common_align);
                build.sections[at].align = build.sections[at].align.max(common_align);
                chunks.push(Chunk {
                    offset,
                    size: common.size,
                    piece: Piece::Zero,
                });
                offset = offset.saturating_add(common.size);
            }
        }
        for size in pending {
            offset = align_up32(offset, align.max(1));
            chunks.push(Chunk {
                offset,
                size,
                piece: Piece::Zero,
            });
            offset = offset.saturating_add(size);
        }
        build.sections[at].virtual_size = offset;
        build.sections[at].chunks = chunks;
    }

    // Drop the empty sections and put the rest in image order.
    let mut keep: Vec<usize> = (0..build.sections.len())
        .filter(|&at| build.sections[at].virtual_size > 0)
        .collect();
    keep.sort_by_key(|&at| (build.ranks[at], at));
    let mut remap = vec![u32::MAX; build.sections.len()];
    for (new, &old) in keep.iter().enumerate() {
        remap[old] = u32::try_from(new).unwrap_or(u32::MAX);
    }
    let mut sections: Vec<OutSection> = Vec::with_capacity(keep.len());
    for &old in &keep {
        sections.push(std::mem::take(&mut build.sections[old]));
    }
    for per_section in &mut placements {
        for slot in per_section.iter_mut() {
            *slot = remap.get(*slot as usize).copied().unwrap_or(u32::MAX);
        }
    }
    let markers: Vec<(Marker, u32, u32)> = markers
        .into_iter()
        .filter_map(|(marker, at, offset)| {
            let at = remap.get(at as usize).copied()?;
            (at != u32::MAX).then_some((marker, at, offset))
        })
        .collect();

    // Headers, then the sections, at the section and file alignments.
    let header_size = u64::try_from(super::write::header_size(sections.len(), options.target()))
        .map_err(|_| Error::Limit("too many output sections".into()))?;
    let file_alignment = u64::from(options.file_alignment);
    let section_alignment = options.section_alignment;
    let size_of_headers = u32::try_from(align_up64(header_size, file_alignment))
        .map_err(|_| Error::Limit("PE headers too large".into()))?;
    let mut rva = align_up32(size_of_headers, section_alignment);
    let mut file_offset = u64::from(size_of_headers);
    for section in &mut sections {
        section.rva = rva;
        if section.is_bss() {
            section.raw_size = 0;
            section.file_offset = 0;
        } else {
            section.raw_size =
                u32::try_from(align_up64(u64::from(section.virtual_size), file_alignment))
                    .map_err(|_| Error::Limit("output section too large".into()))?;
            section.file_offset = u32::try_from(file_offset)
                .map_err(|_| Error::Limit("output file too large".into()))?;
            file_offset = file_offset.saturating_add(u64::from(section.raw_size));
        }
        rva = rva
            .checked_add(section.virtual_size)
            .map(|end| align_up32(end, section_alignment))
            .ok_or_else(|| Error::Limit("image larger than 4 GiB".into()))?;
    }

    // Record where the thunk blocks ended up.
    let mut thunk_rvas = BTreeMap::new();
    for section in &sections {
        for chunk in &section.chunks {
            if let Piece::Thunks {
                file,
                section: number,
            } = chunk.piece
            {
                thunk_rvas.insert((file, number), section.rva.wrapping_add(chunk.offset));
            }
        }
    }

    // Record where every input section ended up.
    let mut rvas: Vec<Vec<u32>> = Vec::with_capacity(input.files.len());
    for (file_index, per_section) in placements.iter().enumerate() {
        let mut file_rvas = vec![0u32; per_section.len()];
        for (index, &at) in per_section.iter().enumerate() {
            let Some(section) = sections.get(at as usize) else {
                continue;
            };
            let number = u32::try_from(index.saturating_add(1)).unwrap_or(u32::MAX);
            let offset = section
                .chunks
                .iter()
                .find(|chunk| {
                    matches!(chunk.piece, Piece::Input { file, section }
                        if file as usize == file_index && section == number)
                })
                .map_or(0, |chunk| chunk.offset);
            if let Some(slot) = file_rvas.get_mut(index) {
                *slot = section.rva.wrapping_add(offset);
            }
        }
        rvas.push(file_rvas);
    }

    Ok(Layout {
        size_of_image: rva,
        size_of_headers,
        file_size: file_offset,
        sections,
        rvas,
        outputs: placements,
        markers,
        thunks: input.thunks.clone(),
        thunk_rvas,
        machine: options.target(),
    })
}

/// Rank given to orphan output sections: after everything the recipe names.
const RANK_ORPHAN: u32 = 1000;
/// Rank of the sections in [`TRAILING`], which close the image.
const RANK_TRAILING: u32 = 2000;
/// Rank of the debugging sections, which follow `.reloc` in the order of
/// [`DEBUG_ORDER`].
const RANK_DEBUG: u32 = 3000;

/// The non-loaded sections GNU `ld`'s PE scripts place after `.reloc`, in
/// the scripts' order. A debugging section not listed follows them.
const DEBUG_ORDER: &[&[u8]] = &[
    b".stab",
    b".stabstr",
    b".debug_aranges",
    b".debug_pubnames",
    b".debug_info",
    b".debug_abbrev",
    b".debug_line",
    b".debug_frame",
    b".debug_str",
    b".debug_loc",
    b".debug_macinfo",
    b".debug_weaknames",
    b".debug_funcnames",
    b".debug_typenames",
    b".debug_varnames",
    b".debug_pubtypes",
    b".debug_ranges",
    b".debug_types",
    b".debug_addr",
    b".debug_line_str",
    b".debug_loclists",
    b".debug_macro",
    b".debug_names",
    b".debug_rnglists",
    b".debug_str_offsets",
    b".debug_sup",
    b".debug_gdb_scripts",
];

/// Whether an output section holds debugging information (DWARF or
/// STABS), which the loader does not map.
#[must_use]
pub fn is_debug_section(name: &[u8]) -> bool {
    name.starts_with(b".debug") || name.starts_with(b".zdebug") || name.starts_with(b".stab")
}

/// The rank of an orphan output section: debugging sections go after
/// `.reloc` in GNU `ld`'s order (`.zdebug_*` next to its `.debug_*`), the
/// rest after the sections the recipe names.
fn orphan_rank(name: &[u8]) -> u32 {
    let plain = match name.strip_prefix(b".z") {
        Some(rest) if rest.starts_with(b"debug") => [b".".as_slice(), rest].concat(),
        _ => name.to_vec(),
    };
    if let Some(index) = DEBUG_ORDER
        .iter()
        .position(|known| *known == plain.as_slice())
    {
        return RANK_DEBUG.saturating_add(u32::try_from(index).unwrap_or(0));
    }
    if plain.starts_with(b".debug") {
        return RANK_DEBUG.saturating_add(u32::try_from(DEBUG_ORDER.len()).unwrap_or(0));
    }
    RANK_ORPHAN
}

/// The output sections under construction, with their ordering keys.
#[derive(Default)]
struct Builder {
    sections: Vec<OutSection>,
    order: Vec<Vec<Placed>>,
    pending: Vec<Vec<u32>>,
    ranks: Vec<u32>,
}

impl Builder {
    /// The index of the output section named `name`, creating it with `rank`
    /// if it does not exist. An existing section keeps its lower rank.
    fn section(&mut self, name: &[u8], rank: u32) -> usize {
        if let Some(at) = self.sections.iter().position(|out| out.name == name) {
            self.ranks[at] = self.ranks[at].min(rank);
            return at;
        }
        self.sections.push(OutSection {
            name: name.to_vec(),
            characteristics: characteristics_for(name),
            rva: 0,
            virtual_size: 0,
            raw_size: 0,
            file_offset: 0,
            chunks: Vec::new(),
            align: 1,
        });
        self.order.push(Vec::new());
        self.pending.push(Vec::new());
        self.ranks.push(rank);
        self.sections.len().saturating_sub(1)
    }
}

/// Appends an input section's chunk to `chunks`, aligned, followed by its
/// thunk block if it has one, and returns the new offset.
fn push_input(
    chunks: &mut Vec<Chunk>,
    input: &LayoutInput<'_, '_>,
    placed: Placed,
    offset: u32,
) -> u32 {
    let Some(section) = input
        .files
        .get(placed.file as usize)
        .and_then(CoffInput::object)
        .and_then(|parsed| parsed.section(placed.section))
    else {
        return offset;
    };
    let offset = align_up32(offset, section.align.max(1));
    chunks.push(Chunk {
        offset,
        size: section.size,
        piece: Piece::Input {
            file: placed.file,
            section: placed.section,
        },
    });
    let end = offset.saturating_add(section.size);
    let thunks = input.thunks.block_size(placed.file, placed.section);
    if thunks == 0 {
        return end;
    }
    let at = align_up32(end, 4);
    chunks.push(Chunk {
        offset: at,
        size: thunks,
        piece: Piece::Thunks {
            file: placed.file,
            section: placed.section,
        },
    });
    at.saturating_add(thunks)
}

/// Sorts the contributions of one output section: by recipe step, then by
/// the step's sort mode, then by input order.
fn sort_contributions(list: &mut [Placed], files: &[CoffInput<'_>]) {
    let name_of = |placed: &Placed| -> Vec<u8> {
        files
            .get(placed.file as usize)
            .and_then(CoffInput::object)
            .and_then(|parsed| parsed.section(placed.section))
            .map_or_else(Vec::new, |section| section.name.to_vec())
    };
    let file_of = |placed: &Placed| -> Vec<u8> {
        files
            .get(placed.file as usize)
            .map_or_else(Vec::new, |file| file.sort_name().to_vec())
    };
    list.sort_by_cached_key(|placed| {
        let key = match placed.sort {
            Sort::None => Vec::new(),
            Sort::ByName => name_of(placed),
            Sort::ByFile => file_of(placed),
        };
        (placed.step, key, placed.file, placed.section)
    });
}

/// The output section name of an orphan: the part before the first `$`.
fn orphan_name(name: &[u8]) -> &[u8] {
    match name.iter().position(|&byte| byte == b'$') {
        Some(at) => name.get(..at).unwrap_or(name),
        None => name,
    }
}

/// The bytes a marker inserts, for an image whose pointers are `pointer`
/// bytes wide: the constructor lists hold one pointer-sized `-1` head and
/// a null tail (`LONG (-1); LONG (-1);` in `i386pep`, `LONG (-1);` in
/// `i386pe`).
fn marker_bytes(marker: Marker, pointer: u32) -> Vec<u8> {
    match marker {
        Marker::CtorHead | Marker::DtorHead => vec![0xffu8; pointer as usize],
        Marker::CtorTail | Marker::DtorTail => vec![0u8; pointer as usize],
        // A null `IMAGE_IMPORT_DESCRIPTOR` ends the import directory.
        Marker::IdataNull => vec![0u8; 20],
        // Position-only markers for linker-defined symbols.
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alignment_helpers() {
        assert_eq!(align_up32(0, 16), 0);
        assert_eq!(align_up32(1, 16), 16);
        assert_eq!(align_up32(16, 16), 16);
        assert_eq!(align_up32(u32::MAX, 16), u32::MAX);
        assert_eq!(align_up64(513, 512), 1024);
    }

    #[test]
    fn debugging_sections_follow_reloc_in_gnu_order() {
        assert!(orphan_rank(b".debug_aranges") < orphan_rank(b".debug_info"));
        assert!(orphan_rank(b".debug_line_str") < orphan_rank(b".debug_rnglists"));
        assert_eq!(orphan_rank(b".zdebug_info"), orphan_rank(b".debug_info"));
        assert!(orphan_rank(b".debug_info") > RANK_TRAILING);
        assert!(orphan_rank(b".debug_unknown") > orphan_rank(b".debug_gdb_scripts"));
        assert_eq!(orphan_rank(b".mysection"), RANK_ORPHAN);
    }

    #[test]
    fn orphan_names_drop_the_group_suffix() {
        assert_eq!(orphan_name(b".CRT$XCA"), b".CRT");
        assert_eq!(orphan_name(b".mysection"), b".mysection");
    }

    #[test]
    fn known_characteristics_match_gnu_ld() {
        assert_eq!(characteristics_for(b".text"), 0x6000_0020);
        assert_eq!(characteristics_for(b".data"), 0xC000_0040);
        assert_eq!(characteristics_for(b".rdata"), 0x4000_0040);
        assert_eq!(characteristics_for(b".bss"), 0xC000_0080);
        assert_eq!(characteristics_for(b".reloc"), 0x4200_0040);
        assert_eq!(characteristics_for(b".tls"), 0xC000_0040);
    }
}
