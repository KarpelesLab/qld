//! Symbols the linker defines itself.
//!
//! MinGW's C runtime relies on the symbols GNU `ld`'s default `i386pep`
//! script assigns: the boundaries of the constructor and destructor lists,
//! of the `.CRT$X*` initializer groups, of the pseudo-relocation list, of the
//! import address table and of `.data`, `.bss` and `.tls`. They are computed
//! from the [`Marker`]s the layout recorded.
//!
//! Two flavors, following the script:
//!
//! - *assignments* (`__CTOR_LIST__ = .`) always win, even over a common
//!   symbol from `libgcc`'s `ctors.o`;
//! - *provided* symbols (`PROVIDE (etext = .)`) are defined only when the
//!   link refers to them and nothing else defines them.

#![deny(clippy::arithmetic_side_effects)]

use hashbrown::HashMap;

use crate::ids::SymbolId;
use crate::symbols::{DefinitionKind, SymbolFlags, SymbolName, SymbolTable};

use super::layout::{Layout, Marker, align_up32};
use super::reloc::Value;

/// A linker-defined symbol and where its value comes from.
struct Rule {
    /// The names it is known by (MinGW defines both the `_`-prefixed and the
    /// bare form for several of them).
    names: &'static [&'static [u8]],
    /// Where the value comes from.
    source: Source,
    /// Whether the definition overrides any other (an assignment) or applies
    /// only to an otherwise undefined symbol (`PROVIDE`).
    provide: bool,
}

