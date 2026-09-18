//! Output section assignment rules.
//!
//! The default layout is GNU ld's built-in `elf_x86_64` script for static
//! executables (`ld --verbose`), expressed as data: an ordered list of output
//! sections, each with input section descriptions (name patterns, `KEEP`,
//! sorting, file exclusions). An input section goes to the first description
//! that matches it, in rule order, exactly like a `SECTIONS` command; a
//! section that matches nothing is an *orphan*, placed after the output
//! section whose flags it resembles, as GNU ld does.
//!
//! Patterns use the linker script matcher ([`crate::script::Pattern`]), so a
//! parsed `SECTIONS` command (roadmap M3) can produce the same [`RuleSet`]
//! and drive the rest of layout unchanged.

#![deny(clippy::arithmetic_side_effects)]

use crate::elf::read::consts::{
    SHF_ALLOC, SHF_EXECINSTR, SHF_TLS, SHF_WRITE, SHT_NOBITS, SHT_NOTE,
};
use crate::script::{Pattern, init_priority};

/// How the input sections matched by one description are ordered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortMode {
    /// Input order.
    None,
    /// `SORT_BY_NAME`.
    Name,
    /// `SORT_BY_INIT_PRIORITY`.
    InitPriority,
}

/// Which files a description applies to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileFilter {
    /// Every file.
    Any,
    /// Every file, except that `.ctors` and `.dtors` sections of
    /// `crtbegin*.o` and `crtend*.o` do not match: GNU ld's
    /// `*(.init_array EXCLUDE_FILE (*crtbegin.o …) .ctors)`, where the
    /// exclusion applies to the patterns after it only.
    NotCrtBeginEnd,
    /// Only `crtbegin.o` and `crtbegin?.o`.
    CrtBegin,
}

/// One input section description.
#[derive(Clone, Copy, Debug)]
pub struct InputRule {
    /// Section name patterns.
    pub patterns: &'static [&'static str],
    /// Ordering.
    pub sort: SortMode,
    /// Files it applies to.
    pub files: FileFilter,
}

/// What synthetic content an output section may hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Synthetic {
    /// No synthetic content.
    None,
    /// `.note.gnu.build-id`.
    BuildId,
    /// `.interp`: the program interpreter path.
    Interp,
    /// `.hash`: the System V symbol hash table.
    Hash,
    /// `.gnu.hash`: the GNU symbol hash table.
    GnuHash,
    /// `.dynsym`.
    DynSym,
    /// `.dynstr`.
    DynStr,
    /// `.gnu.version`: the version of each dynamic symbol.
    VerSym,
    /// `.gnu.version_d`: version definitions.
    VerDef,
    /// `.gnu.version_r`: version requirements.
    VerNeed,
    /// `.rela.dyn`.
    RelaDyn,
    /// `.rela.plt`: `JUMP_SLOT` relocations of a dynamic output, and the
    /// `IRELATIVE` relocations of IFUNC PLT entries (all a static
    /// executable has).
    RelaPlt,
    /// `.relr.dyn`: packed relative relocations.
    RelrDyn,
    /// `.plt`: the lazy PLT of a dynamic output, or the IFUNC stubs of a
    /// static executable.
    Plt,
    /// `.plt.got`: PLT entries that jump through a GOT entry.
    PltGot,
    /// `.plt.sec`: the IBT-enabled second PLT.
    PltSec,
    /// `.eh_frame_hdr`.
    EhFrameHdr,
    /// `.note.gnu.property`.
    GnuProperty,
    /// Space for copy relocations of read-only data, in `.data.rel.ro`.
    DynRelro,
    /// `.dynamic`.
    Dynamic,
    /// The GOT.
    Got,
    /// `.got.plt`: the dynamic linker's reserved words and the PLT slots.
    GotPlt,
    /// Space for copy relocations, at the start of `.bss`.
    DynBss,
    /// Common symbols, at the end of `.bss`.
    Common,
    /// Linker identification appended to `.comment`.
    Comment,
    /// The zero terminator at the end of `.eh_frame`.
    EhFrameEnd,
}

/// Where a class of orphan sections goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum OrphanClass {
    /// Allocated notes.
    Note,
    /// Executable code.
    Text,
    /// Read-only data.
    Rodata,
    /// TLS data.
    Tdata,
    /// TLS zero-initialized data.
    Tbss,
    /// Writable data.
    Data,
    /// Zero-initialized data.
    Bss,
    /// Not allocated.
    NonAlloc,
}

