//! Rust v0 mangling (`_R...`), RFC 2603.
//!
//! The grammar is parsed and printed in one pass. Back-references (`B`)
//! restart parsing at an earlier position, which must lie strictly before
//! the reference, so they cannot loop; they can still expand exponentially,
//! which the bounded [`Output`] stops.
//!
//! With [`Options::rust_hash`](super::Options::rust_hash), crate
//! disambiguators (`core[2e27404414be4892]`) and the types of constants
//! (`5: usize`) are printed, as `c++filt` does; without it they are hidden,
//! as `rustfilt` does. Trailing `.suffix` parts are dropped.

use super::output::Output;
use super::{MAX_DEPTH, MAX_OUTPUT};

/// Demangles a v0 name, or returns `None`.
pub(crate) fn demangle(name: &[u8], verbose: bool, out: &mut Output) -> Option<()> {
    let body = name
        .strip_prefix(b"_R")
        .or_else(|| name.strip_prefix(b"__R"))?;
    let end = body.iter().position(|&c| c == b'.').unwrap_or(body.len());
    let sym = body.get(..end)?;
    if !sym.first()?.is_ascii_uppercase()
        || !sym.iter().all(|&c| c == b'_' || c.is_ascii_alphanumeric())
    {
        return None;
    }
    let mut p = Printer {
        sym,
        next: 0,
        depth: 0,
        out,
        skipping: 0,
        verbose,
        bound_lifetimes: 0,
        in_const_value: 0,
    };
    p.path(true)?;
    if p.next < sym.len() {
        // The instantiating crate: parsed, not printed.
        p.skipping = p.skipping.saturating_add(1);
        p.path(false)?;
        p.skipping = p.skipping.saturating_sub(1);
    }
    (p.next == sym.len() && !p.out.failed()).then_some(())
}

struct Printer<'s, 'o> {
    sym: &'s [u8],
    next: usize,
    depth: usize,
    out: &'o mut Output,
    /// Nonzero while parsing without printing.
    skipping: u32,
    verbose: bool,
    bound_lifetimes: u64,
    /// Nesting depth inside array, tuple, reference and ADT constants.
    in_const_value: u32,
}

/// An identifier: an ASCII part and an optional Punycode part.
#[derive(Clone, Copy)]
struct Ident<'s> {
    ascii: &'s [u8],
    punycode: &'s [u8],
}

impl Ident<'_> {
    fn is_empty(&self) -> bool {
        self.ascii.is_empty() && self.punycode.is_empty()
    }
}

type R<T> = Option<T>;

fn basic_type(tag: u8) -> Option<&'static str> {
    Some(match tag {
        b'b' => "bool",
        b'c' => "char",
        b'e' => "str",
        b'u' => "()",
        b'a' => "i8",
        b's' => "i16",
        b'l' => "i32",
        b'x' => "i64",
        b'n' => "i128",
        b'i' => "isize",
        b'h' => "u8",
        b't' => "u16",
        b'm' => "u32",
        b'y' => "u64",
        b'o' => "u128",
        b'j' => "usize",
        b'f' => "f32",
        b'd' => "f64",
        b'z' => "!",
        b'p' => "_",
        b'v' => "...",
        _ => return None,
    })
}

