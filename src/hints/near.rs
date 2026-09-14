//! Near misses: defined names an undefined reference probably meant.
//!
//! For each undefined name, [`near_misses`] looks through the names the link
//! defines for:
//!
//! - a C++ definition of a name referenced with C linkage (`foo` versus
//!   `_Z3fooi`), or the reverse, matched by demangled base name;
//! - a leading underscore too many or too few (`_foo` versus `foo`);
//! - the same C++ entity with different parameter types, member function
//!   qualifiers, or scope;
//! - a small edit distance (names of 4 bytes or more).
//!
//! Base names are found without demangling every defined name: mangled names
//! spell each identifier as `<length><identifier>`, so only defined names
//! containing a wanted identifier that way are demangled.

use hashbrown::{HashMap, HashSet};
use rayon::prelude::*;

use crate::demangle::{self, Options, Parts};

type FastSet<'a> = HashSet<&'a [u8], foldhash::fast::FixedState>;
type FastMap<K, V> = HashMap<K, V, foldhash::fast::FixedState>;

/// At most this many near misses are suggested per undefined name.
pub const MAX_NEAR_MISSES: usize = 3;

/// Names shorter than this get no edit-distance suggestions.
const MIN_SPELLING_LEN: usize = 4;

/// Only the first this many undefined names get edit-distance suggestions,
/// the one check that looks at every defined name.
const MAX_SPELLING_QUERIES: usize = 256;

/// How a near miss differs from the undefined reference.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum NearMissKind {
    /// The name differs by a leading underscore.
    Underscore,
    /// The reference has C linkage; the definition is the C++ (or Rust)
    /// entity of that name.
    CppDefinition,
    /// The reference is to a C++ entity; the definition has C linkage.
    CDefinition,
    /// Same entity, different parameter types.
    Parameters,
    /// Same entity and parameters, different `const`/`volatile`/`&`/`&&`
    /// member function qualifiers.
    Qualifiers,
    /// Same name and parameters, declared in a different namespace or class.
    Scope,
    /// A similar spelling: the edit distance is given.
    Spelling(usize),
}

/// A defined name close to an undefined one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NearMiss {
    /// The defined name, as mangled.
    pub candidate: Vec<u8>,
    /// How it differs.
    pub kind: NearMissKind,
}

impl NearMissKind {
    fn rank(self) -> (u8, usize) {
        match self {
            Self::Underscore => (0, 0),
            Self::CppDefinition | Self::CDefinition => (1, 0),
            Self::Parameters | Self::Qualifiers => (2, 0),
            Self::Scope => (3, 0),
            Self::Spelling(distance) => (4, distance),
        }
    }
}

/// A query: the undefined name and what demangling tells about it.
struct Query<'a> {
    name: &'a [u8],
    parts: Option<Parts>,
    /// The identifier to look for: the name itself for plain names, the
    /// demangled base name for mangled ones.
    base: Vec<u8>,
}

