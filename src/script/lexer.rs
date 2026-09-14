//! Context-sensitive tokenizer for GNU linker scripts.
//!
//! GNU ld's lexer has several start states, and the same characters form
//! different tokens in each: `*(.text)` is a file pattern and a section
//! pattern inside an output section description, `a-b` is one file name at
//! the top level but a subtraction in an expression, and `/DISCARD/` would be
//! a division anywhere else. The parser therefore asks for each token in a
//! [`Mode`] matching the GNU start state for that position.
//!
//! Tokenization follows GNU ld's rules exactly, including their surprises:
//! at the top level `foo=1` is a single name, `INPUT(a.o, b.o)` names a file
//! `a.o,`, and in an expression `each` is the hexadecimal number `0xeac`
//! (`h` suffix). Matching GNU keeps every script GNU accepts meaning the same
//! thing. Keywords are not recognized here; the parser checks name tokens
//! against the keywords valid at each position.
//!
//! Unlike bison, the parser does not keep a lookahead token lexed in the wrong
//! state: asking for a token in a different mode re-lexes it from the same
//! position. GNU gets the same effect in the places that matter by throwing
//! lookahead names away (`ldlex_backup`).

#![deny(clippy::arithmetic_side_effects)]

use std::borrow::Cow;

/// Lexer start state, named after GNU ld's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    /// `SCRIPT`: top level, `SECTIONS`, `MEMORY`, `PHDRS` names.
    Script,
    /// `EXPRESSION`: arithmetic.
    Expr,
    /// `WILD`: statements inside an output section description.
    Wild,
    /// `INPUTLIST`: the file list of `INPUT`, `GROUP` and `AS_NEEDED`.
    InputList,
    /// `VERS_SCRIPT`: version node names and dependencies.
    VersionScript,
    /// `VERS_NODE`: symbol patterns inside a version node.
    VersionNode,
}

/// Punctuation and operator tokens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Punct {
    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    Semi,
    Comma,
    Colon,
    Assign,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Lt,
    Gt,
    Bang,
    Tilde,
    Question,
    Amp,
    Pipe,
    Caret,
    Shl,
    Shr,
    EqEq,
    Ne,
    Le,
    Ge,
    AndAnd,
    OrOr,
    AddAssign,
    SubAssign,
    MulAssign,
    DivAssign,
    ShlAssign,
    ShrAssign,
    AndAssign,
    OrAssign,
    XorAssign,
}

impl Punct {
    /// The source spelling.
    pub(crate) fn text(self) -> &'static str {
        match self {
            Self::LParen => "(",
            Self::RParen => ")",
            Self::LBrace => "{",
            Self::RBrace => "}",
            Self::LBracket => "[",
            Self::RBracket => "]",
            Self::Semi => ";",
            Self::Comma => ",",
            Self::Colon => ":",
            Self::Assign => "=",
            Self::Plus => "+",
            Self::Minus => "-",
            Self::Star => "*",
            Self::Slash => "/",
            Self::Percent => "%",
            Self::Lt => "<",
            Self::Gt => ">",
            Self::Bang => "!",
            Self::Tilde => "~",
            Self::Question => "?",
            Self::Amp => "&",
            Self::Pipe => "|",
            Self::Caret => "^",
            Self::Shl => "<<",
            Self::Shr => ">>",
            Self::EqEq => "==",
            Self::Ne => "!=",
            Self::Le => "<=",
            Self::Ge => ">=",
            Self::AndAnd => "&&",
            Self::OrOr => "||",
            Self::AddAssign => "+=",
            Self::SubAssign => "-=",
            Self::MulAssign => "*=",
            Self::DivAssign => "/=",
            Self::ShlAssign => "<<=",
            Self::ShrAssign => ">>=",
            Self::AndAssign => "&=",
            Self::OrAssign => "|=",
            Self::XorAssign => "^=",
        }
    }
}

// Modes in which each punctuation token exists, as bit sets.
const S: u8 = 1;
const E: u8 = 2;
const W: u8 = 4;
const I: u8 = 8;
const V: u8 = 16;
const N: u8 = 32;

