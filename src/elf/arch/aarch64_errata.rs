//! Cortex-A53 errata 843419 and 835769: which instructions of the output
//! need a patch.
//!
//! `--fix-cortex-a53-843419` and `--fix-cortex-a53-835769` scan the code of
//! every live input section, as lld (843419) and GNU ld (835769) do, and
//! return the instructions to move into a patch: the last load or store of
//! an 843419 sequence, and the multiply-accumulate of an 835769 one. The
//! instruction tests are in [`crate::arch::aarch64`].
//!
//! Only code is scanned: the `$x` and `$d` mapping symbols of each section
//! delimit its code and data, as in both linkers, and a section without
//! mapping symbols is not scanned. The scan reads the input bytes, before
//! relocation, as lld does.
//!
//! 843419 depends on addresses (the `adrp` must be at a page offset of
//! `0xff8` or `0xffc`), so it runs in every round of the layout fixpoint
//! ([`super::thunk::plan`]); patches go into the pool at the end of the
//! output section, after the range-extension thunks.

#![deny(clippy::arithmetic_side_effects)]

use rayon::prelude::*;

use crate::arch::aarch64;
use crate::args::LinkOptions;
use crate::elf::layout::Layout;
use crate::elf::object::{ObjectInput, SectionKind};
use crate::elf::read::consts::{SHF_ALLOC, SHF_EXECINSTR, SHT_PROGBITS, STT_NOTYPE};
use crate::elf::refs::Refs;
use crate::ids::SectionId;

/// One instruction to patch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Site {
    /// The output section (its index in `Placement::outputs`) holding it.
    pub output: u32,
    /// Its address.
    pub address: u64,
    /// The input section holding it.
    pub section: SectionId,
    /// Its offset in that input section.
    pub offset: u64,
}

/// Whether either erratum workaround is on.
#[must_use]
pub fn enabled(options: &LinkOptions) -> bool {
    options.fix_cortex_a53_843419 || options.aarch64.fix_cortex_a53_835769
}

/// Whether local symbol `name` is a mapping symbol, and if so whether it
/// starts code (`$x`) rather than data (`$d`).
fn mapping_symbol(name: &[u8]) -> Option<bool> {
    let code = match name.get(..2)? {
        b"$x" => true,
        b"$d" => false,
        _ => return None,
    };
    matches!(name.get(2), None | Some(b'.')).then_some(code)
}

/// The mapping symbols of `object`, as `(section, value, code)`, sorted.
fn mapping_symbols(object: &ObjectInput<'_>) -> Vec<(u32, u64, bool)> {
    let symbols = object.elf.symbols();
    let mut out: Vec<(u32, u64, bool)> = symbols
        .iter_raw()
        .enumerate()
        .take(object.first_global)
        .filter(|(_, raw)| raw.kind() == STT_NOTYPE)
        .filter_map(|(index, raw)| {
            let code = mapping_symbol(symbols.name(index, &raw).ok()?)?;
            let section = symbols.section_of(index, &raw)?.section()?;
            Some((section, raw.st_value, code))
        })
        .collect();
    out.sort_by_key(|&(section, value, _)| (section, value));
    out
}

/// The code ranges of a section of `size` bytes whose mapping symbols are
/// `symbols` (sorted by value): lld's reading, where a run of symbols of one
/// kind counts once and leading data symbols are dropped.
fn code_ranges(symbols: &[(u32, u64, bool)], size: u64) -> Vec<(u64, u64)> {
    let mut runs: Vec<(u64, bool)> = Vec::new();
    for &(_, value, code) in symbols {
        if runs.last().is_none_or(|&(_, last)| last != code) {
            runs.push((value, code));
        }
    }
    if runs.first().is_some_and(|&(_, code)| !code) {
        runs.remove(0);
    }
    runs.iter()
        .enumerate()
        .filter(|(_, (_, code))| *code)
        .map(|(index, &(start, _))| {
            let end = runs
                .get(index.saturating_add(1))
                .map_or(size, |&(value, _)| value);
            (start, end.min(size))
        })
        .collect()
}

/// Every instruction the enabled workarounds patch, sorted by output
/// section and address.
#[must_use]
pub fn scan(refs: &Refs<'_, '_>, layout: &Layout<'_>, options: &LinkOptions) -> Vec<Site> {
    let fix_843419 = options.fix_cortex_a53_843419;
    let fix_835769 = options.aarch64.fix_cortex_a53_835769;
    if !fix_843419 && !fix_835769 {
        return Vec::new();
    }
    let mut sites: Vec<Site> = refs
        .files
        .par_iter()
        .enumerate()
        .flat_map_iter(|(file_index, file)| {
            let mut sites = Vec::new();
            let Some(object) = &file.object else {
                return sites;
            };
            let mut symbols: Option<Vec<(u32, u64, bool)>> = None;
            for (section_index, section) in object.sections.iter().enumerate() {
                let Ok(section_index) = u32::try_from(section_index) else {
                    break;
                };
                let header = &section.header;
                if header.sh_type != SHT_PROGBITS
                    || header.sh_flags & (SHF_ALLOC | SHF_EXECINSTR) != (SHF_ALLOC | SHF_EXECINSTR)
                    || section.kind == SectionKind::Ignored
                    || !refs.sections.is_live_in(file_index, section_index)
                {
                    continue;
                }
                let Some(id) = refs.sections.id(file_index, section_index) else {
                    continue;
                };
                let shndx = layout.section_shndx.get(id.index()).copied().unwrap_or(0);
                let Some(output) = layout.output_of_shndx(shndx) else {
                    continue;
                };
                let Some(&base) = layout.section_addr.get(id.index()) else {
                    continue;
                };
                let Ok(code) = object.section_data(section) else {
                    continue;
                };
                let symbols = symbols.get_or_insert_with(|| mapping_symbols(object));
                let from = symbols.partition_point(|&(s, ..)| s < section_index);
                let to = symbols.partition_point(|&(s, ..)| s <= section_index);
                let size = u64::try_from(code.len()).unwrap_or(0);
                for (start, end) in code_ranges(symbols.get(from..to).unwrap_or_default(), size) {
                    let mut found = Vec::new();
                    if fix_843419 {
                        found.extend(aarch64::scan_843419(code, base, start, end));
                    }
                    if fix_835769 {
                        found.extend(aarch64::scan_835769(code, start, end));
                    }
                    sites.extend(found.into_iter().map(|offset| Site {
                        output,
                        address: base.wrapping_add(offset),
                        section: id,
                        offset,
                    }));
                }
            }
            sites
        })
        .collect();
    sites.sort_unstable();
    sites.dedup();
    sites
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mapping_symbols_are_recognized() {
        assert_eq!(mapping_symbol(b"$x"), Some(true));
        assert_eq!(mapping_symbol(b"$d.12"), Some(false));
        assert_eq!(mapping_symbol(b"$xyz"), None);
        assert_eq!(mapping_symbol(b"$t"), None);
        assert_eq!(mapping_symbol(b"x"), None);
    }

    #[test]
    fn code_ranges_follow_mapping_symbols() {
        let symbols = [
            (1, 0, false),
            (1, 8, true),
            (1, 16, true),
            (1, 32, false),
            (1, 40, true),
        ];
        assert_eq!(code_ranges(&symbols, 64), [(8, 32), (40, 64)]);
        assert!(code_ranges(&[], 64).is_empty());
    }
}
