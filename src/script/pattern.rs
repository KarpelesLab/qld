//! Wildcard patterns for input section descriptions.
//!
//! GNU ld matches file names with `fnmatch(pattern, name, 0)` and section
//! names with a variant that first compares the literal prefix and suffix of
//! the pattern. Both are reproduced here, byte for byte:
//!
//! - `*` matches any run of bytes (including `/` and a leading `.`), `?` any
//!   one byte, and `[...]` a byte set with `!` or `^` negation, ranges, a
//!   leading `]`, and `[:class:]` names. An unterminated `[` is literal.
//! - A backslash escapes the next byte, but only in patterns containing a
//!   wildcard (`*`, `?` or `[`): GNU compares wildcard-free patterns with
//!   `strcmp`, so `\` is then an ordinary byte.
//! - File patterns of the form `archive:member` (`libc.a:printf.o`,
//!   `libc.a:`, `:crt1.o`) select archive members; see [`file_matches`].
//!
//! Patterns are compiled once. Literal, `prefix*` and `*suffix` patterns,
//! which are most of the patterns in real scripts, skip the general matcher.
//! Matching is on bytes; multibyte locales are not considered.

#![deny(clippy::arithmetic_side_effects)]

use std::fmt;

/// A compiled wildcard pattern.
///
/// Create section name patterns with [`Pattern::section`] and file name
/// patterns with [`Pattern::file`]; they differ only in rare corner cases
/// that follow GNU ld's two matching routines.
#[derive(Clone, PartialEq, Eq)]
pub struct Pattern {
    text: Box<[u8]>,
    kind: Kind,
    /// For file patterns containing `:`, the archive and member parts.
    archive: Option<Box<(Pattern, Pattern)>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Kind {
    /// `*`.
    All,
    /// No wildcard: the whole text must be equal.
    Literal,
    /// `text[..len]` then `*`.
    Prefix(usize),
    /// `*` then `text[1..]`.
    Suffix,
    /// GNU's section matcher: literal prefix and suffix checks, then
    /// `fnmatch` from the end of the prefix.
    Section {
        prefix: usize,
        suffix: usize,
        glob: Glob,
    },
    /// Plain `fnmatch`.
    Glob(Glob),
}

/// One element of a compiled glob.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Tok {
    Byte(u8),
    Any,
    Star,
    Set(Box<[u64; 4]>),
}

/// A compiled `fnmatch` pattern. `None` when the pattern can never match (a
/// trailing backslash, or an unknown `[:class:]`).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Glob(Option<Box<[Tok]>>);

fn has_wildcard(text: &[u8]) -> bool {
    text.iter().any(|&b| matches!(b, b'*' | b'?' | b'['))
}

impl Pattern {
    /// Compiles a section name pattern, as written inside the parentheses of
    /// an input section description.
    #[must_use]
    pub fn section(text: &[u8]) -> Self {
        let kind = if text == b"*" {
            Kind::All
        } else if !has_wildcard(text) {
            Kind::Literal
        } else {
            let prefix = text
                .iter()
                .position(|&b| matches!(b, b'*' | b'?' | b'['))
                .unwrap_or(text.len());
            let rest = text.get(prefix..).unwrap_or_default();
            let suffix = rest
                .iter()
                .rev()
                .position(|&b| matches!(b, b'*' | b'?' | b']'))
                .unwrap_or(rest.len());
            let simple_star = prefix.checked_add(suffix).and_then(|n| n.checked_add(1))
                == Some(text.len())
                && text.get(prefix) == Some(&b'*');
            if simple_star && suffix == 0 {
                Kind::Prefix(prefix)
            } else {
                Kind::Section {
                    prefix,
                    suffix,
                    glob: Glob::compile(rest),
                }
            }
        };
        Self {
            text: text.into(),
            kind,
            archive: None,
        }
    }

    /// Compiles a file name pattern, as written before the parentheses of an
    /// input section description or inside `EXCLUDE_FILE`.
    #[must_use]
    pub fn file(text: &[u8]) -> Self {
        let archive = text.iter().position(|&b| b == b':').map(|colon| {
            let (archive, member) = text.split_at(colon);
            let member = member.get(1..).unwrap_or_default();
            Box::new((Self::file_name(archive), Self::file_name(member)))
        });
        Self {
            archive,
            ..Self::file_name(text)
        }
    }

