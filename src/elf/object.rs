//! Relocatable input objects as the link sees them.
//!
//! [`ObjectInput`] wraps a parsed [`ObjectFile`] with what later stages need
//! per file: the global symbol names (prehashed for interning) and how each
//! one takes part in resolution, a classification of every section, the
//! relocation section of every section, the COMDAT groups, and the GNU
//! property and stack notes.
//!
//! Parsing touches only headers and the symbol table; section contents and
//! relocations stay in the mapping until they are needed.

#![deny(clippy::arithmetic_side_effects)]

use crate::elf::read::consts::{
    SHF_ALLOC, SHF_EXCLUDE, SHF_EXECINSTR, SHF_MERGE, SHF_WRITE, SHT_GROUP, SHT_LLVM_ADDRSIG,
    SHT_NOBITS, SHT_NULL, SHT_REL, SHT_RELA, SHT_STRTAB, SHT_SYMTAB, SHT_SYMTAB_SHNDX, STB_LOCAL,
    STB_WEAK,
};
use std::sync::Arc;

use crate::debug::section::{CompressedSection, ZDEBUG_PREFIX};
use crate::elf::read::consts::SHF_COMPRESSED;
use crate::elf::read::{
    Elf64Le, ElfFormat, GnuProperties, ObjectFile, SectionHeader, SectionIndex, Source,
};
use crate::error::{Error, Result};
use crate::input::FileTable;
use crate::passes::merge::{MergeKind, SplitSection, split_section};
use crate::symbols::{DefinitionKind, SymbolName, SymbolUse};

use super::resolve::AUX_COMDAT;

/// How an input section takes part in the output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SectionKind {
    /// Never copied: the null section, symbol and string tables, relocation
    /// sections, groups, `.note.GNU-stack`, `.note.gnu.property` (merged into
    /// a synthetic note), `SHF_EXCLUDE` sections, stripped debug sections.
    Ignored,
    /// Copied (or allocated, for `SHT_NOBITS`) and relocated.
    Regular,
    /// A `SHF_MERGE` section whose pieces are deduplicated.
    Merge,
    /// An `.eh_frame` section, rebuilt from its live records.
    EhFrame,
}

/// One section of an input object.
#[derive(Clone, Copy, Debug)]
pub struct InputSection<'a> {
    /// The section name.
    pub name: &'a [u8],
    /// The section header.
    pub header: SectionHeader,
    /// How the section is used.
    pub kind: SectionKind,
    /// Index of the relocation section that applies to this one, or 0.
    pub relocs: u32,
    /// Index plus one of the COMDAT group (in [`ObjectInput::groups`]) this
    /// section belongs to, or 0.
    pub group: u32,
    /// For [`SectionKind::Merge`], the index of its split in
    /// [`ObjectInput::splits`].
    pub split: u32,
    /// The decompressed contents of a compressed section, whose header then
    /// describes the decompressed data.
    pub contents: Option<&'a [u8]>,
}

impl InputSection<'_> {
    /// Whether the section occupies memory at run time.
    #[must_use]
    pub fn is_alloc(&self) -> bool {
        self.header.sh_flags & SHF_ALLOC != 0
    }

    /// Whether the section is `SHT_NOBITS`.
    #[must_use]
    pub fn is_nobits(&self) -> bool {
        self.header.sh_type == SHT_NOBITS
    }
}

/// A COMDAT group of an input object.
#[derive(Clone, Debug)]
pub struct ComdatGroup<'a> {
    /// The group signature.
    pub signature: &'a [u8],
    /// The signature as a claim key, hashed once while the object is
    /// parsed (resolution looks each group up twice).
    pub key: SymbolName<'a>,
    /// Member section indices.
    pub members: Vec<u32>,
}

/// What `--wrap` does to undefined references.
#[derive(Clone, Debug, Default)]
pub struct WrapTable {
    /// `(name, __wrap_name, __real_name)` for each wrapped symbol, sorted by
    /// name.
    entries: Vec<(Vec<u8>, Vec<u8>, Vec<u8>)>,
}

