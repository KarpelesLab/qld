//! A parser for the small TOML subset that `test.toml` fixture files use.
//!
//! Supported: comments, bare and quoted keys, dotted keys (`expect.stdout`),
//! `[table]` headers, basic and literal strings (single- and multi-line),
//! integers, booleans, and arrays of those values (which may span lines).
//! Not supported: floats, dates, inline tables and arrays of tables. Anything
//! outside the subset is a parse error with a line number, never a panic.
//!
//! The parser is std-only on purpose: qld's test code takes no dependencies.

use std::fmt;

/// A TOML value from the supported subset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    /// A string, with escapes already processed.
    String(String),
    /// A 64-bit signed integer.
    Integer(i64),
    /// `true` or `false`.
    Bool(bool),
    /// An array of values.
    Array(Vec<Value>),
}

impl Value {
    /// Name of the value's type, for error messages.
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::String(_) => "string",
            Self::Integer(_) => "integer",
            Self::Bool(_) => "boolean",
            Self::Array(_) => "array",
        }
    }
}

/// One `key = value` assignment, with its full dotted key path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Key segments, including the segments of the enclosing `[table]`.
    pub key: Vec<String>,
    /// The assigned value.
    pub value: Value,
    /// 1-based line number of the key.
    pub line: usize,
}

impl Entry {
    /// The key as it would be written in the file (quoted where needed).
    pub fn key_display(&self) -> String {
        display_key(&self.key)
    }
}

/// Formats a key path, quoting segments that are not bare keys.
pub fn display_key(key: &[String]) -> String {
    key.iter()
        .map(|segment| {
            if !segment.is_empty() && segment.chars().all(is_bare_key_char) {
                segment.clone()
            } else {
                format!("{segment:?}")
            }
        })
        .collect::<Vec<_>>()
        .join(".")
}

/// A parse error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError {
    /// 1-based line number where the problem was found.
    pub line: usize,
    /// What was wrong.
    pub message: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

/// Parses a document into its entries, in file order.
///
/// Duplicate keys, and keys that are both a value and a table
/// (`a = 1` and `a.b = 2`), are errors.
pub fn parse(source: &str) -> PResult<Vec<Entry>> {
    Parser {
        chars: source.chars().collect(),
        pos: 0,
        line: 1,
    }
    .document()
}

fn is_bare_key_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

struct Parser {
    chars: Vec<char>,
    pos: usize,
    line: usize,
}

type PResult<T> = std::result::Result<T, ParseError>;

impl Parser {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn peek_at(&self, offset: usize) -> Option<char> {
        self.chars.get(self.pos.checked_add(offset)?).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += 1;
        if c == '\n' {
            self.line += 1;
        }
        Some(c)
    }

    fn starts_with(&self, pattern: &str) -> bool {
        pattern
            .chars()
            .enumerate()
            .all(|(i, c)| self.peek_at(i) == Some(c))
    }

    fn error<T>(&self, message: impl Into<String>) -> PResult<T> {
        Err(ParseError {
            line: self.line,
            message: message.into(),
        })
    }

    fn skip_spaces(&mut self) {
        while matches!(self.peek(), Some(' ' | '\t')) {
            self.bump();
        }
    }

    fn skip_comment(&mut self) {
        if self.peek() == Some('#') {
            while !matches!(self.peek(), None | Some('\n')) {
                self.bump();
            }
        }
    }

    /// Skips spaces, comments and line breaks.
    fn skip_blank(&mut self) {
        loop {
            self.skip_spaces();
            self.skip_comment();
            match self.peek() {
                Some('\n' | '\r') => {
                    self.bump();
                }
                _ => break,
            }
        }
    }

    fn end_of_line(&mut self) -> PResult<()> {
        self.skip_spaces();
        self.skip_comment();
        match self.peek() {
            None => Ok(()),
            Some('\n') => {
                self.bump();
                Ok(())
            }
            Some('\r') if self.peek_at(1) == Some('\n') => {
                self.bump();
                self.bump();
                Ok(())
            }
            Some(c) => self.error(format!("expected end of line, found {c:?}")),
        }
    }

    fn expect(&mut self, wanted: char) -> PResult<()> {
        match self.peek() {
            Some(c) if c == wanted => {
                self.bump();
                Ok(())
            }
            Some(c) => self.error(format!("expected {wanted:?}, found {c:?}")),
            None => self.error(format!("expected {wanted:?}, found end of file")),
        }
    }