/// One output section of the default layout.
#[derive(Clone, Copy, Debug)]
pub struct OutputRule {
    /// Output section name.
    pub name: &'static str,
    /// Input section descriptions, in order.
    pub inputs: &'static [InputRule],
    /// Matched sections are GC roots (`KEEP`).
    pub keep: bool,
    /// Synthetic content, placed before (or, for `Common`, `Comment` and
    /// `EhFrameEnd`, after) the input sections.
    pub synthetic: Synthetic,
    /// Orphans of this class go right after this output section.
    pub hold: Option<OrphanClass>,
    /// The section is read-only after relocation (`PT_GNU_RELRO`).
    pub relro: bool,
}

const fn plain(patterns: &'static [&'static str]) -> InputRule {
    InputRule {
        patterns,
        sort: SortMode::None,
        files: FileFilter::Any,
    }
}

const fn rule(name: &'static str, inputs: &'static [InputRule]) -> OutputRule {
    OutputRule {
        name,
        inputs,
        keep: false,
        synthetic: Synthetic::None,
        hold: None,
        relro: false,
    }
}

const fn relro(mut rule: OutputRule) -> OutputRule {
    rule.relro = true;
    rule
}

const fn synthetic_only(name: &'static str, synthetic: Synthetic) -> OutputRule {
    synth(rule(name, &[]), synthetic)
}

const fn keep(mut rule: OutputRule) -> OutputRule {
    rule.keep = true;
    rule
}

const fn synth(mut rule: OutputRule, synthetic: Synthetic) -> OutputRule {
    rule.synthetic = synthetic;
    rule
}

const fn hold(mut rule: OutputRule, class: OrphanClass) -> OutputRule {
    rule.hold = Some(class);
    rule
}