impl WrapTable {
    /// Builds the table for the `--wrap` options.
    #[must_use]
    pub fn new(names: &[String]) -> Self {
        let mut entries: Vec<(Vec<u8>, Vec<u8>, Vec<u8>)> = names
            .iter()
            .map(|name| {
                let name = name.as_bytes().to_vec();
                let mut wrapped = b"__wrap_".to_vec();
                wrapped.extend_from_slice(&name);
                let mut real = b"__real_".to_vec();
                real.extend_from_slice(&name);
                (name, wrapped, real)
            })
            .collect();
        entries.sort();
        entries.dedup();
        Self { entries }
    }

    /// Whether no symbol is wrapped.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The name an undefined reference to `name` resolves to.
    #[must_use]
    pub fn redirect<'s>(&'s self, name: &'s [u8]) -> &'s [u8] {
        if self.entries.is_empty() {
            return name;
        }
        if let Ok(found) = self.entries.binary_search_by(|e| e.0.as_slice().cmp(name))
            && let Some(entry) = self.entries.get(found)
        {
            return &entry.1;
        }
        if name.starts_with(b"__real_")
            && let Some(entry) = self.entries.iter().find(|e| e.2.as_slice() == name)
        {
            return &entry.0;
        }
        name
    }
}

/// Settings that affect how objects are read.
#[derive(Clone, Copy, Debug)]
pub struct ParseConfig<'a> {
    /// Drop debug sections (`-S`, `-s`).
    pub strip_debug: bool,
    /// `--wrap` redirections.
    pub wrap: &'a WrapTable,
    /// Where decompressed section contents are kept for the link.
    pub table: &'a FileTable,
}

/// Known `.zdebug_*` names and the names of their decompressed sections.
const ZDEBUG_NAMES: &[(&[u8], &[u8])] = &[
    (b".zdebug_abbrev", b".debug_abbrev"),
    (b".zdebug_addr", b".debug_addr"),
    (b".zdebug_aranges", b".debug_aranges"),
    (b".zdebug_frame", b".debug_frame"),
    (b".zdebug_info", b".debug_info"),
    (b".zdebug_line", b".debug_line"),
    (b".zdebug_line_str", b".debug_line_str"),
    (b".zdebug_loc", b".debug_loc"),
    (b".zdebug_loclists", b".debug_loclists"),
    (b".zdebug_macinfo", b".debug_macinfo"),
    (b".zdebug_macro", b".debug_macro"),
    (b".zdebug_names", b".debug_names"),
    (b".zdebug_pubnames", b".debug_pubnames"),
    (b".zdebug_pubtypes", b".debug_pubtypes"),
    (b".zdebug_ranges", b".debug_ranges"),
    (b".zdebug_rnglists", b".debug_rnglists"),
    (b".zdebug_str", b".debug_str"),
    (b".zdebug_str_offsets", b".debug_str_offsets"),
    (b".zdebug_types", b".debug_types"),
];

/// A decompressed section: its patched header, output name and contents.
type Decompressed<'a> = (SectionHeader, &'a [u8], &'a [u8]);

/// Decompresses a compressed non-allocated section into the file table.
fn decompress_section<'a, F: ElfFormat>(
    elf: &ObjectFile<'a, F>,
    header: &SectionHeader,
    name: &'a [u8],
    config: &ParseConfig<'a>,
) -> Result<Option<Decompressed<'a>>> {
    let Some(compressed) = CompressedSection::detect(elf, header)? else {
        return Ok(None);
    };
    let source = elf.source();
    let data = compressed.decompress(source)?;
    let label = match source.member {
        Some(member) => format!(
            "{}({member}) decompressed section at {:#x}",
            source.path.display(),
            header.sh_offset
        ),
        None => format!(
            "{} decompressed section at {:#x}",
            source.path.display(),
            header.sh_offset
        ),
    };
    let id = config.table.add_bytes(label, Arc::from(data))?;
    let contents = config
        .table
        .get(id)
        .map(crate::input::InputFile::data)
        .ok_or_else(|| Error::Internal("decompressed section missing from table".into()))?;
    let mut patched = *header;
    patched.sh_size = u64::try_from(contents.len()).unwrap_or(u64::MAX);
    patched.sh_addralign = compressed.align.max(1);
    patched.sh_flags &= !SHF_COMPRESSED;
    let name = if compressed.zdebug {
        ZDEBUG_NAMES
            .iter()
            .find(|(z, _)| *z == name)
            .map_or(name, |&(_, plain)| plain)
    } else {
        name
    };
    Ok(Some((patched, name, contents)))
}