impl<'s> Printer<'s, '_> {
    // ----- parsing -----

    fn peek(&self) -> Option<u8> {
        self.sym.get(self.next).copied()
    }

    fn next(&mut self) -> R<u8> {
        let c = self.peek()?;
        self.next = self.next.checked_add(1)?;
        Some(c)
    }

    fn eat(&mut self, c: u8) -> bool {
        if self.peek() == Some(c) {
            self.next = self.next.saturating_add(1);
            true
        } else {
            false
        }
    }

    fn enter(&mut self) -> R<()> {
        if self.depth >= MAX_DEPTH || !self.out.step() {
            return None;
        }
        self.depth = self.depth.saturating_add(1);
        Some(())
    }

    fn leave(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }

    /// `_` is 0; `<base-62 digits> _` is value + 1.
    fn integer_62(&mut self) -> R<u64> {
        if self.eat(b'_') {
            return Some(0);
        }
        let mut x: u64 = 0;
        while !self.eat(b'_') {
            let c = self.next()?;
            let digit = match c {
                b'0'..=b'9' => c.wrapping_sub(b'0'),
                b'a'..=b'z' => c.wrapping_sub(b'a').wrapping_add(10),
                b'A'..=b'Z' => c.wrapping_sub(b'A').wrapping_add(36),
                _ => return None,
            };
            x = x.checked_mul(62)?.checked_add(u64::from(digit))?;
        }
        x.checked_add(1)
    }

    /// `[<tag> <base-62-number>]`: 0 when absent, value + 1 otherwise.
    fn opt_integer_62(&mut self, tag: u8) -> R<u64> {
        if !self.eat(tag) {
            return Some(0);
        }
        self.integer_62()?.checked_add(1)
    }

    fn disambiguator(&mut self) -> R<u64> {
        self.opt_integer_62(b's')
    }

    fn ident(&mut self) -> R<Ident<'s>> {
        let is_punycode = self.eat(b'u');
        let first = self.next()?;
        if !first.is_ascii_digit() {
            return None;
        }
        let mut len = usize::from(first.wrapping_sub(b'0'));
        if first != b'0' {
            while let Some(c) = self.peek().filter(u8::is_ascii_digit) {
                len = len
                    .checked_mul(10)?
                    .checked_add(usize::from(c.wrapping_sub(b'0')))?;
                self.next = self.next.checked_add(1)?;
            }
        }
        self.eat(b'_');
        let start = self.next;
        let end = start.checked_add(len)?;
        let bytes = self.sym.get(start..end)?;
        self.next = end;
        if !is_punycode {
            return Some(Ident {
                ascii: bytes,
                punycode: &[],
            });
        }
        let ident = match bytes.iter().rposition(|&c| c == b'_') {
            Some(i) => Ident {
                ascii: bytes.get(..i)?,
                punycode: bytes.get(i.checked_add(1)?..)?,
            },
            None => Ident {
                ascii: &[],
                punycode: bytes,
            },
        };
        if ident.punycode.is_empty() {
            return None;
        }
        Some(ident)
    }

    fn hex_nibbles(&mut self) -> R<&'s [u8]> {
        let start = self.next;
        while !self.eat(b'_') {
            let c = self.next()?;
            if !matches!(c, b'0'..=b'9' | b'a'..=b'f') {
                return None;
            }
        }
        self.sym.get(start..self.next.checked_sub(1)?)
    }

    /// Parses a back-reference and returns its target position.
    fn backref(&mut self) -> R<usize> {
        let start = self.next.checked_sub(1)?;
        let target = usize::try_from(self.integer_62()?).ok()?;
        (target < start).then_some(target)
    }

    // ----- printing -----

    fn print(&mut self, text: &str) {
        if self.skipping == 0 {
            self.out.push(text);
        }
    }

    fn print_u64(&mut self, value: u64) {
        if self.skipping == 0 {
            self.out.push_u64(value);
        }
    }

    fn print_hex(&mut self, value: u64) {
        if self.skipping == 0 {
            self.out.push(&format!("{value:x}"));
        }
    }

    fn print_ident(&mut self, ident: Ident<'_>) {
        if self.skipping != 0 {
            return;
        }
        if ident.punycode.is_empty() {
            self.out.push_bytes(ident.ascii);
            return;
        }
        match punycode_decode(ident.ascii, ident.punycode) {
            Some(text) => self.out.push(&text),
            None => {
                self.out.push("punycode{");
                if !ident.ascii.is_empty() {
                    self.out.push_bytes(ident.ascii);
                    self.out.push("-");
                }
                self.out.push_bytes(ident.punycode);
                self.out.push("}");
            }
        }
    }

    /// Runs `f` at the back-reference target, then returns to the current
    /// position. Back-references are not followed while skipping.
    fn with_backref<T: Default>(&mut self, f: impl FnOnce(&mut Self) -> R<T>) -> R<T> {
        let target = self.backref()?;
        if self.skipping != 0 {
            return Some(T::default());
        }
        let saved = self.next;
        self.next = target;
        self.enter()?;
        let result = f(self);
        self.leave();
        self.next = saved;
        result
    }

    fn path(&mut self, in_value: bool) -> R<()> {
        self.enter()?;
        let result = self.path_inner(in_value);
        self.leave();
        result
    }

    fn path_inner(&mut self, in_value: bool) -> R<()> {
        let tag = self.next()?;
        match tag {
            b'C' => {
                let dis = self.disambiguator()?;
                let name = self.ident()?;
                self.print_ident(name);
                if self.verbose {
                    self.print("[");
                    self.print_hex(dis);
                    self.print("]");
                }
            }
            b'N' => {
                let ns = self.next()?;
                if !ns.is_ascii_alphabetic() {
                    return None;
                }
                self.path(in_value)?;
                let dis = self.disambiguator()?;
                let name = self.ident()?;
                if ns.is_ascii_uppercase() {
                    self.print("::{");
                    match ns {
                        b'C' => self.print("closure"),
                        b'S' => self.print("shim"),
                        _ => {
                            let text = [ns];
                            self.print(std::str::from_utf8(&text).unwrap_or("?"));
                        }
                    }
                    if !name.is_empty() {
                        self.print(":");
                        self.print_ident(name);
                    }
                    self.print("#");
                    self.print_u64(dis);
                    self.print("}");
                } else if !name.is_empty() {
                    self.print("::");
                    self.print_ident(name);
                }
            }
            b'M' | b'X' | b'Y' => {
                if tag != b'Y' {
                    self.disambiguator()?;
                    self.skipping = self.skipping.saturating_add(1);
                    let skipped = self.path(false);
                    self.skipping = self.skipping.saturating_sub(1);
                    skipped?;
                }
                self.print("<");
                self.ty()?;
                if tag != b'M' {
                    self.print(" as ");
                    self.path(false)?;
                }
                self.print(">");
            }
            b'I' => {
                self.path(in_value)?;
                if in_value {
                    self.print("::");
                }
                self.print("<");
                self.sep_list(", ", Self::generic_arg)?;
                self.print(">");
            }
            b'B' => self.with_backref(|p| p.path(in_value))?,
            _ => return None,
        }
        Some(())
    }

    /// Items up to `E`, separated by `sep`. Returns the count.
    fn sep_list(&mut self, sep: &str, mut f: impl FnMut(&mut Self) -> R<()>) -> R<usize> {
        let mut count = 0usize;
        while !self.eat(b'E') {
            if count > 0 {
                self.print(sep);
            }
            f(self)?;
            count = count.saturating_add(1);
        }
        Some(count)
    }

    fn generic_arg(&mut self) -> R<()> {
        if self.eat(b'L') {
            let lifetime = self.integer_62()?;
            self.lifetime(lifetime)
        } else if self.eat(b'K') {
            self.constant(false)
        } else {
            self.ty()
        }
    }

    fn lifetime(&mut self, index: u64) -> R<()> {
        self.print("'");
        if index == 0 {
            self.print("_");
            return Some(());
        }
        if self.skipping != 0 {
            return Some(());
        }
        let depth = self.bound_lifetimes.checked_sub(index)?;
        if depth < 26 {
            let c = [b'a'.wrapping_add(u8::try_from(depth).ok()?)];
            self.print(std::str::from_utf8(&c).unwrap_or("?"));
        } else {
            self.print("_");
            self.print_u64(depth);
        }
        Some(())
    }

    /// `[G <base-62-number>]` bound lifetimes: prints `for<'a, 'b> ` and
    /// returns how many were bound.
    fn binder(&mut self) -> R<u64> {
        let count = self.opt_integer_62(b'G')?;
        // Each bound lifetime prints something, so the count is bounded by the
        // output size even when nothing is printed.
        if count > u64::try_from(MAX_OUTPUT).unwrap_or(u64::MAX) {
            return None;
        }
        if count > 0 {
            self.print("for<");
            for i in 0..count {
                if !self.out.step() {
                    return None;
                }
                if i > 0 {
                    self.print(", ");
                }
                self.bound_lifetimes = self.bound_lifetimes.checked_add(1)?;
                self.lifetime(1)?;
            }
            self.print("> ");
        }
        Some(count)
    }

    fn ty(&mut self) -> R<()> {
        let tag = self.next()?;
        if let Some(basic) = basic_type(tag) {
            self.print(basic);
            return Some(());
        }
        self.enter()?;
        let result = self.ty_inner(tag);
        self.leave();
        result
    }

    fn ty_inner(&mut self, tag: u8) -> R<()> {
        match tag {
            b'R' | b'Q' => {
                self.print("&");
                if self.eat(b'L') {
                    let lifetime = self.integer_62()?;
                    if lifetime != 0 {
                        self.lifetime(lifetime)?;
                        self.print(" ");
                    }
                }
                if tag == b'Q' {
                    self.print("mut ");
                }
                self.ty()?;
            }
            b'P' => {
                self.print("*const ");
                self.ty()?;
            }
            b'O' => {
                self.print("*mut ");
                self.ty()?;
            }
            b'A' | b'S' => {
                self.print("[");
                self.ty()?;
                if tag == b'A' {
                    self.print("; ");
                    self.constant(true)?;
                }
                self.print("]");
            }
            b'T' => {
                self.print("(");
                let count = self.sep_list(", ", Self::ty)?;
                if count == 1 {
                    self.print(",");
                }
                self.print(")");
            }
            b'F' => {
                let saved = self.bound_lifetimes;
                let result = self.fn_sig();
                self.bound_lifetimes = saved;
                result?;
            }
            b'D' => {
                self.print("dyn ");
                let saved = self.bound_lifetimes;
                let result = self.binder().and_then(|_| {
                    self.sep_list(" + ", Self::dyn_trait)?;
                    Some(())
                });
                self.bound_lifetimes = saved;
                result?;
                if !self.eat(b'L') {
                    return None;
                }
                let lifetime = self.integer_62()?;
                if lifetime != 0 {
                    self.print(" + ");
                    self.lifetime(lifetime)?;
                }
            }
            b'B' => self.with_backref(Self::ty)?,
            _ => {
                self.next = self.next.checked_sub(1)?;
                self.path(false)?;
            }
        }
        Some(())
    }

    fn fn_sig(&mut self) -> R<()> {
        self.binder()?;
        if self.eat(b'U') {
            self.print("unsafe ");
        }
        if self.eat(b'K') {
            let abi: &[u8] = if self.eat(b'C') {
                b"C"
            } else {
                let ident = self.ident()?;
                if ident.ascii.is_empty() || !ident.punycode.is_empty() {
                    return None;
                }
                ident.ascii
            };
            self.print("extern \"");
            for (i, part) in abi.split(|&c| c == b'_').enumerate() {
                if i > 0 {
                    self.print("-");
                }
                if self.skipping == 0 {
                    self.out.push_bytes(part);
                }
            }
            self.print("\" ");
        }
        self.print("fn(");
        self.sep_list(", ", Self::ty)?;
        self.print(")");
        if !self.eat(b'u') {
            self.print(" -> ");
            self.ty()?;
        }
        Some(())
    }

    fn dyn_trait(&mut self) -> R<()> {
        let mut open = self.path_maybe_open_generics()?;
        while self.eat(b'p') {
            self.print(if open { ", " } else { "<" });
            open = true;
            let name = self.ident()?;
            self.print_ident(name);
            self.print(" = ");
            self.ty()?;
        }
        if open {
            self.print(">");
        }
        Some(())
    }

    fn path_maybe_open_generics(&mut self) -> R<bool> {
        self.enter()?;
        let result = if self.eat(b'B') {
            self.with_backref(Self::path_maybe_open_generics)
        } else if self.eat(b'I') {
            self.path(false).and_then(|()| {
                self.print("<");
                self.sep_list(", ", Self::generic_arg)?;
                Some(true)
            })
        } else {
            self.path(false).map(|()| false)
        };
        self.leave();
        result
    }

    fn constant(&mut self, in_value: bool) -> R<()> {
        self.enter()?;
        let result = self.constant_inner(in_value);
        self.leave();
        result
    }

    fn constant_inner(&mut self, in_value: bool) -> R<()> {
        let tag = self.next()?;
        let mut braced = false;
        let mut open_brace = |p: &mut Self| {
            if !in_value {
                braced = true;
                p.print("{");
            }
        };
        match tag {
            b'p' => self.print("_"),
            b'h' | b't' | b'm' | b'y' | b'o' | b'j' => {
                self.const_uint()?;
                self.const_type(tag);
            }
            b'a' | b's' | b'l' | b'x' | b'n' | b'i' => {
                if self.eat(b'n') {
                    self.print("-");
                }
                self.const_uint()?;
                self.const_type(tag);
            }
            b'b' => {
                match self.hex_nibbles()? {
                    b"0" => self.print("false"),
                    b"1" => self.print("true"),
                    _ => return None,
                }
                self.const_type(tag);
            }
            b'c' => {
                let hex = self.hex_nibbles()?;
                if hex.is_empty() || hex.len() > 8 {
                    return None;
                }
                let value = parse_hex(hex)?;
                let c = char::from_u32(u32::try_from(value).ok()?)?;
                self.print_char_literal(c);
                self.const_type(tag);
            }
            b'e' => {
                open_brace(self);
                self.print("*");
                self.const_str()?;
            }
            b'R' | b'Q' => {
                if tag == b'R' && self.eat(b'e') {
                    self.const_str()?;
                } else {
                    open_brace(self);
                    self.print("&");
                    if tag == b'Q' {
                        self.print("mut ");
                    }
                    self.nested_constant()?;
                }
            }
            b'A' => {
                open_brace(self);
                self.print("[");
                self.sep_list(", ", |p| p.nested_constant())?;
                self.print("]");
            }
            b'T' => {
                open_brace(self);
                self.print("(");
                let count = self.sep_list(", ", |p| p.nested_constant())?;
                if count == 1 {
                    self.print(",");
                }
                self.print(")");
            }
            b'V' => {
                open_brace(self);
                self.path(true)?;
                match self.next()? {
                    b'U' => {}
                    b'T' => {
                        self.print("(");
                        self.sep_list(", ", |p| p.nested_constant())?;
                        self.print(")");
                    }
                    b'S' => {
                        self.print(" { ");
                        self.sep_list(", ", |p| {
                            p.disambiguator()?;
                            let name = p.ident()?;
                            p.print_ident(name);
                            p.print(": ");
                            p.nested_constant()
                        })?;
                        self.print(" }");
                    }
                    _ => return None,
                }
            }
            b'B' => self.with_backref(|p| p.constant(in_value))?,
            _ => return None,
        }
        if braced {
            self.print("}");
        }
        Some(())
    }

    fn const_uint(&mut self) -> R<()> {
        let hex = self.hex_nibbles()?;
        match parse_hex(hex) {
            Some(value) if hex.len() <= 16 => self.print_u64(value),
            _ => {
                self.print("0x");
                if self.skipping == 0 {
                    self.out.push_bytes(hex);
                }
            }
        }
        Some(())
    }

    /// A constant inside a constant value (array element, field).
    fn nested_constant(&mut self) -> R<()> {
        self.in_const_value = self.in_const_value.saturating_add(1);
        let result = self.constant(true);
        self.in_const_value = self.in_const_value.saturating_sub(1);
        result
    }

    /// `: type` after a constant, in verbose mode (as `c++filt` prints it),
    /// except inside constant values, which `c++filt` does not support.
    fn const_type(&mut self, tag: u8) {
        if self.verbose
            && self.in_const_value == 0
            && let Some(ty) = basic_type(tag)
        {
            self.print(": ");
            self.print(ty);
        }
    }

    /// A string constant: hex-encoded UTF-8 bytes up to `_`.
    fn const_str(&mut self) -> R<()> {
        let hex = self.hex_nibbles()?;
        if hex.len() % 2 != 0 {
            return None;
        }
        let bytes: Option<Vec<u8>> = hex
            .chunks(2)
            .map(|pair| u8::try_from(parse_hex(pair)?).ok())
            .collect();
        let text = String::from_utf8(bytes?).ok()?;
        if self.skipping == 0 {
            self.out.push("\"");
            for c in text.chars() {
                match c {
                    '"' => self.out.push("\\\""),
                    '\'' => self.out.push("'"),
                    c => self.out.push(&c.escape_debug().to_string()),
                }
            }
            self.out.push("\"");
        }
        Some(())
    }

    fn print_char_literal(&mut self, c: char) {
        if self.skipping != 0 {
            return;
        }
        if self.verbose {
            // `c++filt` prints only plain ASCII verbatim.
            self.out.push("'");
            match c {
                '\t' => self.out.push("\\t"),
                '\r' => self.out.push("\\r"),
                '\n' => self.out.push("\\n"),
                c if c > ' ' && c < '~' => self.out.push_char(c),
                c => {
                    self.out.push("\\u{");
                    self.out.push(&format!("{:x}", u32::from(c)));
                    self.out.push("}");
                }
            }
            self.out.push("'");
            return;
        }
        self.out.push("'");
        match c {
            '\'' => self.out.push("\\'"),
            '"' => self.out.push("\""),
            c => self.out.push(&c.escape_debug().to_string()),
        }
        self.out.push("'");
    }
}