    fn document(mut self) -> PResult<Vec<Entry>> {
        let mut entries: Vec<Entry> = Vec::new();
        let mut table: Vec<String> = Vec::new();
        loop {
            self.skip_blank();
            match self.peek() {
                None => break,
                Some('[') => {
                    if self.peek_at(1) == Some('[') {
                        return self.error("arrays of tables are not supported");
                    }
                    self.bump();
                    table = self.key()?;
                    self.skip_spaces();
                    self.expect(']')?;
                    self.end_of_line()?;
                }
                Some(_) => {
                    let line = self.line;
                    let mut key = table.clone();
                    key.extend(self.key()?);
                    self.skip_spaces();
                    self.expect('=')?;
                    self.skip_spaces();
                    let value = self.value()?;
                    self.end_of_line()?;
                    for existing in &entries {
                        let shorter = existing.key.len().min(key.len());
                        if existing.key[..shorter] == key[..shorter] {
                            return Err(ParseError {
                                line,
                                message: format!(
                                    "key `{}` conflicts with `{}` on line {}",
                                    display_key(&key),
                                    existing.key_display(),
                                    existing.line
                                ),
                            });
                        }
                    }
                    entries.push(Entry { key, value, line });
                }
            }
        }
        Ok(entries)
    }

    fn key(&mut self) -> PResult<Vec<String>> {
        let mut segments = Vec::new();
        loop {
            self.skip_spaces();
            let segment = match self.peek() {
                Some('"') => self.basic_string()?,
                Some('\'') => self.literal_string()?,
                Some(c) if is_bare_key_char(c) => {
                    let mut segment = String::new();
                    while let Some(c) = self.peek().filter(|&c| is_bare_key_char(c)) {
                        segment.push(c);
                        self.bump();
                    }
                    segment
                }
                Some(c) => return self.error(format!("expected a key, found {c:?}")),
                None => return self.error("expected a key, found end of file"),
            };
            segments.push(segment);
            self.skip_spaces();
            if self.peek() == Some('.') {
                self.bump();
            } else {
                return Ok(segments);
            }
        }
    }

    fn value(&mut self) -> PResult<Value> {
        match self.peek() {
            Some('"') if self.starts_with("\"\"\"") => {
                self.multiline_basic_string().map(Value::String)
            }
            Some('"') => self.basic_string().map(Value::String),
            Some('\'') if self.starts_with("'''") => {
                self.multiline_literal_string().map(Value::String)
            }
            Some('\'') => self.literal_string().map(Value::String),
            Some('[') => self.array(),
            Some('t') if self.starts_with("true") => {
                self.pos += 4;
                Ok(Value::Bool(true))
            }
            Some('f') if self.starts_with("false") => {
                self.pos += 5;
                Ok(Value::Bool(false))
            }
            Some(c) if c == '+' || c == '-' || c.is_ascii_digit() => self.integer(),
            Some(c) => self.error(format!("expected a value, found {c:?}")),
            None => self.error("expected a value, found end of file"),
        }
    }

    fn array(&mut self) -> PResult<Value> {
        self.expect('[')?;
        let mut items = Vec::new();
        loop {
            self.skip_blank();
            if self.peek() == Some(']') {
                self.bump();
                break;
            }
            items.push(self.value()?);
            self.skip_blank();
            match self.peek() {
                Some(',') => {
                    self.bump();
                }
                Some(']') => {
                    self.bump();
                    break;
                }
                Some(c) => return self.error(format!("expected ',' or ']' in array, found {c:?}")),
                None => return self.error("unterminated array"),
            }
        }
        Ok(Value::Array(items))
    }