/// A parsed input object.
#[derive(Debug)]
pub struct ObjectInput<'a, F: ElfFormat = Elf64Le> {
    /// The underlying ELF reader.
    pub elf: ObjectFile<'a, F>,
    /// Index of the first global symbol.
    pub first_global: usize,
    /// Global symbol names, in symbol table order from `first_global`.
    pub names: Vec<SymbolName<'a>>,
    /// How each global symbol takes part in resolution.
    pub uses: Vec<SymbolUse>,
    /// Every section, by section index.
    pub sections: Vec<InputSection<'a>>,
    /// COMDAT groups.
    pub groups: Vec<ComdatGroup<'a>>,
    /// Whether a `.note.GNU-stack` section was present.
    pub has_gnu_stack_note: bool,
    /// Whether `.note.GNU-stack` requests an executable stack.
    pub exec_stack: bool,
    /// GNU properties, when the object has a `.note.gnu.property`.
    pub properties: Option<GnuProperties>,
    /// Index of the `.llvm_addrsig` section, or 0.
    pub addrsig: u32,
    /// Indices of `.gnu.warning*` sections.
    pub warnings: Vec<u32>,
    /// The pieces of every [`SectionKind::Merge`] section, split at parse
    /// time.
    pub splits: Vec<SplitSection<'a>>,
    /// Whether some global symbol is named `name@@VERSION`.
    pub has_default_versions: bool,
    /// For each of [`groups`](Self::groups), whether another file's copy
    /// was kept and this one is discarded.
    pub discarded_groups: Vec<bool>,
    /// The global symbols [`discard_groups`](Self::discard_groups) turned
    /// into [`SymbolUse::Ignore`], with their original uses. LTO reports
    /// them as used by a regular object: their references bind to the kept
    /// copy.
    pub group_ignored: Vec<(u32, SymbolUse)>,
    /// Whether the object carries GCC LTO IR (`.gnu.lto_*` sections).
    pub gcc_lto: GccLto,
    /// For each of [`groups`](Self::groups), its claim slot (see
    /// [`ComdatHook`](super::resolve::ComdatHook)), once looked up; taken
    /// when the claims of the object's round are settled.
    pub group_slots: Option<Vec<u32>>,
    /// The rank the object offered its groups with, in the round that made
    /// it live.
    pub claim_rank: Option<u32>,
}

/// Whether an ELF object carries GCC LTO IR.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum GccLto {
    /// No `.gnu.lto_*` sections: an ordinary object.
    #[default]
    None,
    /// IR only (`-flto` without `-ffat-lto-objects`): the object defines
    /// the `__gnu_lto_slim` marker and has no code of its own.
    Slim,
    /// IR next to native code (`-ffat-lto-objects`): linkable without the
    /// plugin.
    Fat,
}

/// The common symbol GCC puts in slim LTO objects.
pub const GCC_LTO_SLIM_MARKER: &[u8] = b"__gnu_lto_slim";
/// The section name prefix of GCC LTO IR.
pub const GCC_LTO_PREFIX: &[u8] = b".gnu.lto_";

/// Whether a section name is debug information that `--strip-debug` drops.
#[must_use]
pub fn is_debug_name(name: &[u8]) -> bool {
    name.starts_with(b".debug")
        || name.starts_with(b".zdebug")
        || name.starts_with(b".gnu.debuglto_")
        || name == b".line"
        || name.starts_with(b".stab")
}