    fn file_name(text: &[u8]) -> Self {
        let simple = !text.contains(&b'\\') && !text.contains(&b'?') && !text.contains(&b'[');
        let stars = text.iter().filter(|&&b| b == b'*').count();
        let kind = if text == b"*" {
            Kind::All
        } else if !has_wildcard(text) {
            Kind::Literal
        } else if simple && stars == 1 && text.last() == Some(&b'*') {
            Kind::Prefix(text.len().saturating_sub(1))
        } else if simple && stars == 1 && text.first() == Some(&b'*') {
            Kind::Suffix
        } else {
            Kind::Glob(Glob::compile(text))
        };
        Self {
            text: text.into(),
            kind,
            archive: None,
        }
    }

    /// The pattern as written.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.text
    }

    /// Whether the pattern contains `*`, `?` or `[`.
    #[must_use]
    pub fn is_wildcard(&self) -> bool {
        has_wildcard(&self.text)
    }

    /// Whether the pattern is exactly `*`.
    #[must_use]
    pub fn matches_everything(&self) -> bool {
        self.kind == Kind::All
    }

    /// The literal bytes every match starts with. Callers can index patterns
    /// by this to avoid testing each one.
    #[must_use]
    pub fn literal_prefix(&self) -> &[u8] {
        let len = match &self.kind {
            Kind::All | Kind::Suffix => 0,
            Kind::Literal => self.text.len(),
            Kind::Prefix(len) => *len,
            Kind::Section { prefix, .. } => *prefix,
            Kind::Glob(_) => self
                .text
                .iter()
                .position(|&b| matches!(b, b'*' | b'?' | b'[' | b'\\'))
                .unwrap_or(self.text.len()),
        };
        self.text.get(..len).unwrap_or_default()
    }

    /// Tests a name against the whole pattern. For file patterns this does
    /// not interpret `archive:member`; use [`file_matches`] for that.
    #[must_use]
    pub fn matches(&self, name: &[u8]) -> bool {
        match &self.kind {
            Kind::All => true,
            Kind::Literal => name == &*self.text,
            Kind::Prefix(len) => self
                .text
                .get(..*len)
                .is_some_and(|prefix| name.starts_with(prefix)),
            Kind::Suffix => self
                .text
                .get(1..)
                .is_some_and(|suffix| name.ends_with(suffix)),
            Kind::Section {
                prefix,
                suffix,
                glob,
            } => self.section_match(name, *prefix, *suffix, glob),
            Kind::Glob(glob) => glob.matches(name),
        }
    }

    /// GNU ld's `spec_match`, including its quirk that the prefix and suffix
    /// checks may overlap (`abc*bcd` matches `abcd`).
    fn section_match(&self, name: &[u8], prefix: usize, suffix: usize, glob: &Glob) -> bool {
        let text = &*self.text;
        if prefix > 0 && text.get(..prefix).is_none_or(|p| !name.starts_with(p)) {
            return false;
        }
        if suffix > 0 {
            let tail = text.len().checked_sub(suffix).and_then(|at| text.get(at..));
            if tail.is_none_or(|t| !name.ends_with(t)) {
                return false;
            }
        }
        if prefix.checked_add(suffix).and_then(|n| n.checked_add(1)) == Some(text.len())
            && text.get(prefix) == Some(&b'*')
        {
            return true;
        }
        name.get(prefix..).is_some_and(|rest| glob.matches(rest))
    }
}

impl fmt::Debug for Pattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Pattern({:?})", String::from_utf8_lossy(&self.text))
    }
}

/// Tests an input file against a file pattern the way GNU ld does.
///
/// `file` is the file's path, or its member name when it is an archive
/// member, in which case `archive` is the archive's path. Rules:
///
/// - `archive:member` matches members whose archive matches `archive` and
///   whose name matches `member`; either part may be empty (`libc.a:` is any
///   member of `libc.a`, `:foo.o` is a `foo.o` that is not in an archive).
/// - Otherwise the pattern is matched against `file`. A pattern without
///   wildcards also matches every member of an archive of that name; in
///   `EXCLUDE_FILE` (`in_exclude`), so does a wildcard pattern.
#[must_use]
pub fn file_matches(
    pattern: &Pattern,
    file: &[u8],
    archive: Option<&[u8]>,
    in_exclude: bool,
) -> bool {
    if let Some(parts) = &pattern.archive {
        let (archive_pattern, member_pattern) = &**parts;
        let member_ok = member_pattern.as_bytes().is_empty() || member_pattern.matches(file);
        let has_archive_part = !archive_pattern.as_bytes().is_empty();
        if !member_ok || has_archive_part != archive.is_some() {
            return false;
        }
        return match archive {
            Some(archive) if has_archive_part => archive_pattern.matches(archive),
            _ => true,
        };
    }
    if pattern.matches(file) {
        return true;
    }
    (in_exclude || !pattern.is_wildcard()) && archive.is_some_and(|a| pattern.matches(a))
}

