//! Tombstone values for relocations in non-allocated sections whose target
//! was discarded.
//!
//! When `--gc-sections`, COMDAT deduplication or ICF removes a function, the
//! debug information describing it stays, and its relocations have nowhere
//! to point. Resolving them normally (to the addend) would make the dead
//! function's DWARF claim low addresses that may belong to live code, so
//! linkers write a *tombstone* value instead.
//!
//! # Rules
//!
//! [`Style::Lld`] (the default) follows lld (`InputSection::relocateNonAlloc`):
//!
//! | Section | Tombstone |
//! | --- | --- |
//! | `.debug_loc`, `.debug_ranges` | `1`: in DWARF < 5 lists a `(0, 0)` pair ends the list and `-1` selects a base address, so `(1, 1)`, an empty range, is used |
//! | `.debug_names` | `u64::MAX` (a local type unit that was discarded) |
//! | other `.debug*` | `0` |
//! | anything else | none: relocate normally |
//!
//! The addend is ignored. Targets folded by ICF also get the tombstone,
//! except in `.debug_line`, so that breakpoints still work on the surviving
//! copy.
//!
//! [`Style::Gnu`] follows GNU ld 2.46 (`_bfd_clear_contents`): the field is
//! cleared to `0` in every section, except `.debug_ranges`, which gets `1`.
//! This was checked by linking with `ld --gc-sections` and reading the
//! output (see `tests/debug.rs`): `.debug_loc` entries for a collected
//! function become `(0, 0)` under GNU ld but `(1, 1)` under lld; DWARF 5
//! `.debug_rnglists`, `.debug_loclists`, `.debug_aranges`, `.debug_line`
//! and `.debug_info` addresses become `0` under both. GNU ld has no ICF.
//!
//! User rules from `-z dead-reloc-in-nonalloc=<glob>=<value>` take
//! precedence over the built-in rules, the last matching rule winning, and
//! apply to any non-allocated section (not only `.debug*`), as in lld.
//!
//! The DWARF version does not change any rule: the `1` for `.debug_loc` and
//! `.debug_ranges` matters only to their pre-DWARF 5 formats, and DWARF 5's
//! `.debug_loclists`/`.debug_rnglists` use explicit end markers.
//!
//! # Applying
//!
//! The linker calls [`Tombstones::for_section`] once per non-allocated input
//! section, then, for each relocation whose target section is dead, writes
//! the value truncated to the field's width ([`truncate`]) instead of
//! relocating. Only absolute relocations (and TLS offsets such as
//! `R_X86_64_DTPOFF32`) should be replaced; lld relocates others normally.

/// Which linker's tombstone values to reproduce.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Style {
    /// lld's values (the default).
    #[default]
    Lld,
    /// GNU ld's values.
    Gnu,
}

/// Why a relocation's target is gone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DeadTarget {
    /// The target section was garbage-collected or discarded (COMDAT,
    /// `/DISCARD/`).
    Discarded,
    /// The target was folded into an identical section by ICF.
    Folded,
}

/// The tombstones that apply to one input section.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct SectionTombstone {
    /// Value for relocations against discarded sections; `None` means
    /// relocate normally.
    pub discarded: Option<u64>,
    /// Value for relocations against ICF-folded sections; `None` means
    /// relocate normally (to the surviving copy).
    pub folded: Option<u64>,
}

impl SectionTombstone {
    /// The value for a dead target of the given kind, if any.
    #[must_use]
    pub fn get(&self, dead: DeadTarget) -> Option<u64> {
        match dead {
            DeadTarget::Discarded => self.discarded,
            DeadTarget::Folded => self.folded,
        }
    }
}

/// An invalid `-z dead-reloc-in-nonalloc=` rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuleError(pub String);

impl std::fmt::Display for RuleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RuleError {}

/// Tombstone rules for a link: a [`Style`] plus user overrides.
#[derive(Clone, Debug, Default)]
pub struct Tombstones {
    style: Style,
    rules: Vec<(Glob, u64)>,
}

impl Tombstones {
    /// The built-in rules of `style`, without overrides.
    #[must_use]
    pub fn new(style: Style) -> Self {
        Self {
            style,
            rules: Vec::new(),
        }
    }