impl<'a, F: ElfFormat> ObjectInput<'a, F> {
    /// Parses an object and prepares its symbols for resolution.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Malformed`] for malformed files, and
    /// [`Error::Unimplemented`] for objects qld cannot link yet.
    pub fn parse(data: &'a [u8], source: Source<'a>, config: &ParseConfig<'a>) -> Result<Self> {
        let elf = ObjectFile::<F>::parse(data, source)?;
        if crate::elf::arch::Arch::from_machine(elf.elf().header().e_machine).is_none() {
            return Err(source.malformed(18, "ELF machine (not an architecture qld links)"));
        }

        let count = elf.section_count();
        let mut sections = Vec::with_capacity(count);
        let mut has_gnu_stack_note = false;
        let mut exec_stack = false;
        let mut has_property_note = false;
        let mut addrsig = 0u32;
        let mut warnings = Vec::new();
        let mut has_lto_ir = false;
        for (index, header) in elf.elf().enumerate_sections() {
            let mut name = elf.section_name(&header)?;
            has_lto_ir |= name.starts_with(GCC_LTO_PREFIX);
            let mut header = header;
            let mut contents = None;
            let stripped =
                config.strip_debug && header.sh_flags & SHF_ALLOC == 0 && is_debug_name(name);
            if !stripped && (header.is_compressed() || name.starts_with(ZDEBUG_PREFIX)) {
                if header.sh_flags & SHF_ALLOC != 0 {
                    return Err(source.malformed(
                        elf.elf().section_header_offset(index),
                        "section flags (SHF_COMPRESSED on an allocated section)",
                    ));
                }
                if let Some((patched, output_name, data)) =
                    decompress_section(&elf, &header, name, config)?
                {
                    header = patched;
                    name = output_name;
                    contents = Some(data);
                }
            }
            let flags = header.sh_flags;
            let kind = match header.sh_type {
                SHT_NULL | SHT_SYMTAB | SHT_STRTAB | SHT_REL | SHT_RELA | SHT_GROUP
                | SHT_SYMTAB_SHNDX => SectionKind::Ignored,
                SHT_LLVM_ADDRSIG => {
                    addrsig = index;
                    SectionKind::Ignored
                }
                _ if flags & SHF_EXCLUDE != 0 => SectionKind::Ignored,
                _ if name == b".note.GNU-stack" => {
                    has_gnu_stack_note = true;
                    exec_stack |= flags & SHF_EXECINSTR != 0;
                    SectionKind::Ignored
                }
                _ if name == b".note.gnu.property" => {
                    has_property_note = true;
                    SectionKind::Ignored
                }
                _ if name.starts_with(b".gnu.warning") => {
                    // `.gnu.warning.SYM`: a message for links that use SYM.
                    warnings.push(index);
                    SectionKind::Ignored
                }
                _ if config.strip_debug && flags & SHF_ALLOC == 0 && is_debug_name(name) => {
                    SectionKind::Ignored
                }
                _ if elf.is_eh_frame(&header)? && flags & SHF_ALLOC != 0 => SectionKind::EhFrame,
                _ if flags & SHF_MERGE != 0
                    && header.sh_entsize != 0
                    && flags & SHF_WRITE == 0
                    && header.sh_type != SHT_NOBITS =>
                {
                    SectionKind::Merge
                }
                _ => SectionKind::Regular,
            };
            if kind != SectionKind::Ignored {
                // Layout trusts the sizes of copied sections: check now that
                // their contents lie inside the file.
                if contents.is_none() {
                    elf.section_data(&header)?;
                }
                if header.sh_addralign > 1 && !header.sh_addralign.is_power_of_two() {
                    return Err(source.malformed(
                        elf.elf().section_header_offset(index),
                        "section alignment (not a power of two)",
                    ));
                }
            }
            sections.push(InputSection {
                name,
                header,
                kind,
                relocs: 0,
                group: 0,
                split: 0,
                contents,
            });
        }

        // Relocation sections: attach them to their targets. A merge section
        // with relocations is copied as a regular section instead.
        for (index, header) in elf.elf().enumerate_sections() {
            if !matches!(header.sh_type, SHT_REL | SHT_RELA) {
                continue;
            }
            let target = usize::try_from(header.sh_info)
                .ok()
                .filter(|&t| t != 0)
                .and_then(|t| sections.get_mut(t));
            let Some(target) = target else {
                // GNU as emits relocation sections for nothing only in broken
                // objects; treat a zero target as malformed.
                return Err(source.malformed(
                    elf.elf().section_header_offset(index),
                    "relocation section target",
                ));
            };
            if header.sh_link != elf.symbols().section_index() && header.sh_link != 0 {
                return Err(source.malformed(
                    elf.elf().section_header_offset(index),
                    "relocation section symbol table link",
                ));
            }
            if target.relocs != 0 {
                return Err(source.malformed(
                    elf.elf().section_header_offset(index),
                    "relocation section target (duplicate)",
                ));
            }
            target.relocs = index;
            if target.kind == SectionKind::Merge {
                target.kind = SectionKind::Regular;
            }
        }

        // COMDAT groups.
        let mut groups = Vec::new();
        for group in elf.groups() {
            let group = group?;
            if !group.is_comdat() {
                continue;
            }
            let signature = elf.group_signature(&group)?;
            let group_number = u32::try_from(groups.len())
                .ok()
                .and_then(|n| n.checked_add(1))
                .ok_or_else(|| source.malformed(group.header.sh_offset, "section group count"))?;
            let mut members = Vec::with_capacity(group.member_count());
            for member in group.members() {
                let section = usize::try_from(member)
                    .ok()
                    .and_then(|m| sections.get_mut(m))
                    .filter(|_| member != 0)
                    .ok_or_else(|| {
                        source.malformed(group.header.sh_offset, "section group member")
                    })?;
                if section.group != 0 {
                    return Err(source.malformed(
                        group.header.sh_offset,
                        "section group member (in two groups)",
                    ));
                }
                section.group = group_number;
                members.push(member);
            }
            groups.push(ComdatGroup {
                signature,
                key: SymbolName::new(signature),
                members,
            });
        }

        // Split mergeable sections into pieces now, while the file is hot.
        let mut splits = Vec::new();
        for section in &mut sections {
            if section.kind != SectionKind::Merge {
                continue;
            }
            let Some(kind) = merge_kind(&section.header) else {
                section.kind = SectionKind::Regular;
                continue;
            };
            let data = match section.contents {
                Some(contents) => contents,
                None => elf.section_data(&section.header)?,
            };
            let alignment = section.header.sh_addralign.max(1);
            if !alignment.is_power_of_two() {
                section.kind = SectionKind::Regular;
                continue;
            }
            let split = split_section(data, kind, alignment).map_err(|malformed| {
                let mut error = malformed.into_error(source.path, section.header.sh_offset);
                if let Error::Malformed { member, .. } = &mut error {
                    *member = source.member.map(str::to_owned);
                }
                error
            })?;
            section.split = u32::try_from(splits.len())
                .map_err(|_| Error::Limit("too many merge sections in one object".into()))?;
            splits.push(split);
        }

        let properties = if has_property_note {
            Some(elf.gnu_properties()?)
        } else {
            None
        };

        let symbols = *elf.symbols();
        let first_global = symbols.first_global();
        let global_count = symbols.len().saturating_sub(first_global);
        let mut names = Vec::with_capacity(global_count);
        let mut uses = Vec::with_capacity(global_count);
        let mut has_default_versions = false;
        let mut slim = false;
        for index in first_global..symbols.len() {
            let Some(raw) = symbols.get_raw(index) else {
                break;
            };
            let name = symbols.name(index, &raw)?;
            slim |= has_lto_ir && name == GCC_LTO_SLIM_MARKER;
            let section = symbols.section(index, &raw)?;
            let binding = raw.binding();
            let weak = binding == STB_WEAK;
            let (name, use_) = match section {
                _ if binding == STB_LOCAL => (name, SymbolUse::Ignore),
                SectionIndex::Undefined => {
                    (config.wrap.redirect(name), SymbolUse::Reference { weak })
                }
                SectionIndex::Common => (
                    name,
                    SymbolUse::Definition {
                        kind: DefinitionKind::Common,
                        aux: raw.st_size & !AUX_COMDAT,
                    },
                ),
                SectionIndex::Absolute => (name, definition(weak, false)),
                SectionIndex::Section(s) => {
                    let section = usize::try_from(s)
                        .ok()
                        .and_then(|s| sections.get(s))
                        .ok_or_else(|| {
                            source.malformed(
                                symbols.strtab().file_offset(),
                                format!("section index {s} of symbol {index}"),
                            )
                        })?;
                    (name, definition(weak, section.group != 0))
                }
                SectionIndex::Reserved(_) => (name, SymbolUse::Ignore),
            };
            // `redirect` may return a name owned by the wrap table, which
            // lives as long as the link.
            let (base, version) = split_version(name);
            has_default_versions |= version.is_none() && base.len() < name.len();
            names.push(SymbolName::with_version(base, version));
            uses.push(use_);
        }

        Ok(Self {
            elf,
            first_global,
            names,
            uses,
            sections,
            groups,
            has_gnu_stack_note,
            exec_stack,
            properties,
            addrsig,
            warnings,
            splits,
            has_default_versions,
            discarded_groups: Vec::new(),
            group_ignored: Vec::new(),
            group_slots: None,
            claim_rank: None,
            gcc_lto: match (has_lto_ir, slim) {
                (false, _) => GccLto::None,
                (true, true) => GccLto::Slim,
                (true, false) => GccLto::Fat,
            },
        })
    }