/// The `init_priority` encoded in a section name, as `SORT_BY_INIT_PRIORITY`
/// uses it: the decimal number after the last `.` of `.init_array.N` or
/// `.fini_array.N`, and `65535 - N` for `.ctors.N` and `.dtors.N`. `None`
/// when the name carries no priority (such sections sort by name).
#[must_use]
pub fn init_priority(name: &[u8]) -> Option<u32> {
    let dot = name.iter().rposition(|&b| b == b'.')?;
    let digits = name.get(dot.checked_add(1)?..)?;
    if !digits.first()?.is_ascii_digit() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let mut value: u64 = 0;
    for &d in digits {
        value = value
            .checked_mul(10)
            .and_then(|v| v.checked_add(u64::from(d.wrapping_sub(b'0'))))?;
    }
    if dot == 6 && (name.starts_with(b".ctors") || name.starts_with(b".dtors")) {
        value = 65535u64.checked_sub(value)?;
    }
    u32::try_from(value).ok().filter(|&v| v <= i32::MAX as u32)
}

impl Glob {
    fn compile(pattern: &[u8]) -> Self {
        let mut toks = Vec::new();
        let mut i = 0usize;
        while let Some(&c) = pattern.get(i) {
            i = i.saturating_add(1);
            match c {
                b'*' => {
                    if toks.last() != Some(&Tok::Star) {
                        toks.push(Tok::Star);
                    }
                }
                b'?' => toks.push(Tok::Any),
                b'\\' => match pattern.get(i) {
                    Some(&next) => {
                        toks.push(Tok::Byte(next));
                        i = i.saturating_add(1);
                    }
                    None => return Self(None),
                },
                b'[' => match parse_bracket(pattern, i) {
                    Bracket::Set(set, next) => {
                        toks.push(Tok::Set(set));
                        i = next;
                    }
                    Bracket::Literal => toks.push(Tok::Byte(b'[')),
                    Bracket::Never => return Self(None),
                },
                _ => toks.push(Tok::Byte(c)),
            }
        }
        Self(Some(toks.into_boxed_slice()))
    }

    fn matches(&self, s: &[u8]) -> bool {
        let Some(toks) = &self.0 else {
            return false;
        };
        let mut p = 0usize;
        let mut i = 0usize;
        // Resume point after the most recent `*`: (token index, string index).
        let mut star: Option<(usize, usize)> = None;
        loop {
            match toks.get(p) {
                Some(Tok::Star) => {
                    p = p.saturating_add(1);
                    star = Some((p, i));
                    continue;
                }
                Some(tok) => {
                    if let Some(&byte) = s.get(i) {
                        let ok = match tok {
                            Tok::Byte(b) => *b == byte,
                            Tok::Any => true,
                            Tok::Set(set) => set_contains(set, byte),
                            Tok::Star => false,
                        };
                        if ok {
                            p = p.saturating_add(1);
                            i = i.saturating_add(1);
                            continue;
                        }
                    }
                }
                None if i == s.len() => return true,
                None => {}
            }
            // Mismatch: let the last `*` absorb one more byte.
            match star {
                Some((resume, from)) if from < s.len() => {
                    let from = from.saturating_add(1);
                    star = Some((resume, from));
                    p = resume;
                    i = from;
                }
                _ => return false,
            }
        }
    }
}

enum Bracket {
    Set(Box<[u64; 4]>, usize),
    Literal,
    Never,
}

fn set_insert(set: &mut [u64; 4], b: u8) {
    if let Some(word) = set.get_mut(usize::from(b >> 6)) {
        *word |= 1u64 << (b & 63);
    }
}

fn set_contains(set: &[u64; 4], b: u8) -> bool {
    set.get(usize::from(b >> 6))
        .is_some_and(|word| word & (1u64 << (b & 63)) != 0)
}