fn mode_bit(mode: Mode) -> u8 {
    match mode {
        Mode::Script => S,
        Mode::Expr => E,
        Mode::Wild => W,
        Mode::InputList => I,
        Mode::VersionScript => V,
        Mode::VersionNode => N,
    }
}

/// Punctuation table, longest spellings first so the first hit is the longest.
const PUNCTS: &[(&[u8], Punct, u8)] = &[
    (b"<<=", Punct::ShlAssign, S | E | W),
    (b">>=", Punct::ShrAssign, S | E | W),
    (b"||", Punct::OrOr, E),
    (b"==", Punct::EqEq, E),
    (b"!=", Punct::Ne, E),
    (b">=", Punct::Ge, E),
    (b"<=", Punct::Le, E),
    (b"<<", Punct::Shl, E),
    (b">>", Punct::Shr, E),
    (b"+=", Punct::AddAssign, S | E | W),
    (b"-=", Punct::SubAssign, S | E | W),
    (b"*=", Punct::MulAssign, S | E | W),
    (b"/=", Punct::DivAssign, S | E | W),
    (b"&=", Punct::AndAssign, S | E | W),
    (b"|=", Punct::OrAssign, S | E | W),
    (b"^=", Punct::XorAssign, S | E | W),
    (b"&&", Punct::AndAnd, E),
    (b"]", Punct::RBracket, W),
    (b"[", Punct::LBracket, W),
    (b">", Punct::Gt, S | E),
    (b",", Punct::Comma, S | E | I | V | N),
    (b"&", Punct::Amp, E | W),
    (b"|", Punct::Pipe, E),
    (b"~", Punct::Tilde, S | E),
    (b"!", Punct::Bang, S | E),
    (b"?", Punct::Question, E),
    (b"*", Punct::Star, E),
    (b"+", Punct::Plus, S | E),
    (b"-", Punct::Minus, S | E),
    (b"/", Punct::Slash, E),
    (b"%", Punct::Percent, E),
    (b"<", Punct::Lt, E),
    (b"^", Punct::Caret, E),
    (b"=", Punct::Assign, S | E | W),
    (b"}", Punct::RBrace, S | E | W | V | N),
    (b"{", Punct::LBrace, S | E | W | V | N),
    (b")", Punct::RParen, S | E | W | I),
    (b"(", Punct::LParen, S | E | W | I),
    (b":", Punct::Colon, S | E | V | N),
    (b";", Punct::Semi, S | E | W | V | N),
];

/// What kind of token was read. The text is found through the token's byte
/// range in the source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TokenKind {
    /// An unquoted name, file name, pattern or keyword.
    Name,
    /// A `"quoted"` string; the range includes the quotes.
    Quoted,
    /// `-lname` in an input list; the range includes `-l`.
    LibName,
    /// An integer literal. `hex_digits` is set for a `0x` literal without a
    /// `K`/`M` suffix, whose digits then start two bytes into the range; GNU
    /// uses them for fill patterns wider than eight bytes.
    Int { value: u64, hex_digits: bool },
    /// Punctuation or an operator.
    Punct(Punct),
    /// End of input.
    Eof,
}

/// One token with its byte range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Token {
    pub(crate) kind: TokenKind,
    pub(crate) start: usize,
    pub(crate) end: usize,
}

/// A lexing failure at a byte offset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LexError {
    pub(crate) offset: usize,
    pub(crate) message: String,
}

/// Tokenizer over one script file.
pub(crate) struct Lexer<'s> {
    src: Cow<'s, [u8]>,
    pos: usize,
    /// Byte offsets at which each line starts, for line/column lookup.
    line_starts: Vec<usize>,
}

fn is_alpha(b: u8) -> bool {
    b.is_ascii_alphabetic()
}

fn is_filename_char1(b: u8) -> bool {
    is_alpha(b) || matches!(b, b'_' | b'/' | b'.' | b'\\' | b'$' | b'~')
}

fn is_filename_char(b: u8) -> bool {
    is_filename_char1(b)
        || b.is_ascii_digit()
        || matches!(b, b'-' | b'+' | b':' | b'[' | b']' | b',' | b'=')
}