    /// Discards the COMDAT groups flagged in `discarded` (by index in
    /// [`groups`](Self::groups)): their global definitions stop taking part
    /// in resolution, so references bind to the kept copy.
    pub fn discard_groups(&mut self, discarded: Vec<bool>) {
        let symbols = *self.elf.symbols();
        for (local, use_) in self.uses.iter_mut().enumerate() {
            if !matches!(use_, SymbolUse::Definition { .. }) {
                continue;
            }
            let Some(index) = local.checked_add(self.first_global) else {
                break;
            };
            let Some(raw) = symbols.get_raw(index) else {
                break;
            };
            let Ok(SectionIndex::Section(section)) = symbols.section(index, &raw) else {
                continue;
            };
            let group = self
                .sections
                .get(section as usize)
                .and_then(|s| s.group.checked_sub(1));
            if group.is_some_and(|g| discarded.get(g as usize).copied().unwrap_or(false)) {
                if let Ok(local) = u32::try_from(local) {
                    self.group_ignored.push((local, *use_));
                }
                *use_ = SymbolUse::Ignore;
            }
        }
        self.discarded_groups = discarded;
    }

    /// For global symbol `local` (an index into [`names`](Self::names)) named
    /// `name@@VERSION`, the version.
    #[must_use]
    pub fn default_version(&self, local: usize) -> Option<&'a [u8]> {
        if !self.has_default_versions {
            return None;
        }
        let symbols = self.elf.symbols();
        let index = local.checked_add(self.first_global)?;
        let raw = symbols.get_raw(index)?;
        let name = symbols.name(index, &raw).ok()?;
        let at = name.windows(2).position(|w| w == b"@@")?;
        name.get(at.checked_add(2)?..)
    }

    /// The contents of `section`: decompressed for compressed sections.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Malformed`] if the contents lie outside the file.
    pub fn section_data(&self, section: &InputSection<'a>) -> Result<&'a [u8]> {
        match section.contents {
            Some(contents) => Ok(contents),
            None => self.elf.section_data(&section.header),
        }
    }

    /// The object's source, for diagnostics.
    #[must_use]
    pub fn source(&self) -> Source<'a> {
        self.elf.source()
    }

    /// The input section `index`, if it exists.
    #[must_use]
    pub fn section(&self, index: u32) -> Option<&InputSection<'a>> {
        self.sections.get(usize::try_from(index).ok()?)
    }

    /// Returns an error naming this file.
    #[must_use]
    pub fn malformed(&self, offset: u64, what: impl Into<String>) -> Error {
        self.source().malformed(offset, what)
    }
}