/// GNU ld's default layout, as rules; `$dyn`, `$plt` and `$iplt` name the
/// dynamic relocation sections, which are `SHT_RELA` or `SHT_REL`.
macro_rules! default_rules {
    ($dyn:literal, $plt:literal, $iplt:literal) => {
        &[
            hold(
                synth(
                    rule(".note.gnu.build-id", &[plain(&[".note.gnu.build-id"])]),
                    Synthetic::BuildId,
                ),
                OrphanClass::Note,
            ),
            synthetic_only(".interp", Synthetic::Interp),
            synthetic_only(".hash", Synthetic::Hash),
            synthetic_only(".gnu.hash", Synthetic::GnuHash),
            synthetic_only(".dynsym", Synthetic::DynSym),
            synthetic_only(".dynstr", Synthetic::DynStr),
            synthetic_only(".gnu.version", Synthetic::VerSym),
            synthetic_only(".gnu.version_d", Synthetic::VerDef),
            synthetic_only(".gnu.version_r", Synthetic::VerNeed),
            synthetic_only($dyn, Synthetic::RelaDyn),
            synth(
                rule($plt, &[plain(&[$plt]), plain(&[$iplt])]),
                Synthetic::RelaPlt,
            ),
            synthetic_only(".relr.dyn", Synthetic::RelrDyn),
            keep(rule(".init", &[plain(&[".init"])])),
            synth(rule(".plt", &[plain(&[".plt", ".iplt"])]), Synthetic::Plt),
            synth(rule(".plt.got", &[plain(&[".plt.got"])]), Synthetic::PltGot),
            synth(rule(".plt.sec", &[plain(&[".plt.sec"])]), Synthetic::PltSec),
            hold(
                rule(
                    ".text",
                    &[
                        plain(&[".text.unlikely", ".text.*_unlikely", ".text.unlikely.*"]),
                        plain(&[".text.exit", ".text.exit.*"]),
                        plain(&[".text.startup", ".text.startup.*"]),
                        plain(&[".text.hot", ".text.hot.*"]),
                        InputRule {
                            patterns: &[".text.sorted.*"],
                            sort: SortMode::Name,
                            files: FileFilter::Any,
                        },
                        plain(&[".text", ".stub", ".text.*", ".gnu.linkonce.t.*"]),
                        plain(&[".gnu.warning"]),
                    ],
                ),
                OrphanClass::Text,
            ),
            keep(rule(".fini", &[plain(&[".fini"])])),
            hold(
                rule(
                    ".rodata",
                    &[plain(&[".rodata", ".rodata.*", ".gnu.linkonce.r.*"])],
                ),
                OrphanClass::Rodata,
            ),
            rule(".rodata1", &[plain(&[".rodata1"])]),
            synth(
                rule(
                    ".eh_frame_hdr",
                    &[
                        plain(&[".eh_frame_hdr"]),
                        plain(&[".eh_frame_entry", ".eh_frame_entry.*"]),
                    ],
                ),
                Synthetic::EhFrameHdr,
            ),
            synth(
                keep(rule(
                    ".eh_frame",
                    &[plain(&[".eh_frame"]), plain(&[".eh_frame.*"])],
                )),
                Synthetic::EhFrameEnd,
            ),
            rule(".sframe", &[plain(&[".sframe"]), plain(&[".sframe.*"])]),
            rule(
                ".gcc_except_table",
                &[plain(&[".gcc_except_table", ".gcc_except_table.*"])],
            ),
            rule(".gnu_extab", &[plain(&[".gnu_extab*"])]),
            rule(".exception_ranges", &[plain(&[".exception_ranges*"])]),
            rule(".note.build-id", &[plain(&[".note.build-id"])]),
            synth(
                rule(".note.gnu.property", &[plain(&[".note.gnu.property"])]),
                Synthetic::GnuProperty,
            ),
            rule(".note.ABI-tag", &[plain(&[".note.ABI-tag"])]),
            rule(".note.package", &[plain(&[".note.package"])]),
            rule(".note.dlopen", &[plain(&[".note.dlopen"])]),
            rule(".note.netbsd.ident", &[plain(&[".note.netbsd.ident"])]),
            rule(".note.openbsd.ident", &[plain(&[".note.openbsd.ident"])]),
            relro(hold(
                rule(
                    ".tdata",
                    &[plain(&[".tdata", ".tdata.*", ".gnu.linkonce.td.*"])],
                ),
                OrphanClass::Tdata,
            )),
            relro(hold(
                rule(
                    ".tbss",
                    &[
                        plain(&[".tbss", ".tbss.*", ".gnu.linkonce.tb.*"]),
                        plain(&[".tcommon"]),
                    ],
                ),
                OrphanClass::Tbss,
            )),
            relro(keep(rule(".preinit_array", &[plain(&[".preinit_array"])]))),
            relro(keep(rule(
                ".init_array",
                &[
                    InputRule {
                        patterns: &[".init_array.*", ".ctors.*"],
                        sort: SortMode::InitPriority,
                        files: FileFilter::Any,
                    },
                    InputRule {
                        patterns: &[".init_array", ".ctors"],
                        sort: SortMode::None,
                        files: FileFilter::NotCrtBeginEnd,
                    },
                ],
            ))),
            relro(keep(rule(
                ".fini_array",
                &[
                    InputRule {
                        patterns: &[".fini_array.*", ".dtors.*"],
                        sort: SortMode::InitPriority,
                        files: FileFilter::Any,
                    },
                    InputRule {
                        patterns: &[".fini_array", ".dtors"],
                        sort: SortMode::None,
                        files: FileFilter::NotCrtBeginEnd,
                    },
                ],
            ))),
            relro(keep(rule(
                ".ctors",
                &[
                    InputRule {
                        patterns: &[".ctors"],
                        sort: SortMode::None,
                        files: FileFilter::CrtBegin,
                    },
                    plain(&[".ctors"]),
                ],
            ))),
            relro(keep(rule(
                ".dtors",
                &[
                    InputRule {
                        patterns: &[".dtors"],
                        sort: SortMode::None,
                        files: FileFilter::CrtBegin,
                    },
                    plain(&[".dtors"]),
                ],
            ))),
            relro(keep(rule(".jcr", &[plain(&[".jcr"])]))),
            relro(synth(
                rule(
                    ".data.rel.ro",
                    &[
                        plain(&[".data.rel.ro.local*", ".gnu.linkonce.d.rel.ro.local.*"]),
                        plain(&[".data.rel.ro", ".data.rel.ro.*", ".gnu.linkonce.d.rel.ro.*"]),
                    ],
                ),
                Synthetic::DynRelro,
            )),
            relro(synth(
                rule(".dynamic", &[plain(&[".dynamic"])]),
                Synthetic::Dynamic,
            )),
            relro(synth(
                rule(".got", &[plain(&[".got"]), plain(&[".igot"])]),
                Synthetic::Got,
            )),
            synth(
                rule(".got.plt", &[plain(&[".got.plt"]), plain(&[".igot.plt"])]),
                Synthetic::GotPlt,
            ),
            hold(
                rule(
                    ".data",
                    &[plain(&[".data", ".data.*", ".gnu.linkonce.d.*"])],
                ),
                OrphanClass::Data,
            ),
            rule(".data1", &[plain(&[".data1"])]),
            hold(
                synth(
                    rule(
                        ".bss",
                        &[
                            plain(&[".dynbss"]),
                            plain(&[".bss", ".bss.*", ".gnu.linkonce.b.*"]),
                        ],
                    ),
                    Synthetic::DynBss,
                ),
                OrphanClass::Bss,
            ),
            rule(
                ".lbss",
                &[
                    plain(&[".dynlbss"]),
                    plain(&[".lbss", ".lbss.*", ".gnu.linkonce.lb.*"]),
                ],
            ),
            rule(
                ".lrodata",
                &[plain(&[".lrodata", ".lrodata.*", ".gnu.linkonce.lr.*"])],
            ),
            rule(
                ".ldata",
                &[plain(&[".ldata", ".ldata.*", ".gnu.linkonce.l.*"])],
            ),
            synth(
                rule(".comment", &[plain(&[".comment"])]),
                Synthetic::Comment,
            ),
            rule(
                ".gnu.build.attributes",
                &[plain(&[".gnu.build.attributes", ".gnu.build.attributes.*"])],
            ),
            rule(".debug", &[plain(&[".debug"])]),
            rule(".line", &[plain(&[".line"])]),
            rule(".debug_srcinfo", &[plain(&[".debug_srcinfo"])]),
            rule(".debug_sfnames", &[plain(&[".debug_sfnames"])]),
            rule(".debug_aranges", &[plain(&[".debug_aranges"])]),
            rule(".debug_pubnames", &[plain(&[".debug_pubnames"])]),
            rule(
                ".debug_info",
                &[plain(&[".debug_info", ".gnu.linkonce.wi.*"])],
            ),
            rule(".debug_abbrev", &[plain(&[".debug_abbrev"])]),
            rule(
                ".debug_line",
                &[plain(&[".debug_line", ".debug_line.*", ".debug_line_end"])],
            ),
            rule(".debug_frame", &[plain(&[".debug_frame"])]),
            rule(".debug_str", &[plain(&[".debug_str"])]),
            rule(".debug_loc", &[plain(&[".debug_loc"])]),
            rule(".debug_macinfo", &[plain(&[".debug_macinfo"])]),
            rule(".debug_weaknames", &[plain(&[".debug_weaknames"])]),
            rule(".debug_funcnames", &[plain(&[".debug_funcnames"])]),
            rule(".debug_typenames", &[plain(&[".debug_typenames"])]),
            rule(".debug_varnames", &[plain(&[".debug_varnames"])]),
            rule(".debug_pubtypes", &[plain(&[".debug_pubtypes"])]),
            rule(".debug_ranges", &[plain(&[".debug_ranges"])]),
            rule(".debug_addr", &[plain(&[".debug_addr"])]),
            rule(".debug_line_str", &[plain(&[".debug_line_str"])]),
            rule(".debug_loclists", &[plain(&[".debug_loclists"])]),
            rule(".debug_macro", &[plain(&[".debug_macro"])]),
            rule(".debug_names", &[plain(&[".debug_names"])]),
            rule(".debug_rnglists", &[plain(&[".debug_rnglists"])]),
            rule(".debug_str_offsets", &[plain(&[".debug_str_offsets"])]),
            hold(
                rule(".debug_sup", &[plain(&[".debug_sup"])]),
                OrphanClass::NonAlloc,
            ),
        ]
    };
}