/// Finds near misses for each of `undefined` among `defined`, at most
/// [`MAX_NEAR_MISSES`] each, best first. The result is in the order of
/// `undefined` and does not depend on the order of `defined` beyond ties,
/// which are broken by name.
#[must_use]
pub fn near_misses(undefined: &[&[u8]], defined: &[&[u8]]) -> Vec<Vec<NearMiss>> {
    if undefined.is_empty() {
        return Vec::new();
    }
    let queries: Vec<Query<'_>> = undefined
        .par_iter()
        .map(|&name| {
            let parts = demangle::parts(name, Options::new());
            let base = match &parts {
                Some(parts) => parts.base.as_bytes().to_vec(),
                None => name.to_vec(),
            };
            Query { name, parts, base }
        })
        .collect();
    let defined_set: FastSet<'_> = defined.iter().copied().collect();
    let bases: FastSet<'_> = queries
        .iter()
        .map(|q| q.base.as_slice())
        .filter(|base| is_identifier(base))
        .collect();

    // Defined mangled names containing a wanted identifier, by identifier.
    let hits: Vec<Vec<(&[u8], u32)>> = defined
        .par_iter()
        .enumerate()
        .map(|(index, &name)| {
            if demangle::scheme(name).is_none() {
                return Vec::new();
            }
            let Ok(index) = u32::try_from(index) else {
                return Vec::new();
            };
            let mut found: Vec<(&[u8], u32)> = Vec::new();
            for token in source_names(name) {
                if let Some(&base) = bases.get(token)
                    && !found.iter().any(|(b, _)| *b == base)
                {
                    found.push((base, index));
                }
            }
            found
        })
        .collect();
    let mut by_base: FastMap<&[u8], Vec<u32>> = FastMap::default();
    for (base, index) in hits.into_iter().flatten() {
        by_base.entry(base).or_default().push(index);
    }
    let mut candidates: Vec<u32> = by_base.values().flatten().copied().collect();
    candidates.sort_unstable();
    candidates.dedup();
    let parsed: FastMap<u32, Parts> = candidates
        .par_iter()
        .filter_map(|&index| {
            let name = defined.get(usize::try_from(index).ok()?)?;
            Some((index, demangle::parts(name, Options::new())?))
        })
        .collect::<Vec<_>>()
        .into_iter()
        .collect();

    let mut by_len: Vec<Vec<u32>> = Vec::new();
    for (index, name) in defined.iter().enumerate() {
        let Ok(index) = u32::try_from(index) else {
            break;
        };
        if by_len.len() <= name.len() {
            by_len.resize_with(name.len().saturating_add(1), Vec::new);
        }
        if let Some(bucket) = by_len.get_mut(name.len()) {
            bucket.push(index);
        }
    }

    let context = Context {
        defined,
        defined_set: &defined_set,
        by_base: &by_base,
        parsed: &parsed,
        by_len: &by_len,
    };
    queries
        .par_iter()
        .enumerate()
        .map(|(index, query)| context.matches(query, index < MAX_SPELLING_QUERIES))
        .collect()
}

struct Context<'c, 'd> {
    defined: &'c [&'d [u8]],
    defined_set: &'c FastSet<'d>,
    by_base: &'c FastMap<&'c [u8], Vec<u32>>,
    parsed: &'c FastMap<u32, Parts>,
    by_len: &'c [Vec<u32>],
}

impl Context<'_, '_> {
    fn name(&self, index: u32) -> Option<&[u8]> {
        self.defined.get(usize::try_from(index).ok()?).copied()
    }