/// How a mergeable section splits, or `None` if it cannot be merged.
fn merge_kind(header: &SectionHeader) -> Option<MergeKind> {
    use crate::elf::read::consts::SHF_STRINGS;
    if header.sh_flags & SHF_STRINGS != 0 {
        let char_size = u8::try_from(header.sh_entsize).ok()?;
        matches!(char_size, 1 | 2 | 4).then_some(MergeKind::Strings { char_size })
    } else {
        (header.sh_entsize != 0).then_some(MergeKind::Fixed {
            entry_size: header.sh_entsize,
        })
    }
}

fn definition(weak: bool, comdat: bool) -> SymbolUse {
    SymbolUse::Definition {
        kind: if weak {
            DefinitionKind::Weak
        } else {
            DefinitionKind::Regular
        },
        aux: if comdat { AUX_COMDAT } else { 0 },
    }
}

/// Splits a symbol table name into the name and its explicit version:
/// `foo@@VERSION` is the default version of `foo` (the plain name, returned
/// without a version), `foo@VERSION` a distinct, versioned symbol.
#[must_use]
pub fn split_version(name: &[u8]) -> (&[u8], Option<&[u8]>) {
    let Some(at) = crate::elf::read::strtab::find_byte(name, b'@') else {
        return (name, None);
    };
    let base = name.get(..at).unwrap_or(name);
    if base.is_empty() {
        return (name, None);
    }
    let rest = name.get(at.saturating_add(1)..).unwrap_or_default();
    match rest.strip_prefix(b"@") {
        Some(_) => (base, None),
        None if rest.is_empty() => (name, None),
        None => (base, Some(rest)),
    }
}