fn is_symbol_char1(b: u8) -> bool {
    is_alpha(b) || matches!(b, b'_' | b'.' | b'\\' | b'$')
}

fn is_symbol_char(b: u8) -> bool {
    is_alpha(b) || b.is_ascii_digit() || matches!(b, b'_' | b'/' | b'.' | b'\\' | b'$' | b'~')
}

fn is_wild_char(b: u8) -> bool {
    is_filename_char(b) || matches!(b, b'?' | b'*' | b'^' | b'!')
}

fn is_vers_tag1(b: u8) -> bool {
    is_alpha(b) || matches!(b, b'.' | b'$' | b'_')
}

fn is_vers_tag(b: u8) -> bool {
    is_alpha(b) || b.is_ascii_digit() || matches!(b, b'.' | b'_')
}

fn is_vers_ident1(b: u8) -> bool {
    is_alpha(b)
        || matches!(
            b,
            b'*' | b'?' | b'.' | b'$' | b'_' | b'[' | b']' | b'-' | b'!' | b'^' | b'\\'
        )
}

fn is_vers_ident(b: u8) -> bool {
    is_vers_ident1(b) || b.is_ascii_digit()
}

/// Length of the run of bytes at the start of `s` satisfying `pred`.
fn run(s: &[u8], pred: impl Fn(u8) -> bool) -> usize {
    s.iter().position(|&b| !pred(b)).unwrap_or(s.len())
}

/// `strtoull`-style conversion: digits valid in `base` are accumulated until
/// the first invalid one; overflow saturates, as `strtoull` returns
/// `ULLONG_MAX` with `ERANGE`, which ld ignores.
fn convert(digits: &[u8], base: u32) -> u64 {
    let mut value: u64 = 0;
    let mut overflow = false;
    for &b in digits {
        let Some(digit) = (b as char).to_digit(base) else {
            break;
        };
        match value
            .checked_mul(u64::from(base))
            .and_then(|v| v.checked_add(u64::from(digit)))
        {
            Some(v) => value = v,
            None => overflow = true,
        }
    }
    if overflow { u64::MAX } else { value }
}

/// `strtoull(s, NULL, 0)`: `0x` prefix for hex, leading `0` for octal.
fn convert_auto(s: &[u8]) -> u64 {
    match s {
        [b'0', b'x' | b'X', rest @ ..] if rest.first().is_some_and(u8::is_ascii_hexdigit) => {
            convert(rest, 16)
        }
        [b'0', ..] => convert(s, 8),
        _ => convert(s, 10),
    }
}

/// `strtoull(s, NULL, 16)`, which also skips an optional `0x` prefix.
fn convert_hex(s: &[u8]) -> u64 {
    match s {
        [b'0', b'x' | b'X', rest @ ..] if rest.first().is_some_and(u8::is_ascii_hexdigit) => {
            convert(rest, 16)
        }
        _ => convert(s, 16),
    }
}

/// A number candidate: length, value, and whether hex digits are kept.
type NumberMatch = (usize, u64, bool);

/// The `$hex` rule (expression state only).
fn number_dollar(s: &[u8]) -> Option<NumberMatch> {
    if s.first() != Some(&b'$') {
        return None;
    }
    let rest = s.get(1..).unwrap_or_default();
    let digits = run(rest, |b| b.is_ascii_hexdigit());
    if digits == 0 {
        return None;
    }
    let len = digits.checked_add(1)?;
    Some((len, convert(rest.get(..digits)?, 16), false))
}