    fn integer(&mut self) -> PResult<Value> {
        let mut text = String::new();
        while let Some(c) = self
            .peek()
            .filter(|&c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '_'))
        {
            text.push(c);
            self.bump();
        }
        let cleaned: String = text.chars().filter(|&c| c != '_').collect();
        let (negative, digits) = match cleaned.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, cleaned.strip_prefix('+').unwrap_or(&cleaned)),
        };
        let parsed = if let Some(hex) = digits.strip_prefix("0x") {
            i64::from_str_radix(hex, 16)
        } else if let Some(octal) = digits.strip_prefix("0o") {
            i64::from_str_radix(octal, 8)
        } else if let Some(binary) = digits.strip_prefix("0b") {
            i64::from_str_radix(binary, 2)
        } else {
            digits.parse::<i64>()
        };
        match parsed {
            Ok(value) if negative => Ok(Value::Integer(-value)),
            Ok(value) => Ok(Value::Integer(value)),
            Err(_) => self.error(format!("invalid integer {text:?}")),
        }
    }

    fn escape(&mut self, out: &mut String) -> PResult<()> {
        let c = match self.bump() {
            Some('n') => '\n',
            Some('t') => '\t',
            Some('r') => '\r',
            Some('b') => '\u{8}',
            Some('f') => '\u{c}',
            Some('e') => '\u{1b}',
            Some('"') => '"',
            Some('\\') => '\\',
            Some(kind @ ('u' | 'U')) => {
                let len = if kind == 'u' { 4 } else { 8 };
                let mut hex = String::new();
                for _ in 0..len {
                    match self.bump() {
                        Some(c) if c.is_ascii_hexdigit() => hex.push(c),
                        _ => return self.error("invalid unicode escape"),
                    }
                }
                match u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                    Some(c) => c,
                    None => return self.error(format!("invalid unicode scalar \\{kind}{hex}")),
                }
            }
            Some(c) => return self.error(format!("invalid escape \\{c}")),
            None => return self.error("unterminated string"),
        };
        out.push(c);
        Ok(())
    }

    fn basic_string(&mut self) -> PResult<String> {
        self.expect('"')?;
        let mut out = String::new();
        loop {
            match self.peek() {
                None | Some('\n') => return self.error("unterminated string"),
                Some('"') => {
                    self.bump();
                    return Ok(out);
                }
                Some('\\') => {
                    self.bump();
                    self.escape(&mut out)?;
                }
                Some(c) => {
                    self.bump();
                    out.push(c);
                }
            }
        }
    }

    fn literal_string(&mut self) -> PResult<String> {
        self.expect('\'')?;
        let mut out = String::new();
        loop {
            match self.peek() {
                None | Some('\n') => return self.error("unterminated string"),
                Some('\'') => {
                    self.bump();
                    return Ok(out);
                }
                Some(c) => {
                    self.bump();
                    out.push(c);
                }
            }
        }
    }

    /// Skips the line break that may directly follow an opening `"""`/`'''`.
    fn skip_leading_newline(&mut self) {
        if self.peek() == Some('\n') {
            self.bump();
        } else if self.starts_with("\r\n") {
            self.bump();
            self.bump();
        }
    }

    /// Consumes a closing delimiter of three `quote`s. Up to two extra quotes
    /// directly before the delimiter belong to the string, as in TOML.
    fn close_multiline(&mut self, quote: char, out: &mut String) -> bool {
        let delimiter: String = std::iter::repeat_n(quote, 3).collect();
        if !self.starts_with(&delimiter) {
            return false;
        }
        let mut run = 0;
        while self.peek_at(run) == Some(quote) {
            run += 1;
        }
        for _ in 0..run.saturating_sub(3).min(2) {
            out.push(quote);
        }
        self.pos += run.min(5);
        true
    }

    fn multiline_basic_string(&mut self) -> PResult<String> {
        self.pos += 3;
        self.skip_leading_newline();
        let mut out = String::new();
        loop {
            if self.close_multiline('"', &mut out) {
                return Ok(out);
            }
            match self.peek() {
                None => return self.error("unterminated multi-line string"),
                Some('\\') => {
                    self.bump();
                    if matches!(self.peek(), Some(' ' | '\t' | '\n' | '\r')) {
                        // Line-ending backslash: trim all whitespace that follows.
                        while matches!(self.peek(), Some(' ' | '\t' | '\n' | '\r')) {
                            self.bump();
                        }
                    } else {
                        self.escape(&mut out)?;
                    }
                }
                Some(c) => {
                    self.bump();
                    out.push(c);
                }
            }
        }
    }

    fn multiline_literal_string(&mut self) -> PResult<String> {
        self.pos += 3;
        self.skip_leading_newline();
        let mut out = String::new();
        loop {
            if self.close_multiline('\'', &mut out) {
                return Ok(out);
            }
            match self.bump() {
                None => return self.error("unterminated multi-line string"),
                Some(c) => out.push(c),
            }
        }
    }
}