    fn matches(&self, query: &Query<'_>, spelling: bool) -> Vec<NearMiss> {
        let mut found: Vec<NearMiss> = Vec::new();
        let push = |found: &mut Vec<NearMiss>, candidate: &[u8], kind| {
            if candidate != query.name && !found.iter().any(|m| m.candidate == candidate) {
                found.push(NearMiss {
                    candidate: candidate.to_vec(),
                    kind,
                });
            }
        };

        // A leading underscore.
        if let Some(stripped) = query.name.strip_prefix(b"_")
            && !stripped.is_empty()
            && let Some(candidate) = self.defined_set.get(stripped)
        {
            push(&mut found, candidate, NearMissKind::Underscore);
        }
        let mut prefixed = Vec::with_capacity(query.name.len().saturating_add(1));
        prefixed.push(b'_');
        prefixed.extend_from_slice(query.name);
        if let Some(candidate) = self.defined_set.get(prefixed.as_slice()) {
            push(&mut found, candidate, NearMissKind::Underscore);
        }

        let same_base = self
            .by_base
            .get(query.base.as_slice())
            .map_or(&[][..], Vec::as_slice);
        match &query.parts {
            None => {
                // A C reference: C++ entities with this name, global ones
                // first.
                let mut cpp: Vec<(bool, &[u8])> = same_base
                    .iter()
                    .filter_map(|&index| {
                        let parts = self.parsed.get(&index)?;
                        (parts.base.as_bytes() == query.name)
                            .then(|| Some((!parts.scope.is_empty(), self.name(index)?)))?
                    })
                    .collect();
                cpp.sort();
                for (_, candidate) in cpp {
                    push(&mut found, candidate, NearMissKind::CppDefinition);
                }
            }
            Some(parts) => {
                if parts.scope.is_empty()
                    && parts.params.is_some()
                    && let Some(candidate) = self.defined_set.get(query.base.as_slice())
                {
                    push(&mut found, candidate, NearMissKind::CDefinition);
                }
                let mut related: Vec<(NearMissKind, &[u8])> = Vec::new();
                for &index in same_base {
                    let (Some(other), Some(candidate)) =
                        (self.parsed.get(&index), self.name(index))
                    else {
                        continue;
                    };
                    if other.base != parts.base || other.scheme != parts.scheme {
                        continue;
                    }
                    let kind = if other.name == parts.name {
                        if other.params != parts.params {
                            NearMissKind::Parameters
                        } else if other.qualifiers != parts.qualifiers {
                            NearMissKind::Qualifiers
                        } else {
                            continue;
                        }
                    } else if other.scope != parts.scope
                        && other.params == parts.params
                        && other.qualifiers == parts.qualifiers
                    {
                        NearMissKind::Scope
                    } else {
                        continue;
                    };
                    related.push((kind, candidate));
                }
                related.sort();
                for (kind, candidate) in related {
                    push(&mut found, candidate, kind);
                }
            }
        }

        if spelling && found.len() < MAX_NEAR_MISSES && query.name.len() >= MIN_SPELLING_LEN {
            let max = max_distance(query.name.len());
            let mangled = demangle::scheme(query.name).is_some();
            let mut spelled: Vec<(usize, &[u8])> = Vec::new();
            let mut rows = (Vec::new(), Vec::new(), Vec::new());
            let low = query.name.len().saturating_sub(max);
            let high = query.name.len().saturating_add(max);
            for bucket in self.by_len.iter().take(high.saturating_add(1)).skip(low) {
                for &index in bucket {
                    let Some(candidate) = self.name(index) else {
                        continue;
                    };
                    if candidate == query.name || demangle::scheme(candidate).is_some() != mangled {
                        continue;
                    }
                    if let Some(distance) = bounded_distance(query.name, candidate, max, &mut rows)
                    {
                        spelled.push((distance, candidate));
                    }
                }
            }
            spelled.sort();
            for (distance, candidate) in spelled {
                push(&mut found, candidate, NearMissKind::Spelling(distance));
            }
        }

        found.sort_by(|a, b| {
            a.kind
                .rank()
                .cmp(&b.kind.rank())
                .then_with(|| a.candidate.cmp(&b.candidate))
        });
        found.truncate(MAX_NEAR_MISSES);
        found
    }
}

/// The largest edit distance suggested for a name of `len` bytes.
fn max_distance(len: usize) -> usize {
    match len {
        0..8 => 1,
        8..16 => 2,
        _ => 3,
    }
}

/// Whether `bytes` could be an identifier (and so appear in a mangled name).
fn is_identifier(bytes: &[u8]) -> bool {
    !bytes.is_empty()
        && bytes
            .iter()
            .all(|&c| c.is_ascii_alphanumeric() || c == b'_' || c == b'$')
}

/// The `<length><identifier>` tokens of a mangled name, possibly with false
/// positives (digits inside identifiers) but never missing an identifier.
fn source_names(name: &[u8]) -> impl Iterator<Item = &[u8]> {
    let v0 = name.starts_with(b"_R") || name.starts_with(b"__R");
    let mut at = 0usize;
    std::iter::from_fn(move || {
        while at < name.len() {
            let start = at;
            at = at.saturating_add(1);
            let &c = name.get(start)?;
            let previous_is_digit = start
                .checked_sub(1)
                .and_then(|p| name.get(p))
                .is_some_and(u8::is_ascii_digit);
            if !c.is_ascii_digit() || previous_is_digit {
                continue;
            }
            let mut end = start;
            let mut len: usize = 0;
            while let Some(&d) = name.get(end).filter(|d| d.is_ascii_digit()) {
                len = len
                    .saturating_mul(10)
                    .saturating_add(usize::from(d.wrapping_sub(b'0')));
                end = end.saturating_add(1);
            }
            if v0 && name.get(end) == Some(&b'_') {
                end = end.saturating_add(1);
            }
            if len == 0 {
                continue;
            }
            if let Some(token) = end.checked_add(len).and_then(|stop| name.get(end..stop)) {
                return Some(token);
            }
        }
        None
    })
}

