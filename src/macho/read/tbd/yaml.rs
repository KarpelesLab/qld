//! A hand-written parser for the YAML subset used by `.tbd` v3 and v4 files.
//!
//! Supported: a stream of documents separated by `---` (with an optional tag
//! such as `!tapi-tbd`) and ended by `...`; block mappings and sequences,
//! including sequences of compact mappings (`- key: value`) and sequences at
//! the same indentation as their key; flow sequences and mappings spanning
//! several lines; plain, single-quoted and double-quoted scalars (with
//! escapes and line folding); literal and folded block scalars; comments.
//!
//! Not supported, because no `.tbd` writer emits them: anchors and aliases,
//! complex keys, multi-line plain scalars, and directives other than being
//! skipped.
//!
//! Every error carries the byte offset where it was found. Nesting is
//! limited to [`MAX_DEPTH`] levels, so hostile input cannot overflow the
//! stack.

use super::value::{Node, Value};

/// Maximum nesting of collections.
pub const MAX_DEPTH: u32 = 64;

/// A parse error: byte offset and description.
pub type ParseError = (usize, String);

type PResult<T> = Result<T, ParseError>;

/// One document of a stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Document {
    /// The tag after `---`, such as `!tapi-tbd`.
    pub tag: Option<String>,
    /// The root node.
    pub root: Node,
    /// Byte offset of the document start.
    pub offset: usize,
}

/// Parses a YAML stream into documents.
///
/// # Errors
///
/// Returns the offset and a description of the first syntax error.
pub fn parse_stream(text: &str) -> PResult<Vec<Document>> {
    let bytes = text.as_bytes();
    let mut documents = Vec::new();
    // (tag, marker offset, content start)
    let mut current: Option<(Option<String>, usize, usize)> = None;
    let mut line_start = 0usize;
    let mut stray_content = false;
    if bytes.starts_with(b"\xef\xbb\xbf") {
        line_start = 3;
    }
    let first_line = line_start;
    while line_start < bytes.len() {
        let line_end = bytes
            .get(line_start..)
            .and_then(|rest| rest.iter().position(|&b| b == b'\n'))
            .map_or(bytes.len(), |n| line_start.saturating_add(n));
        let line = bytes.get(line_start..line_end).unwrap_or(&[]);
        let next = line_end.saturating_add(1);
        let is_marker = |marker: &[u8]| {
            line.starts_with(marker)
                && line
                    .get(3)
                    .is_none_or(|&b| matches!(b, b' ' | b'\t' | b'\r'))
        };
        if is_marker(b"---") {
            if let Some((tag, offset, start)) = current.take() {
                documents.push(parse_document(text, tag, offset, start, line_start)?);
            } else if stray_content {
                documents.push(parse_document(
                    text, None, first_line, first_line, line_start,
                )?);
            }
            stray_content = false;
            let rest = std::str::from_utf8(line.get(3..).unwrap_or(&[]))
                .map_err(|_| (line_start, "invalid UTF-8".to_owned()))?
                .trim();
            let (tag, rest) = if rest.starts_with('!') {
                let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
                (
                    Some(rest.get(..end).unwrap_or("").to_owned()),
                    rest.get(end..).unwrap_or("").trim(),
                )
            } else {
                (None, rest)
            };
            if !rest.is_empty() && !rest.starts_with('#') {
                return Err((line_start, "content on a document start line".to_owned()));
            }
            current = Some((tag, line_start, next.min(bytes.len())));
        } else if is_marker(b"...") {
            if let Some((tag, offset, start)) = current.take() {
                documents.push(parse_document(text, tag, offset, start, line_start)?);
            } else if stray_content {
                documents.push(parse_document(
                    text, None, first_line, first_line, line_start,
                )?);
                stray_content = false;
            }
        } else if current.is_none() {
            let trimmed = line.iter().position(|b| !b.is_ascii_whitespace());
            match trimmed.and_then(|i| line.get(i)) {
                None | Some(b'#') | Some(b'%') => {}
                Some(_) => {
                    if !documents.is_empty() {
                        return Err((line_start, "content after the end of a document".to_owned()));
                    }
                    stray_content = true;
                }
            }
        }
        line_start = next;
    }
    if let Some((tag, offset, start)) = current {
        documents.push(parse_document(text, tag, offset, start, bytes.len())?);
    } else if stray_content {
        documents.push(parse_document(
            text,
            None,
            first_line,
            first_line,
            bytes.len(),
        )?);
    }
    Ok(documents)
}

