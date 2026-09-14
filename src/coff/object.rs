//! A COFF object as the link driver sees it: sections, COMDAT groups and a
//! flat symbol list for [`crate::symbols`].
//!
//! Parsing happens once, when resolution makes the file live. It decodes the
//! section table into [`InputSection`]s, classifies each section (dropped,
//! kept, COMDAT), and splits the symbol table into
//! [`globals`](ParsedObject::globals) — the entries resolution sees — and a
//! per-record target table relocations use.

#![deny(clippy::arithmetic_side_effects)]

use crate::error::Result;
use crate::symbols::{DefinitionKind, SymbolName, SymbolUse};

use super::read::consts::{
    IMAGE_COMDAT_SELECT_ANY, IMAGE_COMDAT_SELECT_ASSOCIATIVE, IMAGE_SCN_LNK_INFO,
    IMAGE_SYM_CLASS_EXTERNAL, IMAGE_SYM_CLASS_WEAK_EXTERNAL, IMAGE_WEAK_EXTERN_ANTI_DEPENDENCY,
    IMAGE_WEAK_EXTERN_SEARCH_LIBRARY,
};
use super::read::{CoffObject, SectionHeader, SectionNumber, Symbol};

/// Alignment used for a section whose header gives none. MSVC and GNU `ld`
/// use 16 bytes; lld uses 1.
pub const DEFAULT_ALIGNMENT: u32 = 16;

/// Whether the linker keeps a section from an input object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SectionKind {
    /// Contributes to the image.
    Regular,
    /// `.drectve`: read for directives, never emitted.
    Directive,
    /// Dropped: `IMAGE_SCN_LNK_REMOVE`, CodeView (`.debug$*`), LTO IR
    /// leftovers and `.llvm_addrsig`.
    Dropped,
}

/// COMDAT information attached to a section.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Comdat<'a> {
    /// `IMAGE_COMDAT_SELECT_*`.
    pub selection: u8,
    /// The name that identifies the group: the first external symbol defined
    /// in the section.
    pub key: SymbolName<'a>,
    /// For `IMAGE_COMDAT_SELECT_ASSOCIATIVE`, the section it follows.
    pub associative: u32,
    /// `CheckSum` from the section definition, for `EXACT_MATCH`.
    pub check_sum: u32,
    /// `Length` from the section definition, for `SAME_SIZE` and `LARGEST`.
    pub length: u32,
}

/// A relocation of a synthetic section, which has no COFF relocation table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SynthReloc {
    /// Offset of the relocated field in the section.
    pub offset: u32,
    /// The machine relocation type.
    pub r_type: u16,
    /// Index into [`ParsedObject::globals`] of the target symbol.
    pub target: u32,
}

/// One input section.
#[derive(Clone, Debug)]
pub struct InputSection<'a> {
    /// The 1-based COFF section number.
    pub number: u32,
    /// The resolved name.
    pub name: std::borrow::Cow<'a, [u8]>,
    /// The decoded header.
    pub header: SectionHeader,
    /// The raw contents (empty for `.bss`-style sections).
    pub data: std::borrow::Cow<'a, [u8]>,
    /// Relocations of a synthetic section; real sections use the COFF
    /// relocation table instead.
    pub relocs: Vec<SynthReloc>,
    /// Whether [`relocs`](Self::relocs) replaces the COFF relocation table.
    pub synthetic: bool,
    /// Size in the image.
    pub size: u32,
    /// Required alignment in bytes.
    pub align: u32,
    /// How the linker treats the section.
    pub kind: SectionKind,
    /// COMDAT information, for `IMAGE_SCN_LNK_COMDAT` sections.
    pub comdat: Option<Comdat<'a>>,
    /// Set when another copy of the COMDAT group was kept.
    pub discarded: bool,
    /// Set by `--gc-sections`; always true without it.
    pub live: bool,
    /// Index of the output section, once placed.
    pub output: u32,
    /// RVA of the section's contents, once laid out.
    pub rva: u32,
}