    /// Adds `-z dead-reloc-in-nonalloc=` overrides, in command-line order
    /// (later rules win).
    ///
    /// # Errors
    ///
    /// Returns a [`RuleError`] for a malformed glob (an unterminated `[` or
    /// `{`, or a trailing `\`).
    pub fn with_rules<'p>(
        mut self,
        rules: impl IntoIterator<Item = (&'p [u8], u64)>,
    ) -> Result<Self, RuleError> {
        for (pattern, value) in rules {
            let glob = Glob::new(pattern).ok_or_else(|| {
                RuleError(format!(
                    "-z dead-reloc-in-nonalloc=: invalid glob pattern: {}",
                    String::from_utf8_lossy(pattern)
                ))
            })?;
            self.rules.push((glob, value));
        }
        Ok(self)
    }

    /// The tombstones for a non-allocated input section named `name`.
    #[must_use]
    pub fn for_section(&self, name: &[u8]) -> SectionTombstone {
        let user = self
            .rules
            .iter()
            .rev()
            .find(|(glob, _)| glob.matches(name))
            .map(|&(_, value)| value);
        let is_debug = name.starts_with(b".debug");
        match self.style {
            Style::Lld => {
                let builtin = is_debug.then_some(match name {
                    b".debug_loc" | b".debug_ranges" => 1,
                    b".debug_names" => u64::MAX,
                    _ => 0,
                });
                let discarded = user.or(builtin);
                let folded = if is_debug && name != b".debug_line" {
                    discarded
                } else {
                    None
                };
                SectionTombstone { discarded, folded }
            }
            Style::Gnu => {
                let builtin = if name == b".debug_ranges" { 1 } else { 0 };
                SectionTombstone {
                    discarded: Some(user.unwrap_or(builtin)),
                    folded: None,
                }
            }
        }
    }

    /// The value for one relocation: shorthand for
    /// `self.for_section(name).get(dead)`.
    #[must_use]
    pub fn value(&self, name: &[u8], dead: DeadTarget) -> Option<u64> {
        self.for_section(name).get(dead)
    }
}

/// Truncates a tombstone to a relocation field of `width` bytes, the way
/// lld writes it (`-1` becomes `0xffffffff` in a 32-bit field).
#[must_use]
pub fn truncate(value: u64, width: usize) -> u64 {
    match width {
        0 => 0,
        1..=7 => value & !u64::MAX.wrapping_shl(u32::try_from(width).unwrap_or(0).wrapping_mul(8)),
        _ => value,
    }
}

/// Parses the value of `-z dead-reloc-in-nonalloc=`: `<glob>=<value>`, the
/// glob ending at the last `=`. The value is decimal, `0x` hexadecimal or
/// `0` octal, as lld's `to_integer(0)` reads it.
#[must_use]
pub fn parse_rule(text: &str) -> Option<(&str, u64)> {
    let (glob, value) = text.rsplit_once('=')?;
    let value = if let Some(hex) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        u64::from_str_radix(hex, 16).ok()?
    } else if value.len() > 1
        && let Some(octal) = value.strip_prefix('0')
    {
        u64::from_str_radix(octal, 8).ok()?
    } else {
        value.parse().ok()?
    };
    Some((glob, value))
}

/// A glob pattern over section names, with lld's `GlobPattern` syntax: `*`,
/// `?`, `[abc]`, `[a-z]`, `[^a]`/`[!a]`, `\` escapes, and `{a,b}`
/// alternatives (not nested).
#[derive(Clone, Debug)]
pub struct Glob {
    alternatives: Vec<Vec<Token>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Token {
    Byte(u8),
    Any,
    Star,
    /// A bracket expression: the 256-bit byte set.
    Class(Box<[u64; 4]>),
}

/// Upper bound on the alternatives `{...}` groups may expand to.
const MAX_ALTERNATIVES: usize = 1024;

impl Glob {
    /// Compiles a pattern, or returns `None` if it is malformed.
    #[must_use]
    pub fn new(pattern: &[u8]) -> Option<Self> {
        let mut alternatives: Vec<Vec<Token>> = vec![Vec::new()];
        let mut i = 0usize;
        while let Some(&c) = pattern.get(i) {
            i = i.wrapping_add(1);
            match c {
                b'\\' => {
                    let &escaped = pattern.get(i)?;
                    i = i.wrapping_add(1);
                    push_all(&mut alternatives, &[Token::Byte(escaped)]);
                }
                b'*' => push_all(&mut alternatives, &[Token::Star]),
                b'?' => push_all(&mut alternatives, &[Token::Any]),
                b'[' => {
                    let (class, next) = parse_class(pattern, i)?;
                    i = next;
                    push_all(&mut alternatives, &[Token::Class(class)]);
                }
                b'{' => {
                    let close = pattern
                        .get(i..)?
                        .iter()
                        .position(|&b| b == b'}')
                        .map(|p| p.wrapping_add(i))?;
                    let body = pattern.get(i..close)?;
                    i = close.wrapping_add(1);
                    let mut expanded = Vec::new();
                    for choice in body.split(|&b| b == b',') {
                        let choice = Glob::new(choice)?;
                        for prefix in &alternatives {
                            for suffix in &choice.alternatives {
                                if expanded.len() >= MAX_ALTERNATIVES {
                                    return None;
                                }
                                let mut tokens = prefix.clone();
                                tokens.extend(suffix.iter().cloned());
                                expanded.push(tokens);
                            }
                        }
                    }
                    alternatives = expanded;
                }
                other => push_all(&mut alternatives, &[Token::Byte(other)]),
            }
        }
        Some(Self { alternatives })
    }