fn class_contains(name: &[u8], b: u8) -> Option<bool> {
    Some(match name {
        b"alnum" => b.is_ascii_alphanumeric(),
        b"alpha" => b.is_ascii_alphabetic(),
        b"blank" => b == b' ' || b == b'\t',
        b"cntrl" => b.is_ascii_control(),
        b"digit" => b.is_ascii_digit(),
        b"graph" => b.is_ascii_graphic(),
        b"lower" => b.is_ascii_lowercase(),
        b"print" => b.is_ascii_graphic() || b == b' ',
        b"punct" => b.is_ascii_punctuation(),
        b"space" => b.is_ascii_whitespace() || b == 0x0b,
        b"upper" => b.is_ascii_uppercase(),
        b"xdigit" => b.is_ascii_hexdigit(),
        _ => return None,
    })
}

/// Parses a bracket expression starting just after `[`.
fn parse_bracket(p: &[u8], start: usize) -> Bracket {
    let mut set = Box::new([0u64; 4]);
    let mut j = start;
    let negate = matches!(p.get(j), Some(b'!' | b'^'));
    if negate {
        j = j.saturating_add(1);
    }
    let mut first = true;
    loop {
        let Some(&c) = p.get(j) else {
            return Bracket::Literal;
        };
        if c == b']' && !first {
            j = j.saturating_add(1);
            break;
        }
        first = false;
        let low = if c == b'\\' {
            let Some(&escaped) = p.get(j.saturating_add(1)) else {
                return Bracket::Never;
            };
            j = j.saturating_add(2);
            escaped
        } else if c == b'[' && p.get(j.saturating_add(1)) == Some(&b':') {
            let name_start = j.saturating_add(2);
            let tail = p.get(name_start..).unwrap_or_default();
            let Some(len) = tail.windows(2).position(|w| w == b":]") else {
                return Bracket::Literal;
            };
            let name = tail.get(..len).unwrap_or_default();
            if class_contains(name, 0).is_none() {
                return Bracket::Never;
            }
            for b in 0..=u8::MAX {
                if class_contains(name, b) == Some(true) {
                    set_insert(&mut set, b);
                }
            }
            j = name_start.saturating_add(len).saturating_add(2);
            continue;
        } else {
            j = j.saturating_add(1);
            c
        };
        if p.get(j) == Some(&b'-')
            && let Some(&high) = p.get(j.saturating_add(1))
            && high != b']'
        {
            let (high, width) = if high == b'\\' {
                match p.get(j.saturating_add(2)) {
                    Some(&h) => (h, 3),
                    None => return Bracket::Never,
                }
            } else {
                (high, 2)
            };
            j = j.saturating_add(width);
            for b in low..=high {
                set_insert(&mut set, b);
            }
        } else {
            set_insert(&mut set, low);
        }
    }
    if negate {
        for word in set.iter_mut() {
            *word = !*word;
        }
    }
    Bracket::Set(set, j)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sec(p: &str, name: &str) -> bool {
        Pattern::section(p.as_bytes()).matches(name.as_bytes())
    }

    fn file(p: &str, name: &str) -> bool {
        Pattern::file(p.as_bytes()).matches(name.as_bytes())
    }

    /// Reference `fnmatch` written as a direct recursive definition, to
    /// cross-check the iterative matcher.
    fn reference(p: &[u8], s: &[u8]) -> bool {
        match p.first() {
            None => s.is_empty(),
            Some(b'*') => (0..=s.len()).any(|k| reference(&p[1..], &s[k..])),
            Some(b'?') => !s.is_empty() && reference(&p[1..], &s[1..]),
            Some(&c) => s.first() == Some(&c) && reference(&p[1..], &s[1..]),
        }
    }

    #[test]
    fn literal_and_star_forms() {
        assert!(sec(".text", ".text"));
        assert!(!sec(".text", ".text.foo"));
        assert!(!sec(".text", ".tex"));
        assert!(sec(".text.*", ".text.hot"));
        assert!(sec(".text.*", ".text."));
        assert!(!sec(".text.*", ".text"));
        assert!(sec("*", ""));
        assert!(sec("*", "anything"));
        assert!(sec(".text.*_unlikely", ".text.foo_unlikely"));
        assert!(!sec(".text.*_unlikely", ".text.foo_likely"));
        assert!(sec(".gnu.linkonce.t.*", ".gnu.linkonce.t.x"));
        assert!(file("*crtbegin.o", "/usr/lib/gcc/crtbegin.o"));
        assert!(file("*crtbegin?.o", "/usr/lib/gcc/crtbeginS.o"));
        assert!(!file("*crtbegin?.o", "/usr/lib/gcc/crtbegin.o"));
        assert!(file("foo*", "foobar"));
        assert!(file("foo.o", "foo.o"));
    }

    #[test]
    fn brackets() {
        assert!(sec(".te[a-x]t", ".text"));
        assert!(!sec(".te[!a-x]t", ".text"));
        assert!(sec(".te[^a-w]t", ".text"));
        assert!(sec("[]]", "]"));
        assert!(sec("[!]]", "a"));
        assert!(sec("[a-]", "-"));
        assert!(sec("[[:digit:]]x", "7x"));
        assert!(!sec("[[:digit:]]x", "ax"));
        assert!(!sec("[[:bogus:]]", "a"));
        // Unterminated bracket: literal `[`.
        assert!(file("a[b", "a[b"));
        assert!(sec("*[b", "xx[b"));
        assert!(sec(".t[\\]]x", ".t]x"));
    }

    #[test]
    fn backslashes_follow_gnu() {
        // No wildcard: compared literally.
        assert!(!sec(".te\\xt", ".text"));
        assert!(sec(".te\\xt", ".te\\xt"));
        // With a wildcard, the escape applies.
        assert!(file("*\\*", "a*"));
        assert!(!file("*\\*", "ab"));
        assert!(!file("*\\", "a\\"));
    }

    #[test]
    fn gnu_section_quirk() {
        // spec_match checks prefix and suffix independently.
        assert!(sec("abc*bcd", "abcd"));
        assert!(!file("abc*bcd", "abcd"));
    }

    #[test]
    fn archive_member_patterns() {
        let p = Pattern::file(b"libc.a:printf.o");
        assert!(file_matches(&p, b"printf.o", Some(b"libc.a"), false));
        assert!(!file_matches(&p, b"printf.o", Some(b"libm.a"), false));
        assert!(!file_matches(&p, b"printf.o", None, false));
        let p = Pattern::file(b"*libc.a:");
        assert!(file_matches(&p, b"x.o", Some(b"/usr/lib/libc.a"), false));
        assert!(!file_matches(&p, b"x.o", None, false));
        let p = Pattern::file(b":crt1.o");
        assert!(file_matches(&p, b"crt1.o", None, false));
        assert!(!file_matches(&p, b"crt1.o", Some(b"libc.a"), false));
        // A literal file name also selects members of an archive of that name.
        let p = Pattern::file(b"libfoo.a");
        assert!(file_matches(&p, b"a.o", Some(b"libfoo.a"), false));
        let p = Pattern::file(b"*foo.a");
        assert!(!file_matches(&p, b"a.o", Some(b"libfoo.a"), false));
        assert!(file_matches(&p, b"a.o", Some(b"libfoo.a"), true));
    }

    #[test]
    fn init_priorities() {
        assert_eq!(init_priority(b".init_array.101"), Some(101));
        assert_eq!(init_priority(b".ctors.101"), Some(65434));
        assert_eq!(init_priority(b".init_array"), None);
        assert_eq!(init_priority(b".init_array.1x"), None);
        assert_eq!(init_priority(b".ctors.70000"), None);
        assert_eq!(init_priority(b".foo.99999999999999999999999"), None);
    }

    #[test]
    fn literal_prefixes() {
        assert_eq!(Pattern::section(b".text.*").literal_prefix(), b".text.");
        assert_eq!(Pattern::section(b"*").literal_prefix(), b"");
        assert_eq!(Pattern::file(b"ab\\*c*").literal_prefix(), b"ab");
        assert!(Pattern::section(b"*").matches_everything());
    }

    #[test]
    fn randomized_against_reference() {
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let alphabet = b"ab.*?";
        for _ in 0..20_000 {
            let plen = (next() % 7) as usize;
            let slen = (next() % 8) as usize;
            let p: Vec<u8> = (0..plen)
                .map(|_| alphabet[(next() % alphabet.len() as u64) as usize])
                .collect();
            let s: Vec<u8> = (0..slen).map(|_| alphabet[(next() % 3) as usize]).collect();
            let expected = reference(&p, &s);
            let compiled = Pattern::file(&p);
            assert_eq!(compiled.matches(&s), expected, "{p:?} {s:?}");
        }
    }
}