impl InputSection<'_> {
    /// Whether the section holds no file bytes (`.bss`).
    #[must_use]
    pub fn is_bss(&self) -> bool {
        self.header.is_uninitialized_data() || self.header.pointer_to_raw_data == 0
    }

    /// Whether the section takes part in layout.
    #[must_use]
    pub fn is_live(&self) -> bool {
        self.kind == SectionKind::Regular && !self.discarded && self.live
    }
}

/// What a symbol record refers to, for relocation resolution.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordTarget {
    /// Not a symbol record start, or a record the linker ignores.
    None,
    /// Entry `index` of [`ParsedObject::globals`].
    Global(u32),
    /// A local definition in section `section` at `value`.
    Local {
        /// 1-based COFF section number.
        section: u32,
        /// Offset in the section.
        value: u32,
    },
    /// An absolute value.
    Absolute(u32),
}

/// How a global symbol is defined in an object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GlobalKind {
    /// Defined in section `section` at `value`.
    Defined {
        /// 1-based COFF section number.
        section: u32,
        /// Offset in the section.
        value: u32,
    },
    /// An absolute (`IMAGE_SYM_ABSOLUTE`) definition.
    Absolute(u32),
    /// A common symbol of `size` bytes.
    Common(u32),
    /// An undefined external.
    Undefined,
    /// A weak external: undefined, with a fallback definition.
    Weak {
        /// Symbol record index of the default definition.
        tag: u32,
        /// `IMAGE_WEAK_EXTERN_*` search type.
        characteristics: u32,
    },
}

/// One global symbol of an object.
#[derive(Clone, Copy, Debug)]
pub struct Global<'a> {
    /// The interned name.
    pub name: SymbolName<'a>,
    /// The symbol table record index.
    pub record: u32,
    /// What it defines, if anything.
    pub kind: GlobalKind,
    /// Alignment requested for a common symbol (`-aligncomm:`), log2.
    pub align_log2: u32,
    /// Set when the symbol's section is a discarded COMDAT copy.
    pub discarded: bool,
}

/// A parsed COFF object.
#[derive(Debug)]
pub struct ParsedObject<'a> {
    /// The zero-copy reader, for files that come from a real COFF object.
    pub object: Option<CoffObject<'a>>,
    /// Sections, indexed by `number - 1`.
    pub sections: Vec<InputSection<'a>>,
    /// The symbols resolution sees, in symbol table order.
    pub globals: Vec<Global<'a>>,
    /// Names of [`globals`](Self::globals), for interning.
    pub names: Vec<SymbolName<'a>>,
    /// How each global takes part in resolution.
    pub uses: Vec<SymbolUse>,
    /// One entry per symbol table record.
    pub targets: Vec<RecordTarget>,
    /// The `.drectve` contents, in section order.
    pub directives: Vec<&'a [u8]>,
    /// Whether the object declares SafeSEH compatibility (`@feat.00`).
    pub safe_seh: bool,
}