    /// Whether `name` matches the whole pattern.
    #[must_use]
    pub fn matches(&self, name: &[u8]) -> bool {
        self.alternatives
            .iter()
            .any(|tokens| match_tokens(tokens, name))
    }
}

fn push_all(alternatives: &mut [Vec<Token>], tokens: &[Token]) {
    for alternative in alternatives {
        alternative.extend(tokens.iter().cloned());
    }
}

/// Parses a bracket expression starting after `[`. Returns the byte set and
/// the index after `]`.
fn parse_class(pattern: &[u8], mut i: usize) -> Option<(Box<[u64; 4]>, usize)> {
    let mut set = [0u64; 4];
    let negate = matches!(pattern.get(i), Some(b'^' | b'!'));
    if negate {
        i = i.wrapping_add(1);
    }
    let mut first = true;
    loop {
        let &c = pattern.get(i)?;
        if c == b']' && !first {
            i = i.wrapping_add(1);
            break;
        }
        first = false;
        let lo = if c == b'\\' {
            i = i.wrapping_add(1);
            *pattern.get(i)?
        } else {
            c
        };
        i = i.wrapping_add(1);
        let hi = if pattern.get(i) == Some(&b'-') && pattern.get(i.wrapping_add(1)) != Some(&b']') {
            let &hi = pattern.get(i.wrapping_add(1))?;
            i = i.wrapping_add(2);
            hi
        } else {
            lo
        };
        for b in lo..=hi {
            set[usize::from(b >> 6)] |= 1 << (b & 63);
        }
    }
    if negate {
        for word in &mut set {
            *word = !*word;
        }
    }
    Some((Box::new(set), i))
}

/// Glob matching with backtracking on the last `*` (linear for patterns
/// with a single star, and never exponential).
fn match_tokens(tokens: &[Token], name: &[u8]) -> bool {
    let (mut t, mut n) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    loop {
        match (tokens.get(t), name.get(n)) {
            (Some(Token::Star), _) => {
                t = t.wrapping_add(1);
                star = Some((t, n));
            }
            (Some(token), Some(&byte))
                if match token {
                    Token::Byte(b) => *b == byte,
                    Token::Any => true,
                    Token::Class(set) => set[usize::from(byte >> 6)] & (1 << (byte & 63)) != 0,
                    Token::Star => false,
                } =>
            {
                t = t.wrapping_add(1);
                n = n.wrapping_add(1);
            }
            (None, None) => return true,
            _ => match star {
                Some((star_t, star_n)) if star_n < name.len() => {
                    let next = star_n.wrapping_add(1);
                    star = Some((star_t, next));
                    t = star_t;
                    n = next;
                }
                _ => return false,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lld_builtin_rules() {
        let t = Tombstones::default();
        let d = DeadTarget::Discarded;
        assert_eq!(t.value(b".debug_info", d), Some(0));
        assert_eq!(t.value(b".debug_line", d), Some(0));
        assert_eq!(t.value(b".debug_loc", d), Some(1));
        assert_eq!(t.value(b".debug_ranges", d), Some(1));
        assert_eq!(t.value(b".debug_rnglists", d), Some(0));
        assert_eq!(t.value(b".debug_names", d), Some(u64::MAX));
        assert_eq!(t.value(b".comment", d), None);
        assert_eq!(t.value(b".stab", d), None);
        // ICF: tombstone everywhere but .debug_line.
        assert_eq!(t.value(b".debug_info", DeadTarget::Folded), Some(0));
        assert_eq!(t.value(b".debug_ranges", DeadTarget::Folded), Some(1));
        assert_eq!(t.value(b".debug_line", DeadTarget::Folded), None);
    }

    #[test]
    fn gnu_builtin_rules() {
        let t = Tombstones::new(Style::Gnu);
        let d = DeadTarget::Discarded;
        assert_eq!(t.value(b".debug_ranges", d), Some(1));
        assert_eq!(t.value(b".debug_loc", d), Some(0));
        assert_eq!(t.value(b".debug_info", d), Some(0));
        assert_eq!(t.value(b".stab", d), Some(0));
        assert_eq!(t.value(b".debug_info", DeadTarget::Folded), None);
    }

    #[test]
    fn user_rules_override_and_last_wins() {
        let t = Tombstones::default()
            .with_rules([
                (b".debug_*".as_slice(), 42),
                (b".debug_ranges".as_slice(), 7),
                (b".my_notes".as_slice(), 9),
            ])
            .unwrap();
        let d = DeadTarget::Discarded;
        assert_eq!(t.value(b".debug_info", d), Some(42));
        assert_eq!(t.value(b".debug_loc", d), Some(42));
        assert_eq!(t.value(b".debug_ranges", d), Some(7));
        assert_eq!(t.value(b".my_notes", d), Some(9));
        // Non-debug sections never get an ICF tombstone.
        assert_eq!(t.value(b".my_notes", DeadTarget::Folded), None);
        assert!(
            Tombstones::default()
                .with_rules([(b"[abc".as_slice(), 1)])
                .is_err()
        );
    }

    #[test]
    fn globs() {
        let m = |p: &[u8], n: &[u8]| Glob::new(p).unwrap().matches(n);
        assert!(m(b".debug_*", b".debug_info"));
        assert!(m(b".debug_*", b".debug_"));
        assert!(!m(b".debug_*", b".debug"));
        assert!(m(b"*", b""));
        assert!(m(b"*info*", b".debug_info.dwo"));
        assert!(m(b".debug_?oc", b".debug_loc"));
        assert!(!m(b".debug_?oc", b".debug_lloc"));
        assert!(m(b".debug_[lr]*", b".debug_ranges"));
        assert!(!m(b".debug_[!lr]*", b".debug_ranges"));
        assert!(m(b".debug_[a-m]*", b".debug_info"));
        assert!(m(br".debug\*", b".debug*"));
        assert!(!m(br".debug\*", b".debug_info"));
        assert!(m(b".debug_{loc,ranges}", b".debug_loc"));
        assert!(m(b".debug_{loc,ranges}", b".debug_ranges"));
        assert!(!m(b".debug_{loc,ranges}", b".debug_info"));
        assert!(m(b"{.a,.b}*{x,y}", b".b123y"));
        assert!(m(b"a*b*c*d", b"aXXbYYcZZd"));
        assert!(!m(b"a*b*c*d", b"aXXbYYcZZ"));
        assert!(Glob::new(b"[").is_none());
        assert!(Glob::new(b"{a").is_none());
        assert!(Glob::new(b"a\\").is_none());
        assert!(m(b"[]]", b"]"));
    }

    #[test]
    fn rule_parsing_and_truncation() {
        assert_eq!(
            parse_rule(".debug_*=0xffffffffffffffff"),
            Some((".debug_*", u64::MAX))
        );
        assert_eq!(parse_rule(".debug_ranges=1"), Some((".debug_ranges", 1)));
        assert_eq!(parse_rule("a=b=010"), Some(("a=b", 8)));
        assert_eq!(parse_rule("x=0"), Some(("x", 0)));
        assert_eq!(parse_rule("nothing"), None);
        assert_eq!(parse_rule("x=zz"), None);
        assert_eq!(truncate(u64::MAX, 4), 0xffff_ffff);
        assert_eq!(truncate(u64::MAX, 8), u64::MAX);
        assert_eq!(truncate(0x1234, 1), 0x34);
    }
}
