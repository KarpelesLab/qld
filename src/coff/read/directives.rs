//! `.drectve` linker directives.
//!
//! A `.drectve` section holds command-line options for the linker, such as
//! ` -export:"foo",data -aligncomm:"bar",3` (GCC) or
//! `/DEFAULTLIB:"LIBCMT" /EXPORT:foo` (MSVC). qld tokenizes it the way
//! `link.exe` and lld do, with the Windows command-line quoting rules:
//!
//! - tokens are separated by spaces, tabs, CR, LF and NUL bytes outside
//!   double quotes;
//! - `"` toggles quoting; inside quotes, `""` is a literal quote;
//! - `2n` backslashes before a `"` produce `n` backslashes and the quote
//!   toggles; `2n + 1` backslashes produce `n` backslashes and a literal
//!   quote; backslashes not before a quote are literal.
//!
//! A leading UTF-8 byte order mark is skipped. Option names are matched
//! without regard to case, with either a `-` or `/` prefix.
//!
//! Values are split at `:`, `,` and `=` *outside* quotes, then each part is
//! unquoted. lld unquotes the whole token first, so the two differ only for
//! a separator inside quotes (`-export:"a,b"`), where qld follows GNU ld and
//! keeps the quoted text as one name. Parts are borrowed from the section
//! when unquoting does not change them or only strips surrounding quotes,
//! which covers what compilers emit.

use std::borrow::Cow;

use super::export::ExportSpec;
use super::source::{Source, to_u64};
use crate::error::Result;

/// Name of the section holding linker directives.
pub const DRECTVE_SECTION: &[u8] = b".drectve";

/// One whitespace-separated token of a directive string, still quoted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Token<'a> {
    /// The token as written, quotes and backslashes included.
    pub raw: &'a [u8],
    /// Offset of the token in the directive string.
    pub offset: usize,
}

impl<'a> Token<'a> {
    /// The token with the Windows quoting rules applied.
    #[must_use]
    pub fn text(&self) -> Cow<'a, [u8]> {
        unquote(self.raw)
    }
}

/// Splits a directive string into [`Token`]s.
#[must_use]
pub fn tokenize(data: &[u8]) -> Tokens<'_> {
    let pos = if data.starts_with(b"\xef\xbb\xbf") {
        3
    } else {
        0
    };
    Tokens { data, pos }
}

/// Iterator over the tokens of a directive string; see [`tokenize`].
#[derive(Clone, Debug)]
pub struct Tokens<'a> {
    data: &'a [u8],
    pos: usize,
}

fn is_separator(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\r' | b'\n' | 0)
}

impl<'a> Iterator for Tokens<'a> {
    type Item = Token<'a>;

    fn next(&mut self) -> Option<Token<'a>> {
        let data = self.data;
        let rest = data.get(self.pos..)?;
        let skip = rest.iter().position(|&c| !is_separator(c))?;
        let start = self.pos.checked_add(skip)?;
        let body = data.get(start..)?;
        let len = scan_until(body, is_separator);
        let end = start.checked_add(len)?;
        self.pos = end;
        Some(Token {
            raw: data.get(start..end)?,
            offset: start,
        })
    }
}

/// Length of the prefix of `raw` before the first byte matching `stop`
/// outside quotes.
fn scan_until(raw: &[u8], stop: impl Fn(u8) -> bool) -> usize {
    let mut quoted = false;
    let mut backslashes = 0usize;
    for (i, &c) in raw.iter().enumerate() {
        match c {
            b'\\' => {
                backslashes = backslashes.wrapping_add(1);
                continue;
            }
            b'"' => {
                // An odd run of backslashes escapes the quote. Inside
                // quotes, `""` toggles twice, which leaves the state as a
                // literal quote would.
                if backslashes.is_multiple_of(2) {
                    quoted = !quoted;
                }
            }
            _ if !quoted && stop(c) => return i,
            _ => {}
        }
        backslashes = 0;
    }
    raw.len()
}

/// Splits `raw` at the first `separator` outside quotes.
fn split_raw(raw: &[u8], separator: u8) -> (&[u8], Option<&[u8]>) {
    let at = scan_until(raw, |c| c == separator);
    match (raw.get(..at), raw.get(at.saturating_add(1)..)) {
        (Some(before), Some(after)) if at < raw.len() => (before, Some(after)),
        _ => (raw, None),
    }
}