/// GNU ld's default x86-64 layout, as rules.
pub static DEFAULT_RULES: &[OutputRule] = default_rules!(".rela.dyn", ".rela.plt", ".rela.iplt");

/// The same layout for architectures whose dynamic relocations are
/// `SHT_REL` (i386).
pub static REL_RULES: &[OutputRule] = default_rules!(".rel.dyn", ".rel.plt", ".rel.iplt");

/// A compiled rule set: patterns ready for matching.
///
/// With a linker script, [`RuleSet::script`] holds the layout plan and the
/// script engine ([`crate::elf::script_layout`]) replaces the default
/// rules in placement and layout.
pub struct RuleSet<'r> {
    /// The output rules.
    pub outputs: &'static [OutputRule],
    /// `(output, input description, pattern)` in match order.
    patterns: Vec<(u16, u16, Pattern)>,
    /// For each orphan class, the output rule it follows.
    holds: Vec<(OrphanClass, u16)>,
    /// The linker script plan, when scripts drive layout.
    pub script: Option<&'r crate::elf::script_layout::LayoutScript>,
    /// Where layout reports script problems (region overflows, failed
    /// assertions, orphans).
    pub diagnostics: Option<&'r dyn crate::diag::DiagnosticSink>,
}

impl std::fmt::Debug for RuleSet<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuleSet")
            .field("outputs", &self.outputs.len())
            .field("script", &self.script.is_some())
            .finish_non_exhaustive()
    }
}