/// The optimal-string-alignment distance between `a` and `b` (edits,
/// adjacent transpositions), if it is at most `max`.
fn bounded_distance(
    a: &[u8],
    b: &[u8],
    max: usize,
    rows: &mut (Vec<usize>, Vec<usize>, Vec<usize>),
) -> Option<usize> {
    if a.len().abs_diff(b.len()) > max {
        return None;
    }
    const FAR: usize = usize::MAX / 2;
    let width = b.len().saturating_add(1);
    let (before, previous, current) = rows;
    before.clear();
    before.resize(width, FAR);
    previous.clear();
    previous.extend((0..width).map(|j| if j <= max { j } else { FAR }));
    current.clear();
    current.resize(width, FAR);
    for (i, &ca) in a.iter().enumerate() {
        let row = i.saturating_add(1);
        // Only cells within `max` of the diagonal can stay within `max`.
        let low = i.saturating_sub(max);
        let high = row.saturating_add(max).min(b.len());
        // The cells just outside the band are read but not computed. Cells
        // further out are never read, so they need no reset.
        if let Some(edge) = current.get_mut(low) {
            *edge = if low == 0 && row <= max { row } else { FAR };
        }
        if let Some(edge) = current.get_mut(high.saturating_add(1)) {
            *edge = FAR;
        }
        let mut row_min = if low == 0 && row <= max { row } else { FAR };
        for (j, &cb) in b.iter().enumerate().take(high).skip(low) {
            let cost = usize::from(ca != cb);
            let deletion = previous
                .get(j.saturating_add(1))
                .copied()
                .unwrap_or(usize::MAX)
                .saturating_add(1);
            let insertion = current
                .get(j)
                .copied()
                .unwrap_or(usize::MAX)
                .saturating_add(1);
            let substitution = previous
                .get(j)
                .copied()
                .unwrap_or(usize::MAX)
                .saturating_add(cost);
            let mut value = deletion.min(insertion).min(substitution);
            if i > 0
                && j > 0
                && a.get(i.wrapping_sub(1)) == Some(&cb)
                && b.get(j.wrapping_sub(1)) == Some(&ca)
            {
                let transposition = before
                    .get(j.wrapping_sub(1))
                    .copied()
                    .unwrap_or(usize::MAX)
                    .saturating_add(1);
                value = value.min(transposition);
            }
            if let Some(slot) = current.get_mut(j.saturating_add(1)) {
                *slot = value;
            }
            row_min = row_min.min(value);
        }
        if row_min > max {
            return None;
        }
        std::mem::swap(before, previous);
        std::mem::swap(previous, current);
    }
    previous.last().copied().filter(|&d| d <= max)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn distance(a: &str, b: &str, max: usize) -> Option<usize> {
        bounded_distance(a.as_bytes(), b.as_bytes(), max, &mut Default::default())
    }

    /// The plain dynamic-programming distance, for comparison.
    fn reference(a: &[u8], b: &[u8]) -> usize {
        let mut d = vec![vec![0usize; b.len() + 1]; a.len() + 1];
        for (i, row) in d.iter_mut().enumerate() {
            row[0] = i;
        }
        for (j, cell) in d[0].iter_mut().enumerate() {
            *cell = j;
        }
        for i in 1..=a.len() {
            for j in 1..=b.len() {
                let cost = usize::from(a[i - 1] != b[j - 1]);
                let mut v = (d[i - 1][j] + 1)
                    .min(d[i][j - 1] + 1)
                    .min(d[i - 1][j - 1] + cost);
                if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                    v = v.min(d[i - 2][j - 2] + 1);
                }
                d[i][j] = v;
            }
        }
        d[a.len()][b.len()]
    }

    #[test]
    fn banded_distance_matches_reference() {
        let words: Vec<&[u8]> = vec![
            b"",
            b"a",
            b"ab",
            b"ba",
            b"abc",
            b"acb",
            b"bca",
            b"memcpy",
            b"memmove",
            b"mempcy",
            b"_ZN3foo3barEv",
            b"_ZN3foo3bazEv",
            b"_ZN3fo3barEv",
            b"xyzzy",
            b"xyzy",
            b"aaaa",
            b"aaab",
            b"baaa",
            b"abab",
            b"baba",
        ];
        let mut rows = Default::default();
        for a in &words {
            for b in &words {
                let expected = reference(a, b);
                for max in 0..4 {
                    let got = bounded_distance(a, b, max, &mut rows);
                    let want = (expected <= max).then_some(expected);
                    assert_eq!(got, want, "{:?} {:?} max {max}", a, b);
                }
            }
        }
    }

    #[test]
    fn distances() {
        assert_eq!(distance("kitten", "sitting", 3), Some(3));
        assert_eq!(distance("kitten", "sitting", 2), None);
        assert_eq!(distance("abcd", "abdc", 1), Some(1));
        assert_eq!(distance("abcd", "abcd", 0), Some(0));
        assert_eq!(distance("", "ab", 2), Some(2));
        assert_eq!(distance("print", "printf", 1), Some(1));
    }

    #[test]
    fn tokens() {
        let tokens: Vec<&[u8]> = source_names(b"_ZN3foo7bar_bazEv").collect();
        assert!(tokens.contains(&&b"foo"[..]));
        assert!(tokens.contains(&&b"bar_baz"[..]));
        let tokens: Vec<&[u8]> = source_names(b"_RNvCs1234_7mycrate3foo").collect();
        assert!(tokens.contains(&&b"mycrate"[..]));
        assert!(tokens.contains(&&b"foo"[..]));
    }

    fn kinds(undefined: &str, defined: &[&str]) -> Vec<(String, NearMissKind)> {
        let defined: Vec<&[u8]> = defined.iter().map(|d| d.as_bytes()).collect();
        near_misses(&[undefined.as_bytes()], &defined)
            .remove(0)
            .into_iter()
            .map(|m| (String::from_utf8(m.candidate).unwrap(), m.kind))
            .collect()
    }

    #[test]
    fn linkage_mismatches() {
        assert_eq!(
            kinds("foo", &["_Z3fooi", "_ZN2ns3fooEv", "bar"]),
            vec![
                ("_Z3fooi".to_string(), NearMissKind::CppDefinition),
                ("_ZN2ns3fooEv".to_string(), NearMissKind::CppDefinition),
            ]
        );
        assert_eq!(
            kinds("_Z3fooi", &["foo", "fop"]),
            vec![("foo".to_string(), NearMissKind::CDefinition)]
        );
    }

    #[test]
    fn underscores_and_spelling() {
        assert_eq!(
            kinds("_start_thing", &["start_thing", "start_thin"]),
            vec![
                ("start_thing".to_string(), NearMissKind::Underscore),
                ("start_thin".to_string(), NearMissKind::Spelling(2)),
            ]
        );
        // Short names allow one edit only.
        assert_eq!(
            kinds("memcpy", &["_memcpy", "memcpy_s", "memmove", "mempcy"]),
            vec![
                ("_memcpy".to_string(), NearMissKind::Underscore),
                ("mempcy".to_string(), NearMissKind::Spelling(1)),
            ]
        );
        assert_eq!(kinds("abc", &["abd"]), vec![]);
        // Mangled names are compared by spelling with mangled names only;
        // the underscore rule still applies.
        assert_eq!(
            kinds(
                "_ZN2ns4testEv",
                &["_ZN2ns4tesTEv", "ZN2ns4testEv", "ZN2ns4tesTEv"]
            ),
            vec![
                ("ZN2ns4testEv".to_string(), NearMissKind::Underscore),
                ("_ZN2ns4tesTEv".to_string(), NearMissKind::Spelling(1)),
            ]
        );
    }

    #[test]
    fn cpp_signature_differences() {
        // f(int) referenced; f(long), f(int) const and g::f(int) defined.
        assert_eq!(
            kinds(
                "_ZN1A1fEi",
                &["_ZN1A1fEl", "_ZNK1A1fEi", "_ZN1B1fEi", "_ZN1A1gEi"]
            ),
            vec![
                ("_ZN1A1fEl".to_string(), NearMissKind::Parameters),
                ("_ZNK1A1fEi".to_string(), NearMissKind::Qualifiers),
                ("_ZN1B1fEi".to_string(), NearMissKind::Scope),
            ]
        );
    }

    #[test]
    fn deterministic_under_permutation() {
        let defined = [
            "_ZN1A1fEl",
            "_ZNK1A1fEi",
            "_ZN1B1fEi",
            "_ZN1C1fEi",
            "_ZN1D1fEi",
        ];
        let forward = kinds("_ZN1A1fEi", &defined);
        let mut reversed = defined;
        reversed.reverse();
        assert_eq!(forward, kinds("_ZN1A1fEi", &reversed));
        assert_eq!(forward.len(), MAX_NEAR_MISSES);
    }
}