/// Applies the Windows quoting rules to one token (or part of one).
#[must_use]
pub fn unquote(raw: &[u8]) -> Cow<'_, [u8]> {
    if !raw.contains(&b'"') {
        return Cow::Borrowed(raw);
    }
    if let Some(inner) = raw.strip_prefix(b"\"").and_then(|r| r.strip_suffix(b"\""))
        && !inner.iter().any(|&c| c == b'"' || c == b'\\')
    {
        return Cow::Borrowed(inner);
    }
    let mut out = Vec::with_capacity(raw.len());
    let mut quoted = false;
    let mut i = 0usize;
    while let Some(&c) = raw.get(i) {
        match c {
            b'\\' => {
                let run = raw
                    .get(i..)
                    .map_or(0, |r| r.iter().take_while(|&&b| b == b'\\').count());
                let after = i.saturating_add(run);
                if raw.get(after) == Some(&b'"') {
                    out.extend(std::iter::repeat_n(b'\\', run / 2));
                    if run % 2 == 1 {
                        out.push(b'"');
                        i = after.saturating_add(1);
                    } else {
                        i = after;
                    }
                } else {
                    out.extend(std::iter::repeat_n(b'\\', run));
                    i = after;
                }
            }
            b'"' => {
                if quoted && raw.get(i.saturating_add(1)) == Some(&b'"') {
                    out.push(b'"');
                    i = i.saturating_add(2);
                } else {
                    quoted = !quoted;
                    i = i.saturating_add(1);
                }
            }
            _ => {
                out.push(c);
                i = i.saturating_add(1);
            }
        }
    }
    Cow::Owned(out)
}