/// Where the rules put one input section.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placement {
    /// Index of the output rule.
    pub output: u16,
    /// Index of the input description within it.
    pub input: u16,
}

impl<'r> RuleSet<'r> {
    /// Compiles the default rules.
    #[must_use]
    pub fn default_rules() -> Self {
        Self::new(DEFAULT_RULES)
    }

    /// The default rules of an architecture: [`REL_RULES`] where dynamic
    /// relocations are `SHT_REL`, else [`DEFAULT_RULES`].
    #[must_use]
    pub fn default_rules_for(arch: crate::elf::arch::Arch) -> Self {
        Self::new(if arch.uses_rel() {
            REL_RULES
        } else {
            DEFAULT_RULES
        })
    }

    /// The rules of a link: the script engine's plan when there is one,
    /// else the default rules.
    #[must_use]
    pub fn for_link(
        script: Option<&'r crate::elf::script_layout::LayoutScript>,
        diagnostics: &'r dyn crate::diag::DiagnosticSink,
        arch: crate::elf::arch::Arch,
    ) -> Self {
        let mut rules = Self::default_rules_for(arch);
        rules.script = script;
        rules.diagnostics = Some(diagnostics);
        rules
    }

    /// Compiles a rule list.
    #[must_use]
    pub fn new(outputs: &'static [OutputRule]) -> Self {
        let mut patterns = Vec::new();
        let mut holds = Vec::new();
        for (output_index, output) in outputs.iter().enumerate() {
            let output_index = u16::try_from(output_index).unwrap_or(u16::MAX);
            for (input_index, input) in output.inputs.iter().enumerate() {
                let input_index = u16::try_from(input_index).unwrap_or(u16::MAX);
                for pattern in input.patterns {
                    patterns.push((
                        output_index,
                        input_index,
                        Pattern::section(pattern.as_bytes()),
                    ));
                }
            }
            if let Some(class) = output.hold {
                holds.push((class, output_index));
            }
        }
        Self {
            outputs,
            patterns,
            holds,
            script: None,
            diagnostics: None,
        }
    }

    /// Finds the first description matching section `name` of a file whose
    /// base name is `file_name`.
    #[must_use]
    pub fn place(&self, name: &[u8], file_name: &[u8]) -> Option<Placement> {
        for (output, input, pattern) in &self.patterns {
            if !pattern.matches(name) {
                continue;
            }
            let Some(rule) = self
                .outputs
                .get(usize::from(*output))
                .and_then(|o| o.inputs.get(usize::from(*input)))
            else {
                continue;
            };
            let applies = match rule.files {
                FileFilter::Any => true,
                FileFilter::NotCrtBeginEnd => {
                    !(matches!(name, b".ctors" | b".dtors") && is_crt_begin_end(file_name))
                }
                FileFilter::CrtBegin => is_crt_begin(file_name),
            };
            if applies {
                return Some(Placement {
                    output: *output,
                    input: *input,
                });
            }
        }
        None
    }

    /// The output rule orphans of `class` follow.
    #[must_use]
    pub fn hold(&self, class: OrphanClass) -> u16 {
        self.holds
            .iter()
            .find(|(c, _)| *c == class)
            .map_or(u16::MAX, |(_, output)| *output)
    }
}

/// The orphan class of a section with these flags and type.
#[must_use]
pub fn orphan_class(flags: u64, sh_type: u32) -> OrphanClass {
    if flags & SHF_ALLOC == 0 {
        OrphanClass::NonAlloc
    } else if sh_type == SHT_NOTE {
        OrphanClass::Note
    } else if flags & SHF_TLS != 0 {
        if sh_type == SHT_NOBITS {
            OrphanClass::Tbss
        } else {
            OrphanClass::Tdata
        }
    } else if flags & SHF_EXECINSTR != 0 {
        OrphanClass::Text
    } else if flags & SHF_WRITE == 0 {
        OrphanClass::Rodata
    } else if sh_type == SHT_NOBITS {
        OrphanClass::Bss
    } else {
        OrphanClass::Data
    }
}