/// A digit class, its suffix letters, and its base.
type DigitClass = (fn(u8) -> bool, &'static [u8], u32);

/// The suffixed rules: `hexH`, `hexX`, `binB`, `octO`, `decD` (expression
/// state only). Returns the longest.
fn number_suffixed(s: &[u8]) -> Option<NumberMatch> {
    let mut best: Option<NumberMatch> = None;
    let classes: [DigitClass; 4] = [
        (|b| b.is_ascii_hexdigit(), b"HhXx", 16),
        (|b| matches!(b, b'0' | b'1'), b"Bb", 2),
        (|b| matches!(b, b'0'..=b'7'), b"Oo", 8),
        (|b| b.is_ascii_digit(), b"Dd", 10),
    ];
    for (class, suffixes, base) in classes {
        // No suffix letter belongs to its own digit class, so the digit run
        // cannot swallow the suffix.
        let digits = run(s, class);
        if digits == 0 {
            continue;
        }
        if let Some(&suffix) = s.get(digits)
            && suffixes.contains(&suffix)
        {
            let len = digits.checked_add(1)?;
            if best.is_none_or(|(l, _, _)| len > l) {
                let text = s.get(..len)?;
                let value = if base == 16 {
                    convert_hex(text)
                } else {
                    convert(text, base)
                };
                best = Some((len, value, false));
            }
        }
    }
    best
}

/// The plain rule, `(($|0x)hex+|dec+)(K|M)?` (script and expression states).
fn number_plain(s: &[u8]) -> Option<NumberMatch> {
    let (body, value, hex) = match s {
        [b'$', rest @ ..] => {
            let digits = run(rest, |b| b.is_ascii_hexdigit());
            if digits == 0 {
                return None;
            }
            (digits.checked_add(1)?, convert(rest, 16), false)
        }
        [b'0', b'x' | b'X', rest @ ..] if rest.first().is_some_and(u8::is_ascii_hexdigit) => {
            let digits = run(rest, |b| b.is_ascii_hexdigit());
            (digits.checked_add(2)?, convert(rest, 16), true)
        }
        [first, ..] if first.is_ascii_digit() => {
            let digits = run(s, |b| b.is_ascii_digit());
            (digits, convert_auto(s.get(..digits)?), false)
        }
        _ => return None,
    };
    match s.get(body) {
        Some(b'K' | b'k') => Some((body.checked_add(1)?, value.wrapping_mul(1024), false)),
        Some(b'M' | b'm') => Some((body.checked_add(1)?, value.wrapping_mul(1024 * 1024), false)),
        _ => Some((body, value, hex)),
    }
}

impl<'s> Lexer<'s> {
    /// Creates a lexer over `src`.
    pub(crate) fn new(src: Cow<'s, [u8]>) -> Self {
        let mut line_starts = vec![0];
        line_starts.extend(
            src.iter()
                .enumerate()
                .filter(|&(_, &b)| b == b'\n')
                .filter_map(|(i, _)| i.checked_add(1)),
        );
        Self {
            src,
            pos: 0,
            line_starts,
        }
    }

    /// The current position, to restore with [`Lexer::restore`].
    pub(crate) fn save(&self) -> usize {
        self.pos
    }

    /// Rewinds to a position returned by [`Lexer::save`] or a token start.
    pub(crate) fn restore(&mut self, pos: usize) {
        self.pos = pos.min(self.src.len());
    }

    /// The bytes of a token's range.
    pub(crate) fn slice(&self, start: usize, end: usize) -> &[u8] {
        self.src.get(start..end).unwrap_or_default()
    }

    /// The text a token stands for: the name, the quoted contents, or the
    /// library name after `-l`.
    pub(crate) fn text(&self, token: &Token) -> &[u8] {
        match token.kind {
            TokenKind::Quoted => self.slice(
                token.start.saturating_add(1),
                token
                    .end
                    .saturating_sub(1)
                    .max(token.start.saturating_add(1)),
            ),
            TokenKind::LibName => self.slice(token.start.saturating_add(2), token.end),
            _ => self.slice(token.start, token.end),
        }
    }

    /// 1-based line and column of a byte offset.
    pub(crate) fn line_col(&self, offset: usize) -> (u32, u32) {
        let line_index = match self.line_starts.binary_search(&offset) {
            Ok(i) => i,
            Err(i) => i.saturating_sub(1),
        };
        let start = self.line_starts.get(line_index).copied().unwrap_or(0);
        let line = u32::try_from(line_index.saturating_add(1)).unwrap_or(u32::MAX);
        let column =
            u32::try_from(offset.saturating_sub(start).saturating_add(1)).unwrap_or(u32::MAX);
        (line, column)
    }

    /// Reads the next token without consuming it.
    pub(crate) fn peek(&mut self, mode: Mode) -> Result<Token, LexError> {
        let saved = self.pos;
        let token = self.next(mode);
        self.pos = saved;
        token
    }

    /// Reads and consumes the next token.
    pub(crate) fn next(&mut self, mode: Mode) -> Result<Token, LexError> {
        self.skip_blanks(mode)?;
        let start = self.pos;
        let rest = self.src.get(start..).unwrap_or_default();
        let Some(&first) = rest.first() else {
            return Ok(Token {
                kind: TokenKind::Eof,
                start,
                end: start,
            });
        };

        // Strings are the same in every state that has them.
        if first == b'"' && mode != Mode::VersionScript {
            let body = rest.get(1..).unwrap_or_default();
            let Some(close) = body.iter().position(|&b| b == b'"') else {
                return Err(LexError {
                    offset: start,
                    message: "unterminated string".into(),
                });
            };
            let len = close.saturating_add(2);
            return Ok(self.finish(TokenKind::Quoted, start, len));
        }

        // Candidates, in flex rule order: earlier rules win ties.
        let mut best: Option<(usize, TokenKind)> = None;
        let mut offer = |len: usize, kind: TokenKind| {
            if len > 0 && best.is_none_or(|(l, _)| len > l) {
                best = Some((len, kind));
            }
        };

        if mode == Mode::Expr {
            for (len, value, hex) in [number_dollar(rest), number_suffixed(rest)]
                .into_iter()
                .flatten()
            {
                offer(
                    len,
                    TokenKind::Int {
                        value,
                        hex_digits: hex,
                    },
                );
            }
        }
        if matches!(mode, Mode::Script | Mode::Expr)
            && let Some((len, value, hex)) = number_plain(rest)
        {
            offer(
                len,
                TokenKind::Int {
                    value,
                    hex_digits: hex,
                },
            );
        }
        let bit = mode_bit(mode);
        if let Some((text, punct, _)) = PUNCTS
            .iter()
            .find(|(text, _, modes)| modes & bit != 0 && rest.starts_with(text))
        {
            offer(text.len(), TokenKind::Punct(*punct));
        }
        match mode {
            Mode::Script => {
                if is_filename_char1(first) {
                    offer(run(rest, is_filename_char), TokenKind::Name);
                }
            }
            Mode::InputList => {
                if is_filename_char1(first) {
                    offer(run(rest, is_filename_char), TokenKind::Name);
                }
                if first == b'=' && rest.get(1).copied().is_some_and(is_filename_char1) {
                    let tail = rest.get(1..).unwrap_or_default();
                    offer(
                        run(tail, is_filename_char).saturating_add(1),
                        TokenKind::Name,
                    );
                }
                if rest.starts_with(b"-l") {
                    let tail = rest.get(2..).unwrap_or_default();
                    let n = run(tail, is_filename_char);
                    if n > 0 {
                        offer(n.saturating_add(2), TokenKind::LibName);
                    }
                }
            }
            Mode::Expr => {
                if is_symbol_char1(first) {
                    offer(run(rest, is_symbol_char), TokenKind::Name);
                }
                if rest.starts_with(b"/DISCARD/") {
                    offer(9, TokenKind::Name);
                }
            }
            Mode::Wild => {
                offer(run(rest, is_wild_char), TokenKind::Name);
            }
            Mode::VersionScript => {
                if is_vers_tag1(first) {
                    offer(run(rest, is_vers_tag), TokenKind::Name);
                }
            }
            Mode::VersionNode => {
                if is_vers_ident1(first) {
                    offer(vers_ident_len(rest), TokenKind::Name);
                }
            }
        }

        match best {
            Some((len, kind)) => Ok(self.finish(kind, start, len)),
            None => Err(LexError {
                offset: start,
                message: invalid_char_message(first),
            }),
        }
    }

    fn finish(&mut self, kind: TokenKind, start: usize, len: usize) -> Token {
        let end = start.saturating_add(len).min(self.src.len());
        self.pos = end;
        Token { kind, start, end }
    }

    /// Skips whitespace and comments valid in `mode`.
    fn skip_blanks(&mut self, mode: Mode) -> Result<(), LexError> {
        loop {
            let rest = self.src.get(self.pos..).unwrap_or_default();
            match rest {
                [b' ' | b'\t' | b'\r' | b'\n', ..] => {
                    self.pos = self.pos.saturating_add(1);
                }
                // In the WILD state GNU matches `/*` as the start of a
                // pattern and then gives it back when it opens a comment;
                // the effect is a comment wherever a token would start.
                [b'/', b'*', ..] => {
                    let body = rest.get(2..).unwrap_or_default();
                    let Some(close) = body.windows(2).position(|w| w == b"*/") else {
                        return Err(LexError {
                            offset: self.pos,
                            message: "unterminated comment".into(),
                        });
                    };
                    self.pos = self.pos.saturating_add(close.saturating_add(4));
                }
                [b'#', ..] if mode != Mode::InputList => {
                    let line = rest.iter().position(|&b| b == b'\n').unwrap_or(rest.len());
                    self.pos = self.pos.saturating_add(line);
                }
                _ => return Ok(()),
            }
        }
    }
}

/// Length of a `V_IDENTIFIER`, which may contain `::`.
fn vers_ident_len(s: &[u8]) -> usize {
    let mut i = 0usize;
    loop {
        match s.get(i..) {
            Some([b, ..]) if (i == 0 && is_vers_ident1(*b)) || (i > 0 && is_vers_ident(*b)) => {
                i = i.saturating_add(1);
            }
            Some([b':', b':', ..]) if i > 0 => i = i.saturating_add(2),
            _ => return i,
        }
    }
}

fn invalid_char_message(b: u8) -> String {
    if b.is_ascii_graphic() {
        format!("invalid character `{}'", b as char)
    } else {
        format!("invalid character `\\{b:03o}'")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(src: &str, mode: Mode) -> Vec<(TokenKind, String)> {
        let mut lexer = Lexer::new(Cow::Borrowed(src.as_bytes()));
        let mut out = Vec::new();
        loop {
            let token = lexer.next(mode).expect("lex");
            if token.kind == TokenKind::Eof {
                return out;
            }
            out.push((
                token.kind,
                String::from_utf8_lossy(lexer.text(&token)).into_owned(),
            ));
        }
    }

    fn names(src: &str, mode: Mode) -> Vec<String> {
        tokens(src, mode).into_iter().map(|(_, t)| t).collect()
    }

    fn int(src: &str) -> u64 {
        let toks = tokens(src, Mode::Expr);
        assert_eq!(toks.len(), 1, "{src}: {toks:?}");
        match toks[0].0 {
            TokenKind::Int { value, .. } => value,
            other => panic!("{src}: not a number: {other:?}"),
        }
    }

    #[test]
    fn numbers() {
        assert_eq!(int("0x10"), 0x10);
        assert_eq!(int("0X1f"), 0x1f);
        assert_eq!(int("10"), 10);
        assert_eq!(int("010"), 8);
        assert_eq!(int("09"), 0);
        assert_eq!(int("4K"), 4096);
        assert_eq!(int("4k"), 4096);
        assert_eq!(int("2M"), 2 << 20);
        assert_eq!(int("0x10k"), 0x4000);
        assert_eq!(int("$ff"), 0xff);
        assert_eq!(int("1fh"), 0x1f);
        assert_eq!(int("17o"), 15);
        assert_eq!(int("101b"), 5);
        assert_eq!(int("99d"), 99);
        assert_eq!(int("0x"), 0);
        assert_eq!(int("each"), 0xeac);
        assert_eq!(int("99999999999999999999999"), u64::MAX);
        assert!(matches!(
            tokens("0x9090", Mode::Expr)[0].0,
            TokenKind::Int {
                hex_digits: true,
                ..
            }
        ));
        assert!(matches!(
            tokens("0x10K", Mode::Expr)[0].0,
            TokenKind::Int {
                hex_digits: false,
                ..
            }
        ));
        // `0x1fH` is `0x1f` followed by the name `H`.
        assert_eq!(names("0x1fH", Mode::Expr), ["0x1f", "H"]);
        // The suffixed forms only exist in expressions.
        assert_eq!(names("1fh", Mode::Script), ["1", "fh"]);
    }

    #[test]
    fn script_names_glue_like_gnu() {
        assert_eq!(names("foo=1;", Mode::Script), ["foo=1", ";"]);
        assert_eq!(names("elf64-x86-64", Mode::Script), ["elf64-x86-64"]);
        assert_eq!(names(".text : {", Mode::Script), [".text", ":", "{"]);
        assert_eq!(names("ram(rwx)", Mode::Script), ["ram", "(", "rwx", ")"]);
        assert_eq!(names("a /* c */ b", Mode::Script), ["a", "b"]);
        assert_eq!(names("a # c\n b", Mode::Script), ["a", "b"]);
    }

    #[test]
    fn expression_tokens() {
        assert_eq!(names("foo-1", Mode::Expr), ["foo", "-", "1"]);
        assert_eq!(names("foo/2", Mode::Expr), ["foo/2"]);
        assert_eq!(
            names("a<<=b>>c!=d&&e||f", Mode::Expr),
            ["a", "<<=", "b", ">>", "c", "!=", "d", "&&", "e", "||", "f"]
        );
        assert_eq!(names("/DISCARD/", Mode::Expr), ["/DISCARD/"]);
        assert_eq!(names("8/*x*/2", Mode::Expr), ["8", "2"]);
        assert_eq!(names("\"a b\"", Mode::Expr), ["a b"]);
    }

    #[test]
    fn wild_tokens() {
        assert_eq!(
            names("KEEP(*(SORT_NONE(.init)))", Mode::Wild),
            [
                "KEEP",
                "(",
                "*",
                "(",
                "SORT_NONE",
                "(",
                ".init",
                ")",
                ")",
                ")"
            ]
        );
        assert_eq!(
            names("*crtbegin?.o(.ctors)", Mode::Wild),
            ["*crtbegin?.o", "(", ".ctors", ")"]
        );
        assert_eq!(
            names("*(.text, .foo)", Mode::Wild),
            ["*", "(", ".text,", ".foo", ")"]
        );
        assert_eq!(names(". += 4;", Mode::Wild), [".", "+=", "4", ";"]);
        assert_eq!(
            names("*(.text)/* c */", Mode::Wild),
            ["*", "(", ".text", ")"]
        );
        assert_eq!(names("[ .a ]", Mode::Wild), ["[", ".a", "]"]);
        assert_eq!(names("A & !B", Mode::Wild), ["A", "&", "!B"]);
    }

    #[test]
    fn input_list_tokens() {
        let toks = tokens("( /lib/libc.so.6 -lm =/x AS_NEEDED(a) )", Mode::InputList);
        let kinds: Vec<_> = toks.iter().map(|(k, _)| *k).collect();
        assert_eq!(kinds[2], TokenKind::LibName);
        assert_eq!(toks[2].1, "m");
        assert_eq!(toks[3].1, "=/x");
        assert_eq!(names("a.o, b.o", Mode::InputList), ["a.o,", "b.o"]);
    }

    #[test]
    fn version_tokens() {
        assert_eq!(
            names("VERS_1.0 { }", Mode::VersionScript),
            ["VERS_1.0", "{", "}"]
        );
        assert_eq!(
            names("global: foo::bar*; \"x y\";", Mode::VersionNode),
            ["global", ":", "foo::bar*", ";", "x y", ";"]
        );
    }

    #[test]
    fn errors_and_positions() {
        let mut lexer = Lexer::new(Cow::Borrowed(b"a\n  \"open"));
        assert_eq!(lexer.next(Mode::Script).unwrap().kind, TokenKind::Name);
        let error = lexer.next(Mode::Script).unwrap_err();
        assert_eq!(lexer.line_col(error.offset), (2, 3));
        let mut lexer = Lexer::new(Cow::Borrowed(b"/* never closed"));
        assert!(lexer.next(Mode::Expr).is_err());
        let mut lexer = Lexer::new(Cow::Borrowed(b"@"));
        assert!(lexer.next(Mode::Script).unwrap_err().message.contains('@'));
    }
}
