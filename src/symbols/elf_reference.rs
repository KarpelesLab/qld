//! A reference implementation of ELF symbol precedence.
//!
//! This exists to test the format-neutral machinery against realistic rules
//! and to document them. The ELF backend owns the rules it actually links
//! with (`src/elf/`), and may refine them (for example `STB_GNU_UNIQUE`,
//! visibility merging, `--warn-common`, COMDAT handling); nothing outside
//! tests should depend on this module.
//!
//! The ranking, from strongest to weakest:
//!
//! 1. [`Regular`](DefinitionKind::Regular) (`STB_GLOBAL` defined). Two of
//!    these are a duplicate-definition error; the earlier one is kept.
//! 2. [`Common`](DefinitionKind::Common). Among commons the larger size
//!    ([`Definition::aux`]) wins; equal sizes go to the earlier input.
//! 3. [`Weak`](DefinitionKind::Weak) (`STB_WEAK` defined).
//! 4. [`Shared`](DefinitionKind::Shared) (defined in a DSO).
//! 5. [`Lazy`](DefinitionKind::Lazy) (defined in an unextracted archive
//!    member).
//!
//! This follows lld: a common symbol replaces a weak definition and is
//! replaced by a strong one, and archive members are extracted only for
//! non-weak references. One difference is deliberate: a shared definition
//! beats a lazy one wherever the two sit on the command line, so a reference
//! that a shared library satisfies never extracts an archive member (lld and
//! GNU ld extract the member if the archive comes first).
//!
//! Note that `docs/architecture.md` lists the ELF order as
//! "strong > weak > common > lazy > shared"; the order implemented here (and
//! in GNU ld, gold, lld and mold) puts common above weak and shared above
//! lazy.

use core::cmp::Ordering;

use super::definition::{Definition, DefinitionKind, Resolver};

/// ELF precedence rules; see the [module documentation](self).
#[derive(Clone, Copy, Debug, Default)]
pub struct ElfReferenceRules;

impl ElfReferenceRules {
    /// Returns the rank of a definition kind; higher takes precedence.
    #[must_use]
    pub const fn rank(kind: DefinitionKind) -> u8 {
        match kind {
            DefinitionKind::Undefined => 0,
            DefinitionKind::Lazy => 1,
            DefinitionKind::Shared => 2,
            DefinitionKind::Weak => 3,
            DefinitionKind::Common => 4,
            DefinitionKind::Regular => 5,
        }
    }
}

impl Resolver for ElfReferenceRules {
    fn compare(&self, a: &Definition, b: &Definition) -> Ordering {
        let by_rank = Self::rank(a.kind).cmp(&Self::rank(b.kind));
        if by_rank == Ordering::Equal && a.kind == DefinitionKind::Common {
            // Larger common wins.
            return a.aux.cmp(&b.aux);
        }
        by_rank
    }

    fn is_duplicate(&self, winner: &Definition, other: &Definition) -> bool {
        winner.kind == DefinitionKind::Regular && other.kind == DefinitionKind::Regular
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::FileId;
    use crate::symbols::definition::takes_precedence;
    use crate::symbols::name::InputPosition;

    fn def(kind: DefinitionKind, position: u32, aux: u64) -> Definition {
        Definition {
            kind,
            file: FileId::new(position as usize),
            index: 0,
            position: InputPosition::new(position, 0),
            aux,
        }
    }

    const KINDS_WEAKEST_FIRST: [DefinitionKind; 5] = [
        DefinitionKind::Lazy,
        DefinitionKind::Shared,
        DefinitionKind::Weak,
        DefinitionKind::Common,
        DefinitionKind::Regular,
    ];

    #[test]
    fn stronger_kind_wins_regardless_of_position() {
        let rules = ElfReferenceRules;
        for (i, &weaker) in KINDS_WEAKEST_FIRST.iter().enumerate() {
            for &stronger in &KINDS_WEAKEST_FIRST[i + 1..] {
                // The weaker candidate comes first on the command line; it
                // still loses.
                let w = def(weaker, 0, 8);
                let s = def(stronger, 1, 8);
                assert!(
                    takes_precedence(&rules, &s, &w),
                    "{stronger:?} > {weaker:?}"
                );
                assert!(
                    !takes_precedence(&rules, &w, &s),
                    "{weaker:?} < {stronger:?}"
                );
            }
        }
    }

    #[test]
    fn same_kind_goes_to_earlier_input() {
        let rules = ElfReferenceRules;
        for kind in KINDS_WEAKEST_FIRST {
            let early = def(kind, 3, 8);
            let late = def(kind, 9, 8);
            assert!(takes_precedence(&rules, &early, &late), "{kind:?}");
            assert!(!takes_precedence(&rules, &late, &early), "{kind:?}");
        }
    }

    #[test]
    fn larger_common_wins() {
        let rules = ElfReferenceRules;
        let small_early = def(DefinitionKind::Common, 0, 4);
        let large_late = def(DefinitionKind::Common, 5, 16);
        assert!(takes_precedence(&rules, &large_late, &small_early));
        assert!(!takes_precedence(&rules, &small_early, &large_late));
    }

    #[test]
    fn size_only_matters_between_commons() {
        let rules = ElfReferenceRules;
        let weak_huge = def(DefinitionKind::Weak, 0, 1 << 40);
        let common_small = def(DefinitionKind::Common, 1, 1);
        let strong_small = def(DefinitionKind::Regular, 2, 0);
        assert!(takes_precedence(&rules, &common_small, &weak_huge));
        assert!(takes_precedence(&rules, &strong_small, &common_small));
    }

    #[test]
    fn only_two_strong_definitions_conflict() {
        let rules = ElfReferenceRules;
        for a in KINDS_WEAKEST_FIRST {
            for b in KINDS_WEAKEST_FIRST {
                let expected = a == DefinitionKind::Regular && b == DefinitionKind::Regular;
                assert_eq!(
                    rules.is_duplicate(&def(a, 0, 0), &def(b, 1, 0)),
                    expected,
                    "{a:?} vs {b:?}"
                );
            }
        }
    }

    #[test]
    fn only_lazy_definitions_extract() {
        let rules = ElfReferenceRules;
        for kind in KINDS_WEAKEST_FIRST {
            assert_eq!(
                rules.extracts(&def(kind, 0, 0)),
                kind == DefinitionKind::Lazy
            );
        }
    }

    #[test]
    fn precedence_is_a_total_order() {
        // Every pair of distinct candidates is strictly ordered one way, and
        // the order is transitive; that is what makes insertion order
        // irrelevant.
        let rules = ElfReferenceRules;
        let mut all = Vec::new();
        for kind in KINDS_WEAKEST_FIRST {
            for position in 0..3 {
                for aux in [1, 2] {
                    let mut d = def(kind, position, aux);
                    d.index = aux as u32;
                    all.push(d);
                }
            }
        }
        for a in &all {
            for b in &all {
                if a == b {
                    assert!(!takes_precedence(&rules, a, b));
                    continue;
                }
                assert_ne!(
                    takes_precedence(&rules, a, b),
                    takes_precedence(&rules, b, a),
                    "{a:?} / {b:?}"
                );
                for c in &all {
                    if takes_precedence(&rules, a, b) && takes_precedence(&rules, b, c) {
                        assert!(takes_precedence(&rules, a, c));
                    }
                }
            }
        }
    }
}