fn parse_document(
    text: &str,
    tag: Option<String>,
    offset: usize,
    start: usize,
    end: usize,
) -> PResult<Document> {
    let mut parser = Parser {
        s: text.as_bytes(),
        pos: start,
        end,
        depth: 0,
    };
    let root = match parser.content_line_at(start) {
        None => Node {
            value: Value::Null,
            offset: start,
        },
        Some((pos, indent)) => {
            parser.pos = pos;
            let root = parser.parse_block(indent)?;
            if let Some((pos, _)) = parser.next_content_line(parser.pos) {
                return Err((pos, "unexpected content (bad indentation?)".to_owned()));
            }
            root
        }
    };
    Ok(Document { tag, root, offset })
}

struct Parser<'t> {
    s: &'t [u8],
    pos: usize,
    end: usize,
    depth: u32,
}

fn is_blank(b: u8) -> bool {
    matches!(b, b' ' | b'\t')
}

impl Parser<'_> {
    fn at(&self, pos: usize) -> Option<u8> {
        if pos < self.end {
            self.s.get(pos).copied()
        } else {
            None
        }
    }

    fn peek(&self) -> Option<u8> {
        self.at(self.pos)
    }

    fn peek_ahead(&self, n: usize) -> Option<u8> {
        self.at(self.pos.checked_add(n)?)
    }

    fn advance(&mut self, n: usize) {
        self.pos = self.pos.saturating_add(n).min(self.end);
    }

    fn column(&self, at: usize) -> usize {
        let line_start = self
            .s
            .get(..at)
            .and_then(|before| before.iter().rposition(|&b| b == b'\n'))
            .map_or(0, |n| n.saturating_add(1));
        at.saturating_sub(line_start)
    }

    fn skip_inline_space(&mut self) {
        while self.peek().is_some_and(is_blank) {
            self.advance(1);
        }
    }

    /// Whether the rest of the line is empty or a comment.
    fn at_line_end(&self) -> bool {
        match self.peek() {
            None | Some(b'\n' | b'\r') => true,
            Some(b'#') => self
                .pos
                .checked_sub(1)
                .and_then(|p| self.s.get(p))
                .is_none_or(|&b| matches!(b, b' ' | b'\t' | b'\n')),
            _ => false,
        }
    }

    fn fail<T>(&self, what: &str) -> PResult<T> {
        Err((self.pos, what.to_owned()))
    }

    /// The first content character at or after the line starting at
    /// `line_start`, skipping blank and comment lines: its position and
    /// indentation.
    fn content_line_at(&self, mut line_start: usize) -> Option<(usize, usize)> {
        loop {
            if line_start >= self.end {
                return None;
            }
            let mut p = line_start;
            while self.at(p).is_some_and(is_blank) {
                p = p.saturating_add(1);
            }
            match self.at(p) {
                None => return None,
                Some(b'\n' | b'\r' | b'#') => {
                    let newline = self
                        .s
                        .get(p..self.end)
                        .and_then(|rest| rest.iter().position(|&b| b == b'\n'))?;
                    line_start = p.saturating_add(newline).saturating_add(1);
                }
                Some(_) => return Some((p, p.saturating_sub(line_start))),
            }
        }
    }

    /// The first content line after the line containing `from`.
    fn next_content_line(&self, from: usize) -> Option<(usize, usize)> {
        let newline = self
            .s
            .get(from..self.end)
            .and_then(|rest| rest.iter().position(|&b| b == b'\n'))?;
        self.content_line_at(from.saturating_add(newline).saturating_add(1))
    }

    fn is_seq_indicator(&self, pos: usize) -> bool {
        self.at(pos) == Some(b'-')
            && self
                .at(pos.saturating_add(1))
                .is_none_or(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\r'))
    }

    fn enter(&mut self) -> PResult<()> {
        self.depth = self.depth.saturating_add(1);
        if self.depth > MAX_DEPTH {
            return self.fail("nesting too deep");
        }
        Ok(())
    }

    fn leave(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }

    /// Whether the line at `pos` is a `key: value` entry.
    fn is_mapping_entry(&self, pos: usize) -> bool {
        let mut p = pos;
        match self.at(p) {
            Some(quote @ (b'"' | b'\'')) => {
                p = p.saturating_add(1);
                loop {
                    match self.at(p) {
                        None | Some(b'\n') => return false,
                        Some(b'\\') if quote == b'"' => p = p.saturating_add(2),
                        Some(b) if b == quote => {
                            if quote == b'\'' && self.at(p.saturating_add(1)) == Some(b'\'') {
                                p = p.saturating_add(2);
                            } else {
                                p = p.saturating_add(1);
                                break;
                            }
                        }
                        Some(_) => p = p.saturating_add(1),
                    }
                }
                while self.at(p).is_some_and(is_blank) {
                    p = p.saturating_add(1);
                }
                self.at(p) == Some(b':')
                    && self
                        .at(p.saturating_add(1))
                        .is_none_or(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\r'))
            }
            Some(b'[' | b'{' | b'|' | b'>' | b'*' | b'&' | b'!') | None => false,
            Some(_) => loop {
                match self.at(p) {
                    None | Some(b'\n' | b'\r') => return false,
                    Some(b'#')
                        if p > pos
                            && self
                                .s
                                .get(p.saturating_sub(1))
                                .is_some_and(|&b| is_blank(b)) =>
                    {
                        return false;
                    }
                    Some(b':')
                        if self
                            .at(p.saturating_add(1))
                            .is_none_or(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\r')) =>
                    {
                        return true;
                    }
                    Some(_) => p = p.saturating_add(1),
                }
            },
        }
    }

    fn parse_block(&mut self, indent: usize) -> PResult<Node> {
        self.enter()?;
        let node = if self.is_seq_indicator(self.pos) {
            self.parse_block_seq(indent)
        } else if self.is_mapping_entry(self.pos) {
            self.parse_block_map(indent)
        } else {
            self.parse_inline(indent).and_then(|node| {
                self.skip_inline_space();
                if self.at_line_end() {
                    Ok(node)
                } else {
                    self.fail("unexpected characters after a value")
                }
            })
        };
        self.leave();
        node
    }

    fn parse_key(&mut self) -> PResult<String> {
        match self.peek() {
            Some(b'"') => self.parse_double_quoted(),
            Some(b'\'') => self.parse_single_quoted(),
            _ => {
                let start = self.pos;
                while let Some(b) = self.peek() {
                    if b == b':'
                        && self
                            .peek_ahead(1)
                            .is_none_or(|n| matches!(n, b' ' | b'\t' | b'\n' | b'\r'))
                    {
                        break;
                    }
                    if b == b'\n' {
                        return self.fail("mapping key without ':'");
                    }
                    self.advance(1);
                }
                let key = self.s.get(start..self.pos).unwrap_or(&[]);
                to_string(key, start).map(|k| k.trim_end().to_owned())
            }
        }
    }

    fn parse_block_map(&mut self, indent: usize) -> PResult<Node> {
        let offset = self.pos;
        let mut entries: Vec<(String, Node)> = Vec::new();
        loop {
            let key_offset = self.pos;
            let key = self.parse_key()?;
            self.skip_inline_space();
            if self.peek() != Some(b':') {
                return self.fail("expected ':' after a mapping key");
            }
            self.advance(1);
            let value = self.parse_value_after_indicator(indent, true)?;
            if entries.iter().any(|(k, _)| *k == key) {
                return Err((key_offset, format!("duplicate key '{key}'")));
            }
            entries.push((key, value));
            match self.next_content_line(self.pos) {
                Some((p, i)) if i == indent => {
                    if self.is_seq_indicator(p) {
                        return Err((p, "sequence entry inside a mapping".to_owned()));
                    }
                    self.pos = p;
                }
                Some((p, i)) if i > indent => {
                    return Err((p, "unexpected indentation".to_owned()));
                }
                _ => break,
            }
        }
        Ok(Node {
            value: Value::Map(entries),
            offset,
        })
    }

    fn parse_block_seq(&mut self, indent: usize) -> PResult<Node> {
        let offset = self.pos;
        let mut items = Vec::new();
        loop {
            self.advance(1); // '-'
            self.skip_inline_space();
            let item = if self.at_line_end() {
                self.parse_value_after_indicator(indent, false)?
            } else {
                let column = self.column(self.pos);
                if self.is_seq_indicator(self.pos) {
                    self.enter()?;
                    let node = self.parse_block_seq(column);
                    self.leave();
                    node?
                } else if self.is_mapping_entry(self.pos) {
                    self.enter()?;
                    let node = self.parse_block_map(column);
                    self.leave();
                    node?
                } else {
                    let node = self.parse_inline(indent)?;
                    self.skip_inline_space();
                    if !self.at_line_end() {
                        return self.fail("unexpected characters after a value");
                    }
                    node
                }
            };
            items.push(item);
            match self.next_content_line(self.pos) {
                Some((p, i)) if i == indent && self.is_seq_indicator(p) => self.pos = p,
                Some((p, i)) if i > indent => {
                    return Err((p, "unexpected indentation".to_owned()));
                }
                _ => break,
            }
        }
        Ok(Node {
            value: Value::Seq(items),
            offset,
        })
    }

    /// Parses the value after `key:` or `- `.
    fn parse_value_after_indicator(&mut self, parent: usize, compact_seq: bool) -> PResult<Node> {
        self.skip_inline_space();
        if !self.at_line_end() {
            let node = self.parse_inline(parent)?;
            self.skip_inline_space();
            if !self.at_line_end() {
                return self.fail("unexpected characters after a value");
            }
            return Ok(node);
        }
        match self.next_content_line(self.pos) {
            Some((p, i)) if i > parent => {
                self.pos = p;
                self.parse_block(i)
            }
            Some((p, i)) if compact_seq && i == parent && self.is_seq_indicator(p) => {
                self.pos = p;
                self.enter()?;
                let node = self.parse_block_seq(i);
                self.leave();
                node
            }
            _ => Ok(Node {
                value: Value::Null,
                offset: self.pos,
            }),
        }
    }

    fn parse_inline(&mut self, parent: usize) -> PResult<Node> {
        let offset = self.pos;
        match self.peek() {
            Some(b'!' | b'&') => {
                while self.peek().is_some_and(|b| !b.is_ascii_whitespace()) {
                    self.advance(1);
                }
                self.enter()?;
                let node = self.parse_value_after_indicator(parent, true);
                self.leave();
                node
            }
            Some(b'*') => self.fail("aliases are not supported"),
            Some(b'|' | b'>') => self.parse_block_scalar(parent),
            Some(b'[' | b'{') => self.parse_flow(),
            Some(b'"') => Ok(Node {
                value: Value::Scalar(self.parse_double_quoted()?),
                offset,
            }),
            Some(b'\'') => Ok(Node {
                value: Value::Scalar(self.parse_single_quoted()?),
                offset,
            }),
            _ => {
                let start = self.pos;
                while !self.at_line_end() {
                    self.advance(1);
                }
                let text = to_string(self.s.get(start..self.pos).unwrap_or(&[]), start)?;
                let text = text.trim_end();
                Ok(Node {
                    value: if text == "~" || text == "null" {
                        Value::Null
                    } else {
                        Value::Scalar(text.to_owned())
                    },
                    offset,
                })
            }
        }
    }

    fn skip_flow_space(&mut self) {
        while let Some(b) = self.peek() {
            match b {
                b' ' | b'\t' | b'\n' | b'\r' => self.advance(1),
                b'#' => {
                    while self.peek().is_some_and(|b| b != b'\n') {
                        self.advance(1);
                    }
                }
                _ => break,
            }
        }
    }

    fn parse_flow(&mut self) -> PResult<Node> {
        self.enter()?;
        let offset = self.pos;
        let open = self.peek();
        self.advance(1);
        let result = if open == Some(b'[') {
            let mut items = Vec::new();
            loop {
                self.skip_flow_space();
                match self.peek() {
                    None => return self.fail("unterminated flow sequence"),
                    Some(b']') => {
                        self.advance(1);
                        break;
                    }
                    _ => {}
                }
                items.push(self.parse_flow_node()?);
                self.skip_flow_space();
                match self.peek() {
                    Some(b',') => self.advance(1),
                    Some(b']') => {
                        self.advance(1);
                        break;
                    }
                    _ => return self.fail("expected ',' or ']' in a flow sequence"),
                }
            }
            Value::Seq(items)
        } else {
            let mut entries = Vec::new();
            loop {
                self.skip_flow_space();
                match self.peek() {
                    None => return self.fail("unterminated flow mapping"),
                    Some(b'}') => {
                        self.advance(1);
                        break;
                    }
                    _ => {}
                }
                let key = match self.peek() {
                    Some(b'"') => self.parse_double_quoted()?,
                    Some(b'\'') => self.parse_single_quoted()?,
                    _ => self.parse_flow_plain(true)?,
                };
                self.skip_flow_space();
                if self.peek() != Some(b':') {
                    return self.fail("expected ':' in a flow mapping");
                }
                self.advance(1);
                self.skip_flow_space();
                let value = if matches!(self.peek(), Some(b',' | b'}')) {
                    Node {
                        value: Value::Null,
                        offset: self.pos,
                    }
                } else {
                    self.parse_flow_node()?
                };
                entries.push((key, value));
                self.skip_flow_space();
                match self.peek() {
                    Some(b',') => self.advance(1),
                    Some(b'}') => {
                        self.advance(1);
                        break;
                    }
                    _ => return self.fail("expected ',' or '}' in a flow mapping"),
                }
            }
            Value::Map(entries)
        };
        self.leave();
        Ok(Node {
            value: result,
            offset,
        })
    }

    fn parse_flow_node(&mut self) -> PResult<Node> {
        self.skip_flow_space();
        let offset = self.pos;
        let value = match self.peek() {
            Some(b'[' | b'{') => return self.parse_flow(),
            Some(b'"') => Value::Scalar(self.parse_double_quoted()?),
            Some(b'\'') => Value::Scalar(self.parse_single_quoted()?),
            Some(b'*') => return self.fail("aliases are not supported"),
            _ => Value::Scalar(self.parse_flow_plain(false)?),
        };
        Ok(Node { value, offset })
    }

    fn parse_flow_plain(&mut self, key: bool) -> PResult<String> {
        let start = self.pos;
        while let Some(b) = self.peek() {
            let stop = match b {
                b',' | b'[' | b']' | b'{' | b'}' | b'\n' | b'\r' => true,
                b':' => {
                    key || self.peek_ahead(1).is_none_or(|n| {
                        matches!(n, b' ' | b'\t' | b'\n' | b'\r' | b',' | b']' | b'}')
                    })
                }
                b'#' => self.at_line_end(),
                _ => false,
            };
            if stop {
                break;
            }
            self.advance(1);
        }
        let text = to_string(self.s.get(start..self.pos).unwrap_or(&[]), start)?;
        let text = text.trim();
        if text.is_empty() {
            return Err((start, "empty flow entry".to_owned()));
        }
        Ok(text.to_owned())
    }

    /// Folds a line break inside a quoted scalar.
    fn fold_line(&mut self, out: &mut Vec<u8>) {
        while out.last().is_some_and(|&b| is_blank(b)) {
            out.pop();
        }
        self.advance(1); // '\n'
        let mut blank_lines = 0usize;
        loop {
            while self
                .peek()
                .is_some_and(|b| matches!(b, b' ' | b'\t' | b'\r'))
            {
                self.advance(1);
            }
            if self.peek() == Some(b'\n') {
                blank_lines = blank_lines.saturating_add(1);
                self.advance(1);
            } else {
                break;
            }
        }
        if blank_lines == 0 {
            out.push(b' ');
        } else {
            out.extend(std::iter::repeat_n(b'\n', blank_lines));
        }
    }

    fn parse_single_quoted(&mut self) -> PResult<String> {
        let start = self.pos;
        self.advance(1);
        let mut out = Vec::new();
        loop {
            match self.peek() {
                None => return Err((start, "unterminated quoted scalar".to_owned())),
                Some(b'\'') => {
                    if self.peek_ahead(1) == Some(b'\'') {
                        out.push(b'\'');
                        self.advance(2);
                    } else {
                        self.advance(1);
                        break;
                    }
                }
                Some(b'\n') => self.fold_line(&mut out),
                Some(b'\r') => self.advance(1),
                Some(b) => {
                    out.push(b);
                    self.advance(1);
                }
            }
        }
        String::from_utf8(out).map_err(|_| (start, "invalid UTF-8".to_owned()))
    }

    fn parse_double_quoted(&mut self) -> PResult<String> {
        let start = self.pos;
        self.advance(1);
        let mut out = Vec::new();
        loop {
            match self.peek() {
                None => return Err((start, "unterminated quoted scalar".to_owned())),
                Some(b'"') => {
                    self.advance(1);
                    break;
                }
                Some(b'\n') => self.fold_line(&mut out),
                Some(b'\r') => self.advance(1),
                Some(b'\\') => {
                    let escape_at = self.pos;
                    self.advance(1);
                    let Some(e) = self.peek() else {
                        return Err((start, "unterminated quoted scalar".to_owned()));
                    };
                    self.advance(1);
                    let simple = match e {
                        b'0' => Some(0u8),
                        b'a' => Some(7),
                        b'b' => Some(8),
                        b't' | b'\t' => Some(b'\t'),
                        b'n' => Some(b'\n'),
                        b'v' => Some(11),
                        b'f' => Some(12),
                        b'r' => Some(b'\r'),
                        b'e' => Some(27),
                        b' ' => Some(b' '),
                        b'"' => Some(b'"'),
                        b'/' => Some(b'/'),
                        b'\\' => Some(b'\\'),
                        _ => None,
                    };
                    if let Some(byte) = simple {
                        out.push(byte);
                        continue;
                    }
                    let digits = match e {
                        b'x' => 2,
                        b'u' => 4,
                        b'U' => 8,
                        b'N' => {
                            out.extend_from_slice("\u{85}".as_bytes());
                            continue;
                        }
                        b'_' => {
                            out.extend_from_slice("\u{a0}".as_bytes());
                            continue;
                        }
                        b'L' => {
                            out.extend_from_slice("\u{2028}".as_bytes());
                            continue;
                        }
                        b'P' => {
                            out.extend_from_slice("\u{2029}".as_bytes());
                            continue;
                        }
                        b'\n' => {
                            // Escaped line break: join without a space.
                            self.skip_inline_space();
                            continue;
                        }
                        b'\r' => {
                            if self.peek() == Some(b'\n') {
                                self.advance(1);
                            }
                            self.skip_inline_space();
                            continue;
                        }
                        _ => return Err((escape_at, "unknown escape sequence".to_owned())),
                    };
                    let hex = self
                        .s
                        .get(self.pos..self.pos.saturating_add(digits))
                        .filter(|_| self.pos.saturating_add(digits) <= self.end)
                        .and_then(|h| std::str::from_utf8(h).ok())
                        .and_then(|h| u32::from_str_radix(h, 16).ok())
                        .ok_or_else(|| (escape_at, "bad hexadecimal escape".to_owned()))?;
                    self.advance(digits);
                    let c = char::from_u32(hex).unwrap_or('\u{fffd}');
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                }
                Some(b) => {
                    out.push(b);
                    self.advance(1);
                }
            }
        }
        String::from_utf8(out).map_err(|_| (start, "invalid UTF-8".to_owned()))
    }

    fn parse_block_scalar(&mut self, parent: usize) -> PResult<Node> {
        let offset = self.pos;
        let literal = self.peek() == Some(b'|');
        self.advance(1);
        let mut chomp = 0i8; // -1 strip, 0 clip, 1 keep
        let mut explicit_indent = None;
        for _ in 0..2 {
            match self.peek() {
                Some(b'-') => chomp = -1,
                Some(b'+') => chomp = 1,
                Some(d @ b'1'..=b'9') => explicit_indent = Some(usize::from(d.wrapping_sub(b'0'))),
                _ => break,
            }
            self.advance(1);
        }
        self.skip_inline_space();
        if !self.at_line_end() {
            return self.fail("unexpected characters after a block scalar header");
        }
        // Collect the content lines.
        let mut lines: Vec<&[u8]> = Vec::new();
        let mut content_indent = explicit_indent.map(|i| parent.saturating_add(i));
        let mut last_end = self.pos;
        let mut cursor = self
            .s
            .get(self.pos..self.end)
            .and_then(|rest| rest.iter().position(|&b| b == b'\n'))
            .map(|n| self.pos.saturating_add(n).saturating_add(1));
        while let Some(line_start) = cursor.filter(|&c| c < self.end) {
            let line_end = self
                .s
                .get(line_start..self.end)
                .and_then(|rest| rest.iter().position(|&b| b == b'\n'))
                .map_or(self.end, |n| line_start.saturating_add(n));
            let raw = self.s.get(line_start..line_end).unwrap_or(&[]);
            let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
            let spaces = raw.iter().take_while(|&&b| b == b' ').count();
            if spaces == raw.len() {
                lines.push(&[]);
            } else {
                let needed = match content_indent {
                    Some(i) => i,
                    None if spaces > parent => {
                        content_indent = Some(spaces);
                        spaces
                    }
                    None => break,
                };
                if spaces < needed {
                    break;
                }
                lines.push(raw.get(needed..).unwrap_or(&[]));
                last_end = line_end;
            }
            cursor = Some(line_end.saturating_add(1));
        }
        // Trailing blank lines after the last content line belong to
        // chomping only.
        let content_lines = lines
            .iter()
            .rposition(|l| !l.is_empty())
            .map_or(0, |i| i.saturating_add(1));
        let trailing_blank = lines.len().saturating_sub(content_lines);
        let mut out = Vec::new();
        for (i, line) in lines.iter().take(content_lines).enumerate() {
            if i > 0 {
                let previous_empty = lines.get(i.saturating_sub(1)).is_some_and(|l| l.is_empty());
                if literal || line.is_empty() || previous_empty {
                    out.push(b'\n');
                } else {
                    out.push(b' ');
                }
            }
            out.extend_from_slice(line);
        }
        if content_lines > 0 {
            match chomp {
                -1 => {}
                0 => out.push(b'\n'),
                _ => out.extend(std::iter::repeat_n(b'\n', trailing_blank.saturating_add(1))),
            }
        }
        self.pos = last_end;
        Ok(Node {
            value: Value::Scalar(
                String::from_utf8(out).map_err(|_| (offset, "invalid UTF-8".to_owned()))?,
            ),
            offset,
        })
    }
}

fn to_string(bytes: &[u8], offset: usize) -> PResult<&str> {
    std::str::from_utf8(bytes).map_err(|_| (offset, "invalid UTF-8".to_owned()))
}

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)]
mod tests {
    use super::*;