impl<'a> ParsedObject<'a> {
    /// Parses `object` into the linker's model.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Malformed`](crate::Error::Malformed) if a section,
    /// symbol or section name cannot be decoded.
    pub fn parse(object: CoffObject<'a>) -> Result<Self> {
        let count = object.section_count();
        let mut sections = Vec::with_capacity(count as usize);
        let mut directives = Vec::new();
        for number in 1..=count {
            let section = object.section(number)?;
            let header = section.header;
            let kind = classify(section.name, &header);
            let data = if header.is_uninitialized_data() {
                &[][..]
            } else {
                object.section_data(&header)?
            };
            if kind == SectionKind::Directive {
                directives.push(data);
            }
            sections.push(InputSection {
                number,
                name: std::borrow::Cow::Borrowed(section.name),
                header,
                data: std::borrow::Cow::Borrowed(data),
                relocs: Vec::new(),
                synthetic: false,
                size: header.size_of_raw_data,
                align: header.alignment().unwrap_or(DEFAULT_ALIGNMENT).min(8192),
                kind,
                comdat: None,
                discarded: false,
                live: true,
                output: u32::MAX,
                rva: 0,
            });
        }

        let records = object.symbols().len();
        let mut targets = vec![RecordTarget::None; records as usize];
        let mut globals = Vec::new();
        // Section definitions carry the COMDAT selection; the group key is
        // the first external symbol defined in the section, which follows.
        let mut selections: Vec<Option<(u8, u32, u32, u32)>> = vec![None; sections.len()];
        let mut symbols = Vec::new();
        for symbol in object.symbols().iter() {
            let symbol = symbol?;
            if let Some(definition) = symbol.section_definition()
                && let SectionNumber::Section(number) = symbol.section()
                && let Some(slot) = index_of(&mut selections, number)
                && slot.is_none()
            {
                *slot = Some((
                    definition.selection,
                    definition.number,
                    definition.check_sum,
                    definition.length,
                ));
            }
            symbols.push(symbol);
        }

        for symbol in &symbols {
            let record = symbol.index;
            let target = match classify_symbol(symbol) {
                Some(kind) => {
                    let index = u32::try_from(globals.len()).unwrap_or(u32::MAX);
                    globals.push(Global {
                        name: SymbolName::new(symbol.name),
                        record,
                        kind,
                        align_log2: 0,
                        discarded: false,
                    });
                    RecordTarget::Global(index)
                }
                None => match symbol.section() {
                    SectionNumber::Section(number) => RecordTarget::Local {
                        section: number,
                        value: symbol.value,
                    },
                    SectionNumber::Absolute => RecordTarget::Absolute(symbol.value),
                    _ => RecordTarget::None,
                },
            };
            if let Some(slot) = targets.get_mut(record as usize) {
                *slot = target;
            }
        }

        // Attach COMDAT information now that the globals are known.
        for section in &mut sections {
            if !section.header.is_comdat() {
                continue;
            }
            let Some(Some((selection, number, check_sum, length))) = selections
                .get(section.number.wrapping_sub(1) as usize)
                .copied()
            else {
                continue;
            };
            let Some(key) = comdat_key(&symbols, section.number) else {
                continue;
            };
            section.comdat = Some(Comdat {
                selection: if selection == 0 {
                    IMAGE_COMDAT_SELECT_ANY
                } else {
                    selection
                },
                key: SymbolName::new(key),
                associative: if selection == IMAGE_COMDAT_SELECT_ASSOCIATIVE {
                    number
                } else {
                    0
                },
                check_sum,
                length,
            });
        }

        let names = globals.iter().map(|global| global.name).collect();
        let uses = globals.iter().map(|global| symbol_use(global)).collect();
        let safe_seh = object.feat00()?.is_some_and(super::read::Feat00::safe_seh);
        Ok(Self {
            object: Some(object),
            sections,
            globals,
            names,
            uses,
            targets,
            directives,
            safe_seh,
        })
    }

    /// The section with COFF number `number` (1-based).
    #[must_use]
    pub fn section(&self, number: u32) -> Option<&InputSection<'a>> {
        self.sections.get(number.checked_sub(1)? as usize)
    }

    /// Marks the sections of the COMDAT groups this file lost as discarded,
    /// and makes their definitions invisible to resolution.
    ///
    /// `keeps` answers whether this file keeps the group with the given key.
    pub fn discard_lost_comdats(&mut self, keeps: &dyn Fn(&SymbolName<'a>) -> bool) {
        let mut any = false;
        for index in 0..self.sections.len() {
            let Some(comdat) = self.sections[index].comdat else {
                continue;
            };
            // An associative section follows its parent's fate.
            let discarded = if comdat.selection == IMAGE_COMDAT_SELECT_ASSOCIATIVE {
                self.section(comdat.associative)
                    .is_some_and(|parent| parent.discarded)
            } else {
                !keeps(&comdat.key)
            };
            if discarded {
                self.sections[index].discarded = true;
                any = true;
            }
        }
        if !any {
            return;
        }
        for (index, global) in self.globals.iter_mut().enumerate() {
            let GlobalKind::Defined { section, .. } = global.kind else {
                continue;
            };
            let lost = section
                .checked_sub(1)
                .and_then(|i| self.sections.get(i as usize))
                .is_some_and(|section| section.discarded);
            if lost {
                global.discarded = true;
                if let Some(slot) = self.uses.get_mut(index) {
                    *slot = SymbolUse::Ignore;
                }
            }
        }
    }

    /// Applies an `-aligncomm:` directive to a common symbol.
    pub fn align_common(&mut self, name: &[u8], align_log2: u32) {
        for global in &mut self.globals {
            if global.name.bytes() == name && matches!(global.kind, GlobalKind::Common(_)) {
                global.align_log2 = global.align_log2.max(align_log2);
            }
        }
    }
}

