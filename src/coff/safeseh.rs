//! SafeSEH: the i386 table of registered exception handlers.
//!
//! On i386, structured exception handling walks a chain of handler records
//! on the stack, which an attacker who overwrites the stack controls. An
//! image *registers* its handlers in a sorted table of RVAs that the load
//! configuration points at (`SEHandlerTable`, `SEHandlerCount`); Windows
//! then calls only a registered handler of the image.
//!
//! Each object lists its handlers in `.sxdata`, as symbol indices, and
//! declares that it registers all of them with bit 0 of `@feat.00`. The
//! C runtime's load configuration (`__load_config_used`) refers to the
//! table through `___safe_se_handler_table` and
//! `___safe_se_handler_count`, which the linker defines, as `link.exe`
//! and lld do. GNU `ld` has no SafeSEH support; the MinGW runtime defines
//! no load configuration, so nothing changes for a GCC-built program.
//!
//! | [`PeOptions::safe_seh`] | Every object compatible | Otherwise |
//! | --- | --- | --- |
//! | `None` (default) | the table | both symbols 0: no SafeSEH |
//! | `Some(true)` (`/SAFESEH`) | the table | an error naming the object |
//! | `Some(false)` (`/SAFESEH:NO`) | both symbols 0 | both symbols 0 |
//!
//! Nothing is defined when nothing refers to the symbols. The table is
//! placed at the end of `.rdata`. `--no-seh` is independent: it sets
//! `IMAGE_DLLCHARACTERISTICS_NO_SEH`, which says the image has no handlers
//! at all.

#![deny(clippy::arithmetic_side_effects)]

use hashbrown::HashMap;

use crate::error::{Error, Result};
use crate::ids::{FileId, SymbolId};
use crate::symbols::{Resolution, SymbolFlags, SymbolName, SymbolTable};

use super::inputs::CoffInput;
use super::options::PeOptions;
use super::reloc::{Addresses, Value};

/// The symbol naming the table (`__safe_se_handler_table` in C).
pub const TABLE_SYMBOL: &[u8] = b"___safe_se_handler_table";
/// The absolute symbol holding the number of entries.
pub const COUNT_SYMBOL: &[u8] = b"___safe_se_handler_count";

/// What the link does about SafeSEH.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Plan {
    /// Nothing refers to the table: define nothing.
    #[default]
    Nothing,
    /// Define both symbols as 0: the image makes no SafeSEH promise.
    Zero,
    /// Build the table from these `.sxdata` entries, as `(file, record)`.
    Table(Vec<(u32, u32)>),
}

impl Plan {
    /// Bytes to reserve for the table: an upper bound, since two records
    /// may name the same handler.
    #[must_use]
    pub fn reserved_size(&self) -> u32 {
        match self {
            Self::Table(handlers) => u32::try_from(handlers.len())
                .unwrap_or(u32::MAX)
                .saturating_mul(4),
            Self::Nothing | Self::Zero => 0,
        }
    }
}

/// Decides what the link does about SafeSEH.
///
/// # Errors
///
/// Returns [`Error::Option`] when `safe_seh` is `Some(true)` and an object
/// is not SafeSEH-compatible.
pub fn plan(
    pe: &PeOptions,
    symbols: &SymbolTable<'_>,
    files: &[CoffInput<'_>],
    resolution: &Resolution<'_>,
) -> Result<Plan> {
    if !pe.target().is_pe32() {
        return Ok(Plan::Nothing);
    }
    let referenced = [TABLE_SYMBOL, COUNT_SYMBOL].iter().any(|name| {
        symbols.lookup(&SymbolName::new(name)).is_some_and(|id| {
            symbols
                .flags(id)
                .intersects(SymbolFlags::REFERENCED | SymbolFlags::WEAK_REFERENCED)
        })
    });
    if !referenced && pe.safe_seh != Some(true) {
        return Ok(Plan::Nothing);
    }
    if pe.safe_seh == Some(false) {
        return Ok(Plan::Zero);
    }
    let mut handlers = Vec::new();
    for (index, file) in files.iter().enumerate() {
        let Some(parsed) = file.object() else {
            continue;
        };
        if !resolution.is_live(FileId::new(index)) {
            continue;
        }
        if !parsed.safe_seh_compatible() {
            if pe.safe_seh == Some(true) {
                return Err(Error::Option(format!(
                    "SafeSEH: {} is not compatible with SafeSEH (no `@feat.00` bit 0)",
                    file.display()
                )));
            }
            return Ok(if referenced {
                Plan::Zero
            } else {
                Plan::Nothing
            });
        }
        let file = u32::try_from(index).unwrap_or(u32::MAX);
        handlers.extend(parsed.sxdata.iter().map(|&record| (file, record)));
    }
    handlers.sort_unstable();
    handlers.dedup();
    Ok(Plan::Table(handlers))
}

/// The handler RVAs of a [`Plan::Table`], sorted and without duplicates,
/// as the loader's binary search needs them.
#[must_use]
pub fn handler_rvas(addresses: &Addresses<'_, '_>, handlers: &[(u32, u32)]) -> Vec<u32> {
    let mut rvas: Vec<u32> = handlers
        .iter()
        .filter_map(|&(file, record)| addresses.record_value(file as usize, record))
        .filter_map(Value::rva)
        .collect();
    rvas.sort_unstable();
    rvas.dedup();
    rvas
}

/// The values of the two symbols, for the linker-defined symbol map:
/// `table` is where the table was placed (`None` for [`Plan::Zero`]).
#[must_use]
pub fn symbol_values(
    symbols: &SymbolTable<'_>,
    table: Option<Value>,
    count: usize,
) -> HashMap<SymbolId, Value> {
    let mut values = HashMap::default();
    let count = u64::try_from(count).unwrap_or(0);
    if let Some(id) = symbols.lookup(&SymbolName::new(TABLE_SYMBOL)) {
        values.insert(id, table.unwrap_or(Value::Absolute(0)));
    }
    if let Some(id) = symbols.lookup(&SymbolName::new(COUNT_SYMBOL)) {
        values.insert(id, Value::Absolute(count));
    }
    values
}

/// The table's bytes: one little-endian RVA per handler.
#[must_use]
pub fn encode(rvas: &[u32]) -> Vec<u8> {
    rvas.iter().flat_map(|rva| rva.to_le_bytes()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_size_counts_entries() {
        assert_eq!(Plan::Table(vec![(1, 2), (1, 3)]).reserved_size(), 8);
        assert_eq!(Plan::Zero.reserved_size(), 0);
        assert_eq!(encode(&[0x1000, 0x1010]), [0, 0x10, 0, 0, 0x10, 0x10, 0, 0]);
    }
}