/// The init priority used by `SORT_BY_INIT_PRIORITY` for a section name.
/// Sections without a numeric suffix sort as 65536, after numbered ones.
#[must_use]
pub fn priority(name: &[u8]) -> u32 {
    init_priority(name).unwrap_or(65536)
}

fn base_name(path: &[u8]) -> &[u8] {
    match path.iter().rposition(|&b| b == b'/') {
        Some(at) => path.get(at.saturating_add(1)..).unwrap_or(path),
        None => path,
    }
}

/// `crtbegin.o` or `crtbegin?.o`.
#[must_use]
pub fn is_crt_begin(file: &[u8]) -> bool {
    let name = base_name(file);
    name == b"crtbegin.o"
        || (name.len() == 11 && name.starts_with(b"crtbegin") && name.ends_with(b".o"))
}

/// `crtbegin*.o` or `crtend*.o`, as `EXCLUDE_FILE (*crtbegin.o *crtbegin?.o
/// *crtend.o *crtend?.o)` matches.
#[must_use]
pub fn is_crt_begin_end(file: &[u8]) -> bool {
    let name = base_name(file);
    is_crt_begin(file)
        || name == b"crtend.o"
        || (name.len() == 9 && name.starts_with(b"crtend") && name.ends_with(b".o"))
}

/// Whether `name` is a valid C identifier, so that `__start_name` and
/// `__stop_name` are defined for it.
#[must_use]
pub fn is_c_identifier(name: &[u8]) -> bool {
    match name.split_first() {
        Some((first, rest)) => {
            (first.is_ascii_alphabetic() || *first == b'_')
                && rest.iter().all(|b| b.is_ascii_alphanumeric() || *b == b'_')
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output_of(rules: &RuleSet, name: &str, file: &str) -> Option<&'static str> {
        rules
            .place(name.as_bytes(), file.as_bytes())
            .and_then(|p| rules.outputs.get(usize::from(p.output)))
            .map(|o| o.name)
    }

    #[test]
    fn places_like_gnu_ld() {
        let rules = RuleSet::default_rules();
        assert_eq!(output_of(&rules, ".text", "a.o"), Some(".text"));
        assert_eq!(output_of(&rules, ".text.hot.foo", "a.o"), Some(".text"));
        assert_eq!(output_of(&rules, ".rodata.str1.1", "a.o"), Some(".rodata"));
        assert_eq!(
            output_of(&rules, ".init_array.00100", "a.o"),
            Some(".init_array")
        );
        assert_eq!(output_of(&rules, ".ctors", "x/crtbegin.o"), Some(".ctors"));
        assert_eq!(output_of(&rules, ".ctors", "main.o"), Some(".init_array"));
        assert_eq!(output_of(&rules, ".ctors", "crtend.o"), Some(".ctors"));
        assert_eq!(output_of(&rules, ".tbss.x", "a.o"), Some(".tbss"));
        assert_eq!(
            output_of(&rules, ".data.rel.ro.local", "a.o"),
            Some(".data.rel.ro")
        );
        assert_eq!(output_of(&rules, "rodata.cst32", "a.o"), None);
        assert_eq!(output_of(&rules, "qld_items", "a.o"), None);
        let unlikely = rules.place(b".text.unlikely.x", b"a.o").unwrap();
        let normal = rules.place(b".text.x", b"a.o").unwrap();
        assert!(unlikely.input < normal.input);
    }

    #[test]
    fn classes_and_names() {
        assert_eq!(orphan_class(SHF_ALLOC, 1), OrphanClass::Rodata);
        assert_eq!(orphan_class(SHF_ALLOC | SHF_WRITE, 1), OrphanClass::Data);
        assert_eq!(
            orphan_class(SHF_ALLOC | SHF_WRITE, SHT_NOBITS),
            OrphanClass::Bss
        );
        assert_eq!(orphan_class(0, 1), OrphanClass::NonAlloc);
        assert!(is_c_identifier(b"qld_items"));
        assert!(!is_c_identifier(b".text"));
        assert!(is_crt_begin(b"/usr/lib/gcc/crtbeginT.o"));
        assert!(is_crt_begin_end(b"crtendS.o"));
        assert!(!is_crt_begin_end(b"main.o"));
        assert_eq!(priority(b".init_array.00150"), 150);
        assert_eq!(priority(b".init_array"), 65536);
    }
}