/// A mutable slot for 1-based section `number`.
fn index_of<T>(slots: &mut [T], number: u32) -> Option<&mut T> {
    slots.get_mut(number.checked_sub(1)? as usize)
}

/// The COMDAT group key of section `number`: the first external symbol
/// defined in it.
fn comdat_key<'a>(symbols: &[Symbol<'a>], number: u32) -> Option<&'a [u8]> {
    symbols
        .iter()
        .find(|symbol| {
            symbol.section() == SectionNumber::Section(number)
                && matches!(
                    symbol.storage_class,
                    IMAGE_SYM_CLASS_EXTERNAL | IMAGE_SYM_CLASS_WEAK_EXTERNAL
                )
        })
        .map(|symbol| symbol.name)
}

/// Whether the linker keeps, reads or drops a section.
fn classify(name: &[u8], header: &SectionHeader) -> SectionKind {
    if name == super::read::DRECTVE_SECTION {
        return SectionKind::Directive;
    }
    if header.is_remove() || header.has(IMAGE_SCN_LNK_INFO) {
        return SectionKind::Dropped;
    }
    let dropped = name.starts_with(b".debug$")
        || name.starts_with(b".gnu.lto_")
        || name == super::read::ADDRSIG_SECTION
        || name == b".gnu_object_only"
        || name == b".note.GNU-stack"
        || name == b".chks64";
    if dropped {
        SectionKind::Dropped
    } else {
        SectionKind::Regular
    }
}

/// What a symbol record defines, or `None` if it is not a global.
fn classify_symbol(symbol: &Symbol<'_>) -> Option<GlobalKind> {
    if symbol.is_weak_external() {
        let aux = symbol.weak_external().unwrap_or_default();
        return Some(GlobalKind::Weak {
            tag: aux.tag_index,
            characteristics: aux.characteristics,
        });
    }
    if !symbol.is_external() {
        return None;
    }
    Some(match symbol.section() {
        SectionNumber::Section(number) => GlobalKind::Defined {
            section: number,
            value: symbol.value,
        },
        SectionNumber::Absolute => GlobalKind::Absolute(symbol.value),
        SectionNumber::Undefined if symbol.value != 0 => GlobalKind::Common(symbol.value),
        SectionNumber::Undefined => GlobalKind::Undefined,
        SectionNumber::Debug | SectionNumber::Reserved(_) => return None,
    })
}

/// How a global takes part in resolution.
fn symbol_use(global: &Global<'_>) -> SymbolUse {
    match global.kind {
        GlobalKind::Defined { .. } | GlobalKind::Absolute(_) => SymbolUse::Definition {
            kind: DefinitionKind::Regular,
            aux: 0,
        },
        GlobalKind::Common(size) => SymbolUse::Definition {
            kind: DefinitionKind::Common,
            aux: u64::from(size),
        },
        GlobalKind::Undefined => SymbolUse::Reference { weak: false },
        // A weak external only searches archives with `SEARCH_LIBRARY`;
        // the anti-dependency form never does.
        GlobalKind::Weak {
            characteristics, ..
        } => SymbolUse::Reference {
            weak: characteristics != IMAGE_WEAK_EXTERN_SEARCH_LIBRARY
                || characteristics == IMAGE_WEAK_EXTERN_ANTI_DEPENDENCY,
        },
    }
}
