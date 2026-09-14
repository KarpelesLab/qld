//! Resolution problems as data, for diagnostics to render later.
//!
//! Everything here is sorted deterministically by the resolution driver, so a
//! renderer can emit it in order without sorting again.

use super::definition::Definition;
use super::name::{InputPosition, SymbolName};
use crate::ids::{FileId, SymbolId};

/// One place in a live file that refers to a symbol.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct SymbolReference {
    /// The referring file's input position. Listed first so that the derived
    /// order is input order.
    pub position: InputPosition,
    /// The referring file.
    pub file: FileId,
    /// Index of the reference in the file's global symbol list.
    pub index: u32,
}

/// A symbol that live files reference (non-weakly) but nothing defines.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct UndefinedSymbol<'a> {
    /// The symbol.
    pub symbol: SymbolId,
    /// Its name.
    pub name: SymbolName<'a>,
    /// Every non-weak reference from a live file, in input order. Never
    /// empty.
    pub references: Vec<SymbolReference>,
}

/// A symbol with more than one definition that the format treats as an
/// error (for ELF, two strong definitions).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DuplicateSymbol<'a> {
    /// The symbol.
    pub symbol: SymbolId,
    /// Its name.
    pub name: SymbolName<'a>,
    /// The definition that was kept.
    pub winner: Definition,
    /// The conflicting definitions that lost, in input order. Never empty.
    pub others: Vec<Definition>,
}
