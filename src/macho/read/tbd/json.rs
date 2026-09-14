//! A small JSON parser for `.tbd` v5 files.
//!
//! Numbers keep their text; `true`, `false` and `null` become scalars and
//! [`Value::Null`]. Trailing commas are accepted. Nesting is limited to
//! [`MAX_DEPTH`] levels.

use super::value::{Node, Value};
use super::yaml::ParseError;

/// Maximum nesting of arrays and objects.
pub const MAX_DEPTH: u32 = 64;

type PResult<T> = Result<T, ParseError>;

/// Parses one JSON value, which must be followed only by whitespace.
///
/// # Errors
///
/// Returns the offset and a description of the first syntax error.
pub fn parse(text: &str) -> PResult<Node> {
    let mut parser = Parser {
        s: text.as_bytes(),
        pos: 0,
        depth: 0,
    };
    if parser.s.starts_with(b"\xef\xbb\xbf") {
        parser.pos = 3;
    }
    let node = parser.value()?;
    parser.skip_space();
    if parser.pos < parser.s.len() {
        return parser.fail("trailing characters after the JSON value");
    }
    Ok(node)
}

struct Parser<'t> {
    s: &'t [u8],
    pos: usize,
    depth: u32,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.s.get(self.pos).copied()
    }

    fn advance(&mut self, n: usize) {
        self.pos = self.pos.saturating_add(n).min(self.s.len());
    }

    fn fail<T>(&self, what: &str) -> PResult<T> {
        Err((self.pos, what.to_owned()))
    }

    fn skip_space(&mut self) {
        while self
            .peek()
            .is_some_and(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\r'))
        {
            self.advance(1);
        }
    }

    fn value(&mut self) -> PResult<Node> {
        self.skip_space();
        let offset = self.pos;
        let value = match self.peek() {
            Some(b'{') => return self.object(),
            Some(b'[') => return self.array(),
            Some(b'"') => Value::Scalar(self.string()?),
            Some(b't') => self.keyword("true", Value::Scalar("true".to_owned()))?,
            Some(b'f') => self.keyword("false", Value::Scalar("false".to_owned()))?,
            Some(b'n') => self.keyword("null", Value::Null)?,
            Some(b'-' | b'0'..=b'9') => Value::Scalar(self.number()?),
            None => return self.fail("unexpected end of JSON"),
            Some(_) => return self.fail("unexpected character in JSON"),
        };
        Ok(Node { value, offset })
    }

    fn keyword(&mut self, word: &str, value: Value) -> PResult<Value> {
        if self
            .s
            .get(self.pos..)
            .is_some_and(|rest| rest.starts_with(word.as_bytes()))
        {
            self.advance(word.len());
            Ok(value)
        } else {
            self.fail("unexpected character in JSON")
        }
    }

    fn number(&mut self) -> PResult<String> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.advance(1);
        }
        let digits = |p: &mut Self| {
            let begin = p.pos;
            while p.peek().is_some_and(|b| b.is_ascii_digit()) {
                p.advance(1);
            }
            p.pos > begin
        };
        if !digits(self) {
            return self.fail("malformed number");
        }
        if self.peek() == Some(b'.') {
            self.advance(1);
            if !digits(self) {
                return self.fail("malformed number");
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.advance(1);
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.advance(1);
            }
            if !digits(self) {
                return self.fail("malformed number");
            }
        }
        let text = self.s.get(start..self.pos).unwrap_or(&[]);
        Ok(String::from_utf8_lossy(text).into_owned())
    }

    fn hex4(&mut self) -> PResult<u32> {
        let value = self
            .s
            .get(self.pos..self.pos.saturating_add(4))
            .and_then(|h| std::str::from_utf8(h).ok())
            .filter(|h| h.len() == 4)
            .and_then(|h| u32::from_str_radix(h, 16).ok());
        match value {
            Some(v) => {
                self.advance(4);
                Ok(v)
            }
            None => self.fail("bad \\u escape"),
        }
    }

    fn string(&mut self) -> PResult<String> {
        let start = self.pos;
        self.advance(1);
        let mut out = Vec::new();
        loop {
            match self.peek() {
                None => return Err((start, "unterminated string".to_owned())),
                Some(b'"') => {
                    self.advance(1);
                    break;
                }
                Some(b'\\') => {
                    self.advance(1);
                    let escape = self.peek();
                    self.advance(1);
                    let byte = match escape {
                        Some(b'"') => b'"',
                        Some(b'\\') => b'\\',
                        Some(b'/') => b'/',
                        Some(b'b') => 8,
                        Some(b'f') => 12,
                        Some(b'n') => b'\n',
                        Some(b'r') => b'\r',
                        Some(b't') => b'\t',
                        Some(b'u') => {
                            let first = self.hex4()?;
                            let code = if (0xd800..0xdc00).contains(&first)
                                && self
                                    .s
                                    .get(self.pos..)
                                    .is_some_and(|r| r.starts_with(b"\\u"))
                            {
                                let save = self.pos;
                                self.advance(2);
                                let second = self.hex4()?;
                                if (0xdc00..0xe000).contains(&second) {
                                    0x10000u32
                                        .saturating_add((first.saturating_sub(0xd800)) << 10)
                                        .saturating_add(second.saturating_sub(0xdc00))
                                } else {
                                    self.pos = save;
                                    0xfffd
                                }
                            } else {
                                first
                            };
                            let c = char::from_u32(code).unwrap_or('\u{fffd}');
                            let mut buf = [0u8; 4];
                            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                            continue;
                        }
                        _ => return self.fail("unknown escape sequence"),
                    };
                    out.push(byte);
                }
                Some(b) if b < 0x20 => return self.fail("control character in string"),
                Some(b) => {
                    out.push(b);
                    self.advance(1);
                }
            }
        }
        String::from_utf8(out).map_err(|_| (start, "invalid UTF-8 in string".to_owned()))
    }

    fn enter(&mut self) -> PResult<()> {
        self.depth = self.depth.saturating_add(1);
        if self.depth > MAX_DEPTH {
            return self.fail("nesting too deep");
        }
        Ok(())
    }

    fn array(&mut self) -> PResult<Node> {
        self.enter()?;
        let offset = self.pos;
        self.advance(1);
        let mut items = Vec::new();
        loop {
            self.skip_space();
            if self.peek() == Some(b']') {
                self.advance(1);
                break;
            }
            items.push(self.value()?);
            self.skip_space();
            match self.peek() {
                Some(b',') => self.advance(1),
                Some(b']') => {
                    self.advance(1);
                    break;
                }
                _ => return self.fail("expected ',' or ']'"),
            }
        }
        self.depth = self.depth.saturating_sub(1);
        Ok(Node {
            value: Value::Seq(items),
            offset,
        })
    }

    fn object(&mut self) -> PResult<Node> {
        self.enter()?;
        let offset = self.pos;
        self.advance(1);
        let mut entries: Vec<(String, Node)> = Vec::new();
        loop {
            self.skip_space();
            match self.peek() {
                Some(b'}') => {
                    self.advance(1);
                    break;
                }
                Some(b'"') => {}
                _ => return self.fail("expected a string key"),
            }
            let key_offset = self.pos;
            let key = self.string()?;
            self.skip_space();
            if self.peek() != Some(b':') {
                return self.fail("expected ':'");
            }
            self.advance(1);
            let value = self.value()?;
            if entries.iter().any(|(k, _)| *k == key) {
                return Err((key_offset, format!("duplicate key \"{key}\"")));
            }
            entries.push((key, value));
            self.skip_space();
            match self.peek() {
                Some(b',') => self.advance(1),
                Some(b'}') => {
                    self.advance(1);
                    break;
                }
                _ => return self.fail("expected ',' or '}'"),
            }
        }
        self.depth = self.depth.saturating_sub(1);
        Ok(Node {
            value: Value::Map(entries),
            offset,
        })
    }
}

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)]
mod tests {
    use super::*;

    #[test]
    fn values() {
        let node =
            parse(r#" { "a": [1, -2.5e3, true, null], "b": "x\"\u00e9\ud83d\ude00", "c": {}, } "#)
                .unwrap();
        let a = node.get("a").unwrap().as_seq().unwrap();
        assert_eq!(a[1].as_str(), Some("-2.5e3"));
        assert_eq!(a[3].value, Value::Null);
        assert_eq!(node.get("b").unwrap().as_str(), Some("x\"é😀"));
        for bad in [
            "{",
            "[1 2]",
            "{\"a\" 1}",
            "\"\\x\"",
            "01x",
            "{} {}",
            "\"a\nb\"",
            "{\"a\":1,\"a\":2}",
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
        assert!(parse(&"[".repeat(10_000)).is_err());
    }
}
