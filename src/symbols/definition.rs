//! Symbol definitions and the precedence rule that picks between them.

use core::cmp::Ordering;

use super::name::InputPosition;
use crate::ids::FileId;

/// The format-neutral category of a symbol definition.
///
/// Every format has some version of these, though they rank them differently
/// (which is what [`Resolver`] decides). The variant order here means nothing
/// for precedence.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
#[repr(u8)]
pub enum DefinitionKind {
    /// No definition. This is the state of every symbol before resolution,
    /// and never a candidate.
    #[default]
    Undefined = 0,
    /// A definition inside an archive member that has not been extracted.
    Lazy = 1,
    /// A definition exported by a shared library (ELF DSO, Mach-O dylib,
    /// PE import library).
    Shared = 2,
    /// A weak definition (ELF `STB_WEAK`, Mach-O weak definition).
    Weak = 3,
    /// A tentative definition (ELF `SHN_COMMON`, Mach-O `N_UNDF` with a
    /// value, COFF common). [`Definition::aux`] usually holds its size.
    Common = 4,
    /// An ordinary strong definition.
    Regular = 5,
}

impl DefinitionKind {
    #[inline]
    pub(crate) const fn from_u8(raw: u8) -> Self {
        match raw {
            1 => Self::Lazy,
            2 => Self::Shared,
            3 => Self::Weak,
            4 => Self::Common,
            5 => Self::Regular,
            _ => Self::Undefined,
        }
    }
}

/// One definition of a symbol, as stored in the symbol table.
///
/// The table holds, per symbol, the definition that currently takes
/// precedence. Everything else about the symbol (its section, value, type,
/// visibility) stays in the owning file's own tables, found through
/// [`file`](Self::file) and [`index`](Self::index).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Definition {
    /// What kind of definition this is.
    pub kind: DefinitionKind,
    /// The file that defines the symbol. For [`DefinitionKind::Lazy`], the
    /// archive member that would be extracted.
    pub file: FileId,
    /// The index of the symbol within the defining file's global symbol list
    /// (for a lazy definition, within its lazy name list). The backend uses
    /// it to find the section and value.
    pub index: u32,
    /// The defining file's input position, used to break ties.
    pub position: InputPosition,
    /// Format-specific data the precedence rule may use, such as the size of
    /// an ELF common symbol. Zero when unused.
    pub aux: u64,
}

impl Default for Definition {
    fn default() -> Self {
        Self::undefined()
    }
}

impl Definition {
    /// Returns the empty definition every symbol starts with.
    #[inline]
    #[must_use]
    pub fn undefined() -> Self {
        Self {
            kind: DefinitionKind::Undefined,
            file: FileId::from_u32(u32::MAX),
            index: u32::MAX,
            position: InputPosition::from_raw(u64::MAX),
            aux: 0,
        }
    }

    /// Returns `true` unless this is [`DefinitionKind::Undefined`].
    #[inline]
    #[must_use]
    pub fn is_defined(&self) -> bool {
        self.kind != DefinitionKind::Undefined
    }

    /// The deterministic tie-break key: input position, then file, then
    /// symbol index. Lower wins.
    #[inline]
    #[must_use]
    pub fn tie_key(&self) -> (InputPosition, FileId, u32) {
        (self.position, self.file, self.index)
    }

    /// Returns `true` if the two definitions come from the same symbol of the
    /// same file.
    #[inline]
    #[must_use]
    pub fn same_origin(&self, other: &Self) -> bool {
        self.file == other.file && self.index == other.index
    }
}

/// A format's symbol precedence rules.
///
/// The symbol table keeps, per symbol, the best definition seen so far, and
/// the backend supplies what "best" means. The combined order used by the
/// table is: [`compare`](Self::compare) first, then the lower
/// [`Definition::tie_key`]. For results to be independent of the order in
/// which threads insert definitions, `compare` must be a *total preorder*:
/// consistent (`compare(a, b)` is the reverse of `compare(b, a)`) and
/// transitive. The table then always ends up with the maximum candidate,
/// whatever the insertion order.
///
/// A reference implementation of ELF's rules lives in
/// [`elf_reference`](super::elf_reference); the ELF backend owns the real one.
pub trait Resolver: Sync {
    /// Compares two defined candidates, ignoring input position.
    /// [`Ordering::Greater`] means `a` takes precedence over `b`;
    /// [`Ordering::Equal`] defers to input position (earlier wins).
    ///
    /// Neither argument is ever [`DefinitionKind::Undefined`].
    fn compare(&self, a: &Definition, b: &Definition) -> Ordering;

    /// Returns `true` if a live definition `other` that lost to `winner` is a
    /// duplicate-definition error. Checked once, after resolution settles, for
    /// every live definition that is not the winner.
    fn is_duplicate(&self, winner: &Definition, other: &Definition) -> bool;

    /// Returns `true` if a non-weak reference to a symbol whose best
    /// definition is `current` should extract `current.file` from its
    /// archive. The default extracts exactly when the best definition is
    /// lazy.
    fn extracts(&self, current: &Definition) -> bool {
        current.kind == DefinitionKind::Lazy
    }
}

/// Returns `true` if `candidate` should replace `current` under `resolver`.
///
/// An undefined `current` is always replaced; an undefined `candidate` never
/// replaces anything. Otherwise the resolver decides, and ties go to the
/// lower [`Definition::tie_key`].
#[inline]
pub fn takes_precedence<R: Resolver + ?Sized>(
    resolver: &R,
    candidate: &Definition,
    current: &Definition,
) -> bool {
    if !candidate.is_defined() {
        return false;
    }
    if !current.is_defined() {
        return true;
    }
    match resolver.compare(candidate, current) {
        Ordering::Greater => true,
        Ordering::Less => false,
        Ordering::Equal => candidate.tie_key() < current.tie_key(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Positional;

    impl Resolver for Positional {
        fn compare(&self, _: &Definition, _: &Definition) -> Ordering {
            Ordering::Equal
        }

        fn is_duplicate(&self, _: &Definition, _: &Definition) -> bool {
            false
        }
    }

    fn def(kind: DefinitionKind, file: u32, position: u64) -> Definition {
        Definition {
            kind,
            file: FileId::from_u32(file),
            index: 0,
            position: InputPosition::from_raw(position),
            aux: 0,
        }
    }

    #[test]
    fn undefined_never_wins_and_always_loses() {
        let d = def(DefinitionKind::Lazy, 0, 5);
        assert!(takes_precedence(&Positional, &d, &Definition::undefined()));
        assert!(!takes_precedence(&Positional, &Definition::undefined(), &d));
        assert!(!takes_precedence(
            &Positional,
            &Definition::undefined(),
            &Definition::undefined()
        ));
    }

    #[test]
    fn ties_go_to_the_earlier_position() {
        let early = def(DefinitionKind::Regular, 7, 1);
        let late = def(DefinitionKind::Regular, 3, 2);
        assert!(takes_precedence(&Positional, &early, &late));
        assert!(!takes_precedence(&Positional, &late, &early));
        assert!(!takes_precedence(&Positional, &early, &early));
    }

    #[test]
    fn kind_round_trips_through_u8() {
        for kind in [
            DefinitionKind::Undefined,
            DefinitionKind::Lazy,
            DefinitionKind::Shared,
            DefinitionKind::Weak,
            DefinitionKind::Common,
            DefinitionKind::Regular,
        ] {
            assert_eq!(DefinitionKind::from_u8(kind as u8), kind);
        }
        assert_eq!(DefinitionKind::from_u8(200), DefinitionKind::Undefined);
    }
}
