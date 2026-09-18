//! Link maps (`-Map`, `-M`).
//!
//! The format is lld's: one line per output section, per input section, and
//! per global symbol defined in it, with address, size and alignment
//! columns:
//!
//! ```text
//!              VMA     Size Align Out     In      Symbol
//!           401000       a0    16 .text
//!           401000       20     1         start.o:(.text)
//!           401000        0     0                 _start
//! ```

#![deny(clippy::arithmetic_side_effects)]

use std::fmt::Write as _;

use crate::args::LinkOptions;
use crate::error::{Error, Result};
use crate::ids::SectionId;

use super::layout::{Member, Trailer};
use super::refs::Def;
use super::symtab::SymtabPlan;
use super::values::Addresses;

/// Renders the link map.
#[must_use]
pub fn render(addresses: &Addresses<'_, '_>, plan: &SymtabPlan) -> String {
    let refs = &addresses.refs;
    let layout = addresses.layout;

    // Global symbols by defining section.
    let mut symbols: Vec<(SectionId, u64, &[u8])> = plan
        .globals
        .iter()
        .chain(&plan.hidden)
        .filter_map(|&id| {
            let target = refs.global_target(id, true);
            let Def::Section { file, section, .. } = target.def else {
                return None;
            };
            let section = refs.sections.id(file, section)?;
            let address = addresses.globals.get(id.index()).copied()?;
            Some((section, address, refs.symbols.name(id).bytes()))
        })
        .collect();
    symbols.sort_unstable();

    let mut text = String::new();
    let _ = writeln!(
        text,
        "{:>16} {:>8} {:>5} Out     In      Symbol",
        "VMA", "Size", "Align"
    );
    for section in &layout.sections {
        if section.trailer != Trailer::None && section.trailer != Trailer::Shstrtab {
            continue;
        }
        let _ = writeln!(
            text,
            "{:>16x} {:>8x} {:>5} {}",
            section.addr,
            section.size,
            section.align,
            String::from_utf8_lossy(section.name)
        );
        for placed in &section.members {
            let address = section.addr.saturating_add(placed.offset);
            let what = match placed.member {
                Member::Input(id) => describe(addresses, id),
                Member::Merge(_) => "<merged sections>".to_string(),
                Member::Synthetic(kind) => format!("<internal>:({kind:?})"),
            };
            let _ = writeln!(
                text,
                "{:>16x} {:>8x} {:>5}         {what}",
                address, placed.size, 1
            );
            if let Member::Input(id) = placed.member {
                let start = symbols.partition_point(|(s, ..)| *s < id);
                let end = symbols.partition_point(|(s, ..)| *s <= id);
                for &(_, value, name) in symbols.get(start..end).unwrap_or_default() {
                    let _ = writeln!(
                        text,
                        "{:>16x} {:>8x} {:>5}                 {}",
                        value,
                        0,
                        0,
                        String::from_utf8_lossy(name)
                    );
                }
            }
        }
    }
    text
}

fn describe(addresses: &Addresses<'_, '_>, id: SectionId) -> String {
    let refs = &addresses.refs;
    let Some((file, index)) = refs.sections.locate(id) else {
        return String::new();
    };
    let Some(input) = refs.files.get(file) else {
        return String::new();
    };
    let name = input
        .object
        .as_ref()
        .and_then(|o| o.section(index))
        .map_or_else(String::new, |s| {
            String::from_utf8_lossy(s.name).into_owned()
        });
    format!("{}:({name})", input.display())
}

/// Writes the map requested by `-Map` or `-M`, followed by the `--cref`
/// table when there is one (which goes to [`LinkOptions::map_output`]
/// without a map).
///
/// # Errors
///
/// Returns [`Error::Io`] if the map file cannot be written.
pub fn write(
    options: &LinkOptions,
    addresses: &Addresses<'_, '_>,
    plan: &SymtabPlan,
    cref: Option<&str>,
) -> Result<()> {
    if options.map_file.is_none() && !options.print_map {
        return write_cref(options, cref);
    }
    let mut text = render(addresses, plan);
    if let Some(cref) = cref {
        text.push_str(cref);
    }
    if let Some(path) = &options.map_file {
        std::fs::write(path, &text).map_err(|e| Error::io(path, e))?;
    }
    if options.print_map {
        options.print_text(&text);
    }
    Ok(())
}

/// Writes the `--cref` table alone: into the `-Map` file if one was named,
/// otherwise to [`LinkOptions::map_output`]. Used when no map is rendered
/// (relocatable output, or no `-Map`/`-M`).
///
/// # Errors
///
/// Returns [`Error::Io`] if the map file cannot be written.
pub fn write_cref(options: &LinkOptions, cref: Option<&str>) -> Result<()> {
    let Some(cref) = cref else {
        return Ok(());
    };
    match &options.map_file {
        Some(path) => std::fs::write(path, cref).map_err(|e| Error::io(path, e)),
        None => {
            options.print_text(cref);
            Ok(())
        }
    }
}