fn parse_hex(hex: &[u8]) -> Option<u64> {
    let mut value: u64 = 0;
    for &c in hex {
        let digit = match c {
            b'0'..=b'9' => c.wrapping_sub(b'0'),
            b'a'..=b'f' => c.wrapping_sub(b'a').wrapping_add(10),
            _ => return None,
        };
        value = value.checked_mul(16)?.checked_add(u64::from(digit))?;
    }
    Some(value)
}

/// Decodes RFC 3492 Punycode with `ascii` as the basic code points.
fn punycode_decode(ascii: &[u8], punycode: &[u8]) -> Option<String> {
    const BASE: u64 = 36;
    const T_MIN: u64 = 1;
    const T_MAX: u64 = 26;
    const SKEW: u64 = 38;
    const MAX_CHARS: usize = 4096;

    let mut output: Vec<char> = ascii.iter().map(|&c| char::from(c)).collect();
    let mut n: u64 = 0x80;
    let mut i: u64 = 0;
    let mut bias: u64 = 72;
    let mut damp: u64 = 700;
    let mut input = punycode.iter();
    let mut remaining = punycode.len();
    while remaining > 0 {
        let old_i = i;
        let mut w: u64 = 1;
        let mut k = BASE;
        loop {
            let &c = input.next()?;
            remaining = remaining.checked_sub(1)?;
            let digit = match c {
                b'a'..=b'z' => u64::from(c.wrapping_sub(b'a')),
                b'0'..=b'9' => u64::from(c.wrapping_sub(b'0')).checked_add(26)?,
                _ => return None,
            };
            i = i.checked_add(digit.checked_mul(w)?)?;
            let t = if k <= bias {
                T_MIN
            } else if k >= bias.checked_add(T_MAX)? {
                T_MAX
            } else {
                k.checked_sub(bias)?
            };
            if digit < t {
                break;
            }
            w = w.checked_mul(BASE.checked_sub(t)?)?;
            k = k.checked_add(BASE)?;
        }
        let len = u64::try_from(output.len()).ok()?.checked_add(1)?;
        if output.len() >= MAX_CHARS {
            return None;
        }
        // Bias adaptation.
        let mut delta = i.checked_sub(old_i)?.checked_div(damp)?;
        damp = 2;
        delta = delta.checked_add(delta.checked_div(len)?)?;
        let mut k = 0u64;
        // The constants: (BASE - T_MIN) * T_MAX / 2 = 455, BASE - T_MIN = 35,
        // BASE - T_MIN + 1 = 36.
        while delta > 455 {
            delta = delta.checked_div(35)?;
            k = k.checked_add(BASE)?;
        }
        bias = k.checked_add(36u64.checked_mul(delta)?.checked_div(delta.checked_add(SKEW)?)?)?;

        n = n.checked_add(i.checked_div(len)?)?;
        i = i.checked_rem(len)?;
        let c = char::from_u32(u32::try_from(n).ok()?)?;
        output.insert(usize::try_from(i).ok()?, c);
        i = i.checked_add(1)?;
    }
    Some(output.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(name: &str, verbose: bool) -> Option<String> {
        let mut out = Output::for_input(name.len());
        demangle(name.as_bytes(), verbose, &mut out)?;
        out.finish()
    }

    #[test]
    fn crate_disambiguators() {
        assert_eq!(
            run("_RNvCs1234_7mycrate3foo", true).as_deref(),
            Some("mycrate[3c1c0]::foo")
        );
        assert_eq!(
            run("_RNvCs1234_7mycrate3foo", false).as_deref(),
            Some("mycrate::foo")
        );
    }

    #[test]
    fn generics_and_backrefs() {
        let name = "_RINvNtCs3XFJfFEDSOQ_4core3ptr13drop_in_placeNtNtCs5MFCHAZFjYk_12regex_syntax3hir5ClassEBK_";
        assert_eq!(
            run(name, true).as_deref(),
            Some(
                "core[2e27404414be4892]::ptr::drop_in_place::<regex_syntax[4361b8f2a39a11b2]::hir::Class>"
            )
        );
        assert_eq!(
            run(name, false).as_deref(),
            Some("core::ptr::drop_in_place::<regex_syntax::hir::Class>")
        );
    }

    #[test]
    fn punycode() {
        // "gödel" from the rustc-demangle test suite.
        assert_eq!(punycode_decode(b"gdel", b"5qa").as_deref(), Some("gödel"));
        assert_eq!(
            run("_RNvCs1234_7mycrateu8gdel_5qa", false).as_deref(),
            Some("mycrate::gödel")
        );
    }

    #[test]
    fn rejects_forward_backrefs() {
        assert_eq!(run("_RNvB0_3foo", false), None);
        assert_eq!(run("_RB_", false), None);
    }
}