    fn scalar(s: &str) -> Value {
        Value::Scalar(s.to_owned())
    }

    fn strip(node: &Node) -> String {
        match &node.value {
            Value::Null => "~".to_owned(),
            Value::Scalar(s) => format!("{s:?}"),
            Value::Seq(items) => format!(
                "[{}]",
                items.iter().map(strip).collect::<Vec<_>>().join(", ")
            ),
            Value::Map(entries) => format!(
                "{{{}}}",
                entries
                    .iter()
                    .map(|(k, v)| format!("{k}: {}", strip(v)))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }

    fn one(text: &str) -> String {
        let docs = parse_stream(text).unwrap();
        assert_eq!(docs.len(), 1, "{text}");
        strip(&docs[0].root)
    }

    #[test]
    fn block_structures() {
        assert_eq!(
            one("a: 1\nb:\n  - x\n  - 'y z'\nc:\n- p\n- q\nd:\n  e: \"f\\tg\"\n"),
            r#"{a: "1", b: ["x", "y z"], c: ["p", "q"], d: {e: "f\tg"}}"#
        );
        assert_eq!(
            one(
                "exports:\n  - targets: [ a, b ]\n    symbols: [ _x,\n               _y ]\n  - targets: [ c ]\n"
            ),
            r#"{exports: [{targets: ["a", "b"], symbols: ["_x", "_y"]}, {targets: ["c"]}]}"#
        );
        assert_eq!(one("- - a\n  - b\n- c\n"), r#"[["a", "b"], "c"]"#);
        assert_eq!(one("k:   # comment\n  v # trailing\n"), r#"{k: "v"}"#);
        assert_eq!(
            one("empty:\nnext: [ ]\nmap: { }\n"),
            "{empty: ~, next: [], map: {}}"
        );
        assert_eq!(
            one("q: 'it''s'\nr: \"a\\x41\\u00e9\"\n"),
            r#"{q: "it's", r: "aAé"}"#
        );
        assert_eq!(
            one("f: { a: 1, b: [2, 3] }\n"),
            r#"{f: {a: "1", b: ["2", "3"]}}"#
        );
        assert_eq!(one("s: 'folded\n   line'\n"), r#"{s: "folded line"}"#);
        assert_eq!(
            one("l: |\n  one\n  two\n\nm: >-\n  a\n  b\n"),
            r#"{l: "one\ntwo\n", m: "a b"}"#
        );
        assert_eq!(one("url: http://x.y/z\n"), r#"{url: "http://x.y/z"}"#);
    }

    #[test]
    fn documents() {
        let docs = parse_stream("--- !tapi-tbd\na: 1\n...\n--- !tapi-tbd-v3\nb: 2\n").unwrap();
        assert_eq!(docs.len(), 2);
        assert_eq!(docs[0].tag.as_deref(), Some("!tapi-tbd"));
        assert_eq!(docs[1].tag.as_deref(), Some("!tapi-tbd-v3"));
        assert_eq!(docs[1].root.get("b").unwrap().value, scalar("2"));
        let docs = parse_stream("--- !a\nx: 1\n--- !b\ny: 2\n").unwrap();
        assert_eq!(docs.len(), 2);
        assert_eq!(parse_stream("a: 1\n").unwrap().len(), 1);
    }

    #[test]
    fn errors() {
        for bad in [
            "a: [1, 2\n",
            "a: 'open\n",
            "a: 1\n  b: 2\n",
            "a: 1\na: 2\n",
            "a: *alias\n",
            "- a\nb: 1\n",
            "a: \"\\q\"\n",
            "a: {b 1}\n",
        ] {
            assert!(parse_stream(bad).is_err(), "{bad:?}");
        }
        let deep = "[".repeat(1000);
        assert!(parse_stream(&format!("a: {deep}\n")).is_err());
        let deep: String = (0..200)
            .map(|i| format!("{}- \n", " ".repeat(i * 2)))
            .collect();
        let _ = parse_stream(&deep);
    }
}