/// A parsed linker directive.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Directive<'a> {
    /// `-export:name[=internal][,@ordinal[,NONAME]][,DATA][,PRIVATE]`
    /// `[,CONSTANT][,EXPORTAS,name]`.
    Export(ExportSpec<'a>),
    /// `-include:symbol`: force `symbol` to be undefined (a GC root).
    Include(Cow<'a, [u8]>),
    /// `-includeoptional:symbol`: like `-include:`, but only if the symbol
    /// is defined somewhere.
    IncludeOptional(Cow<'a, [u8]>),
    /// `-exclude-symbols:a,b,...`: symbols excluded from
    /// `--export-all-symbols`. The list is kept as written; see
    /// [`split_list`].
    ExcludeSymbols(Cow<'a, [u8]>),
    /// `-defaultlib:name`.
    DefaultLib(Cow<'a, [u8]>),
    /// `-nodefaultlib[:name]`.
    NoDefaultLib(Option<Cow<'a, [u8]>>),
    /// `-alternatename:alias=target`: `alias` resolves to `target` if it is
    /// otherwise undefined.
    AlternateName {
        /// The name that may be undefined.
        alias: Cow<'a, [u8]>,
        /// The name it falls back to.
        target: Cow<'a, [u8]>,
    },
    /// `-aligncomm:symbol,log2`: minimum alignment of a common symbol.
    AlignComm {
        /// The common symbol.
        symbol: Cow<'a, [u8]>,
        /// Log2 of the alignment in bytes.
        alignment_log2: u32,
    },
    /// `-entry:symbol`.
    Entry(Cow<'a, [u8]>),
    /// `-subsystem:name[,major[.minor]]`, kept as written.
    Subsystem(Cow<'a, [u8]>),
    /// `-stack:reserve[,commit]`.
    Stack {
        /// Reserve size.
        reserve: u64,
        /// Commit size, if given.
        commit: Option<u64>,
    },
    /// `-heap:reserve[,commit]`.
    Heap {
        /// Reserve size.
        reserve: u64,
        /// Commit size, if given.
        commit: Option<u64>,
    },
    /// `-merge:from=to`.
    Merge {
        /// Section merged away.
        from: Cow<'a, [u8]>,
        /// Section it is merged into.
        to: Cow<'a, [u8]>,
    },
    /// `-section:name,attributes`.
    Section {
        /// Section name.
        name: Cow<'a, [u8]>,
        /// Attribute letters (`[!]{DEKPRSW}`), kept as written.
        attributes: Cow<'a, [u8]>,
    },
    /// `-failifmismatch:key=value`.
    FailIfMismatch {
        /// The key.
        key: Cow<'a, [u8]>,
        /// The value that all objects must agree on.
        value: Cow<'a, [u8]>,
    },
    /// `-manifestdependency:text`.
    ManifestDependency(Cow<'a, [u8]>),
    /// `-release`: set the image checksum.
    Release,
    /// Another option (`-guardsym:`, `-throwingnew:`, `-attr:`, …): name
    /// without its prefix, and the value after `:`, if any.
    Other {
        /// Option name, without the `-` or `/` prefix.
        name: Cow<'a, [u8]>,
        /// The value after the first `:`.
        value: Option<Cow<'a, [u8]>>,
    },
    /// A token without a `-` or `/` prefix. lld rejects these.
    Unprefixed(Cow<'a, [u8]>),
}

/// Splits a comma-separated list (as in `-exclude-symbols:`), skipping
/// empty entries.
pub fn split_list<'s>(list: &'s [u8]) -> impl Iterator<Item = &'s [u8]> + 's {
    list.split(|&c| c == b',').filter(|item| !item.is_empty())
}

/// Parses every directive in a `.drectve` section.
///
/// `file_offset` is the file offset of the section contents, used in error
/// messages.
#[must_use]
pub fn parse_directives<'a>(
    data: &'a [u8],
    file_offset: u64,
    source: Source<'a>,
) -> Directives<'a> {
    Directives {
        tokens: tokenize(data),
        file_offset,
        source,
    }
}

/// Iterator over the directives of a `.drectve` section; see
/// [`parse_directives`]. A directive with an invalid value yields an error,
/// and parsing continues with the next one.
#[derive(Clone, Debug)]
pub struct Directives<'a> {
    tokens: Tokens<'a>,
    file_offset: u64,
    source: Source<'a>,
}

impl<'a> Iterator for Directives<'a> {
    type Item = Result<Directive<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        let token = self.tokens.next()?;
        Some(parse_token(token.raw).map_err(|what| {
            self.source.malformed(
                self.file_offset.saturating_add(to_u64(token.offset)),
                format!(
                    ".drectve directive `{}` ({what})",
                    String::from_utf8_lossy(token.raw)
                ),
            )
        }))
    }
}

/// Parses one directive token.
///
/// # Errors
///
/// Returns a description of the problem if a known directive has a missing
/// or invalid value.
pub fn parse_token(raw: &[u8]) -> Result<Directive<'_>, &'static str> {
    let Some(body) = raw.strip_prefix(b"-").or_else(|| raw.strip_prefix(b"/")) else {
        return Ok(Directive::Unprefixed(unquote(raw)));
    };
    let (name_raw, value_raw) = split_raw(body, b':');
    let name = unquote(name_raw);
    let required = || value_raw.ok_or("missing value");
    let is = |option: &str| name.eq_ignore_ascii_case(option.as_bytes());

    let directive = if is("export") {
        Directive::Export(parse_export(required()?)?)
    } else if is("include") {
        Directive::Include(non_empty(required()?)?)
    } else if is("includeoptional") {
        Directive::IncludeOptional(non_empty(required()?)?)
    } else if is("exclude-symbols") {
        Directive::ExcludeSymbols(unquote(required()?))
    } else if is("defaultlib") {
        Directive::DefaultLib(non_empty(required()?)?)
    } else if is("nodefaultlib") {
        Directive::NoDefaultLib(value_raw.map(unquote))
    } else if is("alternatename") {
        let (alias, target) = pair(required()?, b'=')?;
        Directive::AlternateName { alias, target }
    } else if is("aligncomm") {
        let (symbol, log2) = pair(required()?, b',')?;
        let alignment_log2 = parse_integer(&log2)
            .and_then(|v| u32::try_from(v).ok())
            .filter(|&v| v < 32)
            .ok_or("invalid alignment")?;
        Directive::AlignComm {
            symbol,
            alignment_log2,
        }
    } else if is("entry") {
        Directive::Entry(non_empty(required()?)?)
    } else if is("subsystem") {
        Directive::Subsystem(non_empty(required()?)?)
    } else if is("stack") || is("heap") {
        let (reserve, commit) = split_raw(required()?, b',');
        let reserve = parse_integer(&unquote(reserve)).ok_or("invalid size")?;
        let commit = match commit {
            Some(commit) => Some(parse_integer(&unquote(commit)).ok_or("invalid size")?),
            None => None,
        };
        if is("stack") {
            Directive::Stack { reserve, commit }
        } else {
            Directive::Heap { reserve, commit }
        }
    } else if is("merge") {
        let (from, to) = pair(required()?, b'=')?;
        Directive::Merge { from, to }
    } else if is("section") {
        let (name, attributes) = pair(required()?, b',')?;
        Directive::Section { name, attributes }
    } else if is("failifmismatch") {
        let (key, value) = pair(required()?, b'=')?;
        Directive::FailIfMismatch { key, value }
    } else if is("manifestdependency") {
        Directive::ManifestDependency(unquote(required()?))
    } else if is("release") {
        Directive::Release
    } else {
        Directive::Other {
            name,
            value: value_raw.map(unquote),
        }
    };
    Ok(directive)
}

fn non_empty(raw: &[u8]) -> Result<Cow<'_, [u8]>, &'static str> {
    let text = unquote(raw);
    if text.is_empty() {
        return Err("empty value");
    }
    Ok(text)
}

/// The two parts of a `key=value` style value.
type Pair<'a> = (Cow<'a, [u8]>, Cow<'a, [u8]>);

fn pair(raw: &[u8], separator: u8) -> Result<Pair<'_>, &'static str> {
    let (first, second) = split_raw(raw, separator);
    let second = second.ok_or("missing separator")?;
    Ok((non_empty(first)?, non_empty(second)?))
}

/// Parses an `-export:` value, following lld's `parseExport`.
///
/// # Errors
///
/// Returns a description of the problem for an empty name, an invalid
/// ordinal, `NONAME` without an ordinal, a misplaced `EXPORTAS` or an
/// unknown flag.
pub fn parse_export(raw: &[u8]) -> Result<ExportSpec<'_>, &'static str> {
    let (first, mut rest) = split_raw(raw, b',');
    let (name, internal) = split_raw(first, b'=');
    let mut spec = ExportSpec {
        name: non_empty(name)?,
        internal_name: internal.map(non_empty).transpose()?,
        ..ExportSpec::default()
    };
    while let Some(part_raw) = rest {
        let (part, next) = split_raw(part_raw, b',');
        rest = next;
        let part = unquote(part);
        if part.eq_ignore_ascii_case(b"noname") {
            if spec.ordinal.is_none() {
                return Err("NONAME without an ordinal");
            }
            spec.noname = true;
        } else if part.eq_ignore_ascii_case(b"data") {
            spec.data = true;
        } else if part.eq_ignore_ascii_case(b"constant") {
            spec.constant = true;
        } else if part.eq_ignore_ascii_case(b"private") {
            spec.private = true;
        } else if part.eq_ignore_ascii_case(b"exportas") {
            let value = rest.ok_or("EXPORTAS without a name")?;
            if split_raw(value, b',').1.is_some() {
                return Err("EXPORTAS must be last");
            }
            spec.export_as = Some(non_empty(value)?);
            break;
        } else if let Some(ordinal) = part.strip_prefix(b"@") {
            let ordinal = parse_integer(ordinal)
                .and_then(|v| u16::try_from(v).ok())
                .filter(|&v| v != 0)
                .ok_or("invalid ordinal")?;
            spec.ordinal = Some(ordinal);
        } else {
            return Err("unknown export flag");
        }
    }
    Ok(spec)
}

/// Parses an integer with LLVM's automatic radix: `0x` hexadecimal, `0b`
/// binary, `0o` or a leading `0` octal, decimal otherwise.
#[must_use]
pub fn parse_integer(text: &[u8]) -> Option<u64> {
    let (digits, radix) = if let Some(hex) = text
        .strip_prefix(b"0x")
        .or_else(|| text.strip_prefix(b"0X"))
    {
        (hex, 16)
    } else if let Some(bin) = text
        .strip_prefix(b"0b")
        .or_else(|| text.strip_prefix(b"0B"))
    {
        (bin, 2)
    } else if let Some(oct) = text
        .strip_prefix(b"0o")
        .or_else(|| text.strip_prefix(b"0O"))
    {
        (oct, 8)
    } else if text.len() > 1
        && let Some(oct) = text.strip_prefix(b"0")
    {
        (oct, 8)
    } else {
        (text, 10)
    };
    parse_radix(digits, radix)
}

/// Parses digits in `radix` (up to 16), with no sign or prefix.
pub(crate) fn parse_radix(digits: &[u8], radix: u32) -> Option<u64> {
    if digits.is_empty() {
        return None;
    }
    let mut value: u64 = 0;
    for &c in digits {
        let digit = char::from(c).to_digit(radix)?;
        value = value
            .checked_mul(u64::from(radix))?
            .checked_add(u64::from(digit))?;
    }
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(s: &[u8]) -> Vec<Vec<u8>> {
        tokenize(s).map(|t| t.text().into_owned()).collect()
    }

    #[test]
    fn windows_tokenization() {
        assert_eq!(
            tokens(b" -export:\"foo\",data\0\0-a  b\t\"c d\""),
            [&b"-export:foo,data"[..], b"-a", b"b", b"c d"]
        );
        // Backslashes before quotes.
        assert_eq!(tokens(br#"a\\"b c"d"#), [&br#"a\b cd"#[..]]);
        assert_eq!(tokens(br#"a\"b c"#), [&br#"a"b"#[..], b"c"]);
        assert_eq!(tokens(br#"a\\\"b"#), [&br#"a\"b"#[..]]);
        assert_eq!(tokens(br#"a\b"#), [&br#"a\b"#[..]]);
        // Doubled quotes inside quotes.
        assert_eq!(tokens(br#""a""b" c"#), [&br#"a"b"#[..], b"c"]);
        assert_eq!(tokens(b"\xef\xbb\xbf/x"), [&b"/x"[..]]);
        assert!(tokens(b"").is_empty());
        assert!(tokens(b" \0 ").is_empty());
    }

    #[test]
    fn borrowed_when_possible() {
        assert!(matches!(unquote(b"foo"), Cow::Borrowed(b"foo")));
        assert!(matches!(unquote(b"\"foo\""), Cow::Borrowed(b"foo")));
        assert!(matches!(unquote(b"a\"b\""), Cow::Owned(_)));
    }

    #[test]
    fn directives() {
        let parse = |s: &'static [u8]| parse_token(s).unwrap();
        assert_eq!(
            parse(b"-export:\"exported_data\",data"),
            Directive::Export(ExportSpec {
                name: Cow::Borrowed(b"exported_data"),
                data: true,
                ..ExportSpec::default()
            })
        );
        let Directive::Export(spec) = parse(b"/EXPORT:foo=bar,@5,NONAME,PRIVATE") else {
            panic!()
        };
        assert_eq!(&*spec.name, b"foo");
        assert_eq!(spec.internal_name.as_deref(), Some(&b"bar"[..]));
        assert_eq!(spec.ordinal, Some(5));
        assert!(spec.noname && spec.private && !spec.data);
        let Directive::Export(spec) = parse(b"-export:f,EXPORTAS,g") else {
            panic!()
        };
        assert_eq!(spec.export_as.as_deref(), Some(&b"g"[..]));
        assert_eq!(
            parse(b"-aligncomm:\"common_var\",2"),
            Directive::AlignComm {
                symbol: Cow::Borrowed(b"common_var"),
                alignment_log2: 2
            }
        );
        assert_eq!(
            parse(b"/alternatename:__a=__b"),
            Directive::AlternateName {
                alias: Cow::Borrowed(b"__a"),
                target: Cow::Borrowed(b"__b")
            }
        );
        assert_eq!(
            parse(b"/DEFAULTLIB:\"LIBCMT\""),
            Directive::DefaultLib(Cow::Borrowed(b"LIBCMT"))
        );
        assert_eq!(parse(b"/NODEFAULTLIB"), Directive::NoDefaultLib(None));
        assert_eq!(
            parse(b"-stack:0x100000,4096"),
            Directive::Stack {
                reserve: 0x10_0000,
                commit: Some(4096)
            }
        );
        assert_eq!(
            parse(b"-include:foo"),
            Directive::Include(Cow::Borrowed(b"foo"))
        );
        assert_eq!(
            parse(b"/FAILIFMISMATCH:_MSC_VER=1900"),
            Directive::FailIfMismatch {
                key: Cow::Borrowed(b"_MSC_VER"),
                value: Cow::Borrowed(b"1900")
            }
        );
        assert_eq!(
            parse(b"/guardsym:x"),
            Directive::Other {
                name: Cow::Borrowed(b"guardsym"),
                value: Some(Cow::Borrowed(b"x"))
            }
        );
        assert_eq!(
            parse(b"foo.lib"),
            Directive::Unprefixed(Cow::Borrowed(b"foo.lib"))
        );
        for bad in [
            &b"-export:"[..],
            b"-export:foo,NONAME",
            b"-export:foo,@0",
            b"-export:foo,@65536",
            b"-export:foo,bogus",
            b"-export:foo,EXPORTAS",
            b"-export:foo,EXPORTAS,a,b",
            b"-aligncomm:foo",
            b"-aligncomm:foo,x",
            b"-include",
            b"-alternatename:a",
            b"-stack:x",
        ] {
            assert!(
                parse_token(bad).is_err(),
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
    }

    #[test]
    fn integers() {
        assert_eq!(parse_integer(b"10"), Some(10));
        assert_eq!(parse_integer(b"0x10"), Some(16));
        assert_eq!(parse_integer(b"010"), Some(8));
        assert_eq!(parse_integer(b"0"), Some(0));
        assert_eq!(parse_integer(b"0b11"), Some(3));
        assert_eq!(parse_integer(b""), None);
        assert_eq!(parse_integer(b"0x"), None);
        assert_eq!(parse_integer(b"99999999999999999999"), None);
    }
}