/// Where a linker-defined symbol's value comes from.
enum Source {
    /// The position a layout marker recorded.
    At(Marker),
    /// The start of an output section.
    SectionStart(&'static [u8]),
    /// The end of an output section.
    SectionEnd(&'static [u8]),
    /// RVA 0: the image base itself.
    ImageBase,
    /// The end of the last loaded section before `.rsrc`/`.reloc`.
    ImageEnd,
}

const RULES: &[Rule] = &[
    Rule {
        names: &[b"__image_base__", b"__ImageBase", b"___ImageBase"],
        source: Source::ImageBase,
        provide: false,
    },
    Rule {
        names: &[b"etext", b"_etext"],
        source: Source::At(Marker::Etext),
        provide: true,
    },
    Rule {
        names: &[b"__data_start__"],
        source: Source::At(Marker::DataStart),
        provide: false,
    },
    Rule {
        names: &[b"__data_end__"],
        source: Source::At(Marker::DataEnd),
        provide: false,
    },
    Rule {
        names: &[b"__bss_start__"],
        source: Source::SectionStart(b".bss"),
        provide: false,
    },
    Rule {
        names: &[b"__bss_end__"],
        source: Source::SectionEnd(b".bss"),
        provide: false,
    },
    Rule {
        names: &[b"__CTOR_LIST__", b"___CTOR_LIST__"],
        source: Source::At(Marker::CtorHead),
        provide: false,
    },
    Rule {
        names: &[b"__DTOR_LIST__", b"___DTOR_LIST__"],
        source: Source::At(Marker::DtorHead),
        provide: false,
    },
    Rule {
        names: &[b"___crt_xc_start__"],
        source: Source::At(Marker::CrtXcStart),
        provide: false,
    },
    Rule {
        names: &[b"___crt_xc_end__"],
        source: Source::At(Marker::CrtXcEnd),
        provide: false,
    },
    Rule {
        names: &[b"___crt_xi_start__"],
        source: Source::At(Marker::CrtXiStart),
        provide: false,
    },
    Rule {
        names: &[b"___crt_xi_end__"],
        source: Source::At(Marker::CrtXiEnd),
        provide: false,
    },
    Rule {
        names: &[b"___crt_xl_start__"],
        source: Source::At(Marker::CrtXlStart),
        provide: false,
    },
    Rule {
        names: &[b"___crt_xp_start__"],
        source: Source::At(Marker::CrtXpStart),
        provide: false,
    },
    Rule {
        names: &[b"___crt_xp_end__"],
        source: Source::At(Marker::CrtXpEnd),
        provide: false,
    },
    Rule {
        names: &[b"___crt_xt_start__"],
        source: Source::At(Marker::CrtXtStart),
        provide: false,
    },
    Rule {
        names: &[b"___crt_xt_end__"],
        source: Source::At(Marker::CrtXtEnd),
        provide: false,
    },
    Rule {
        names: &[b"___crt_xd_start__"],
        source: Source::At(Marker::CrtXdStart),
        provide: false,
    },
    Rule {
        names: &[b"___crt_xd_end__"],
        source: Source::At(Marker::CrtXdEnd),
        provide: false,
    },
    Rule {
        names: &[
            b"__RUNTIME_PSEUDO_RELOC_LIST__",
            b"___RUNTIME_PSEUDO_RELOC_LIST__",
        ],
        source: Source::At(Marker::PseudoStart),
        provide: false,
    },
    Rule {
        names: &[
            b"__RUNTIME_PSEUDO_RELOC_LIST_END__",
            b"___RUNTIME_PSEUDO_RELOC_LIST_END__",
        ],
        source: Source::At(Marker::PseudoEnd),
        provide: false,
    },
    Rule {
        names: &[b"__IAT_start__"],
        source: Source::At(Marker::IatStart),
        provide: false,
    },
    Rule {
        names: &[b"__IAT_end__"],
        source: Source::At(Marker::IatEnd),
        provide: false,
    },
    Rule {
        names: &[b"___tls_start__"],
        source: Source::At(Marker::TlsStart),
        provide: false,
    },
    Rule {
        names: &[b"___tls_end__"],
        source: Source::At(Marker::TlsEnd),
        provide: false,
    },
    Rule {
        names: &[b"end", b"_end", b"__end__"],
        source: Source::ImageEnd,
        provide: true,
    },
];

/// Every name the linker may define, so they can be interned before
/// resolution and take part in it.
pub fn names() -> impl Iterator<Item = &'static [u8]> {
    RULES.iter().flat_map(|rule| rule.names.iter().copied())
}

/// Computes the value of every linker-defined symbol that the link needs.
///
/// A symbol is defined when it is an assignment (it always wins) or when it
/// is `PROVIDE`d, referenced and otherwise undefined.
#[must_use]
pub fn values(
    layout: &Layout,
    symbols: &SymbolTable<'_>,
    section_alignment: u32,
) -> HashMap<SymbolId, Value> {
    let mut defined = HashMap::default();
    for rule in RULES {
        let Some(rva) = value_of(layout, &rule.source, section_alignment) else {
            continue;
        };
        for &name in rule.names {
            let Some(id) = symbols.lookup(&SymbolName::new(name)) else {
                continue;
            };
            if rule.provide {
                let kind = symbols.definition_kind(id);
                let referenced = symbols
                    .flags(id)
                    .intersects(SymbolFlags::REFERENCED | SymbolFlags::WEAK_REFERENCED);
                if !referenced || matches!(kind, DefinitionKind::Regular | DefinitionKind::Common) {
                    continue;
                }
            }
            defined.insert(id, rva);
        }
    }
    defined
}

/// The RVA a rule's source names.
fn value_of(layout: &Layout, source: &Source, section_alignment: u32) -> Option<Value> {
    let address = |rva: u32, section: u32| Some(Value::Address { rva, section });
    match source {
        Source::ImageBase => address(0, u32::MAX),
        Source::At(marker) => {
            let &(_, section, offset) = layout
                .markers
                .iter()
                .find(|&&(kind, _, _)| kind == *marker)?;
            let base = layout.sections.get(section as usize)?.rva;
            address(base.wrapping_add(offset), section)
        }
        Source::SectionStart(name) => {
            let index = layout.index_of(name)?;
            address(layout.sections.get(index as usize)?.rva, index)
        }
        Source::SectionEnd(name) => {
            let index = layout.index_of(name)?;
            let section = layout.sections.get(index as usize)?;
            address(section.rva.wrapping_add(section.virtual_size), index)
        }
        Source::ImageEnd => {
            let last = layout.sections.iter().enumerate().rfind(
                |(_, section): &(usize, &super::layout::OutSection)| {
                    !matches!(section.name.as_slice(), b".rsrc" | b".reloc")
                        && !section.name.starts_with(b".debug")
                },
            )?;
            let (index, section) = last;
            address(
                align_up32(
                    section.rva.wrapping_add(section.virtual_size),
                    section_alignment,
                ),
                u32::try_from(index).unwrap_or(u32::MAX),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_rule_name_is_unique() {
        let mut all: Vec<&[u8]> = names().collect();
        let total = all.len();
        all.sort_unstable();
        all.dedup();
        assert_eq!(all.len(), total);
    }

    #[test]
    fn mingw_crt_symbols_are_covered() {
        let all: Vec<&[u8]> = names().collect();
        for expected in [
            &b"__CTOR_LIST__"[..],
            b"___CTOR_LIST__",
            b"__DTOR_LIST__",
            b"___crt_xi_start__",
            b"___crt_xl_start__",
            b"__RUNTIME_PSEUDO_RELOC_LIST__",
            b"__RUNTIME_PSEUDO_RELOC_LIST_END__",
            b"__IAT_start__",
            b"___tls_start__",
            b"__ImageBase",
            b"_end",
        ] {
            assert!(
                all.contains(&expected),
                "{}",
                String::from_utf8_lossy(expected)
            );
        }
    }
}
