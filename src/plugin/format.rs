//! Formatting of `message` callbacks: a `printf` subset.
//!
//! Plugins pass a C format string and variadic arguments. Stable Rust cannot
//! define a C-variadic function, so the host's callback receives the
//! register-passed arguments as plain machine words (see `host.rs`) and this
//! module interprets them the way `printf` would. Integer and pointer
//! conversions are supported, which covers what GCC and LLVM plugins print
//! (`%s`, `%d`, occasionally `%u`/`%x`/`%p`/`%c`). A conversion with no
//! argument left, or a floating-point conversion, is copied verbatim.

/// Where conversions take their arguments from.
pub(crate) trait Arguments {
    /// The next integer-class argument, or `None` when there are no more.
    fn next_word(&mut self) -> Option<usize>;

    /// Reads the NUL-terminated string at `address`, at most `limit` bytes.
    /// `address` is never 0.
    fn string(&mut self, address: usize, limit: usize) -> Vec<u8>;
}

/// Longest string a `%s` conversion copies.
const MAX_STRING: usize = 1 << 16;

/// Formats `format` with `args`. Invalid UTF-8 is replaced.
pub(crate) fn format_message(format: &[u8], args: &mut dyn Arguments) -> String {
    let mut out = Vec::with_capacity(format.len());
    let mut rest = format;
    while let Some(position) = rest.iter().position(|&b| b == b'%') {
        out.extend_from_slice(&rest[..position]);
        rest = &rest[position..];
        let consumed = conversion(rest, args, &mut out);
        rest = &rest[consumed..];
    }
    out.extend_from_slice(rest);
    while out.last() == Some(&b'\n') {
        out.pop();
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Length modifiers, reduced to the width they select.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Width {
    Char,
    Short,
    Int,
    Long,
}

/// Handles one conversion starting at `spec[0] == b'%'`. Returns how many
/// bytes of `spec` it used.
fn conversion(spec: &[u8], args: &mut dyn Arguments, out: &mut Vec<u8>) -> usize {
    let mut i = 1;
    let byte = |i: usize| spec.get(i).copied().unwrap_or(0);

    let mut left = false;
    let mut zero = false;
    let mut plus = false;
    let mut space = false;
    let mut alternate = false;
    loop {
        match byte(i) {
            b'-' => left = true,
            b'0' => zero = true,
            b'+' => plus = true,
            b' ' => space = true,
            b'#' => alternate = true,
            _ => break,
        }
        i += 1;
    }

    let mut width: Option<usize> = None;
    let mut missing = false;
    if byte(i) == b'*' {
        i += 1;
        match args.next_word() {
            // An `int` argument: a negative width means left alignment.
            Some(word) => {
                let value = word as u32 as i32;
                left |= value < 0;
                width = Some(value.unsigned_abs() as usize);
            }
            None => missing = true,
        }
    } else {
        width = digits(spec, &mut i);
    }

    let mut precision: Option<usize> = None;
    if byte(i) == b'.' {
        i += 1;
        if byte(i) == b'*' {
            i += 1;
            match args.next_word() {
                Some(word) => {
                    let value = word as u32 as i32;
                    precision = usize::try_from(value).ok();
                }
                None => missing = true,
            }
        } else {
            precision = Some(digits(spec, &mut i).unwrap_or(0));
        }
    }

    let mut length = Width::Int;
    match byte(i) {
        b'h' if byte(i + 1) == b'h' => {
            length = Width::Char;
            i += 2;
        }
        b'h' => {
            length = Width::Short;
            i += 1;
        }
        b'l' if byte(i + 1) == b'l' => {
            length = Width::Long;
            i += 2;
        }
        b'l' | b'j' | b'z' | b't' | b'q' => {
            length = Width::Long;
            i += 1;
        }
        b'L' => i += 1,
        _ => {}
    }

    let kind = byte(i);
    if kind == 0 {
        // A truncated conversion at the end of the string: keep it as text.
        out.extend_from_slice(spec);
        return spec.len();
    }
    i += 1;
    let verbatim = &spec[..i];

    if kind == b'%' {
        out.push(b'%');
        return i;
    }
    if missing {
        out.extend_from_slice(verbatim);
        return i;
    }

    let body: Vec<u8> = match kind {
        b'd' | b'i' => {
            let Some(word) = args.next_word() else {
                out.extend_from_slice(verbatim);
                return i;
            };
            let value = signed(word, length);
            let mut text = value.unsigned_abs().to_string().into_bytes();
            pad_precision(&mut text, precision);
            let sign: &[u8] = if value < 0 {
                b"-"
            } else if plus {
                b"+"
            } else if space {
                b" "
            } else {
                b""
            };
            return finish(
                out,
                sign,
                &text,
                width,
                left,
                zero && precision.is_none(),
                i,
            );
        }
        b'u' | b'x' | b'X' | b'o' => {
            let Some(word) = args.next_word() else {
                out.extend_from_slice(verbatim);
                return i;
            };
            let value = unsigned(word, length);
            let mut text = match kind {
                b'u' => value.to_string(),
                b'x' => format!("{value:x}"),
                b'X' => format!("{value:X}"),
                _ => format!("{value:o}"),
            }
            .into_bytes();
            pad_precision(&mut text, precision);
            let prefix: &[u8] = match kind {
                b'x' if alternate && value != 0 => b"0x",
                b'X' if alternate && value != 0 => b"0X",
                b'o' if alternate && text.first() != Some(&b'0') => b"0",
                _ => b"",
            };
            return finish(
                out,
                prefix,
                &text,
                width,
                left,
                zero && precision.is_none(),
                i,
            );
        }
        b'c' => {
            let Some(word) = args.next_word() else {
                out.extend_from_slice(verbatim);
                return i;
            };
            vec![word as u8]
        }
        b's' => {
            let Some(word) = args.next_word() else {
                out.extend_from_slice(verbatim);
                return i;
            };
            if word == 0 {
                b"(null)".to_vec()
            } else {
                args.string(word, precision.unwrap_or(MAX_STRING).min(MAX_STRING))
            }
        }
        b'p' => {
            let Some(word) = args.next_word() else {
                out.extend_from_slice(verbatim);
                return i;
            };
            if word == 0 {
                b"(nil)".to_vec()
            } else {
                format!("{word:#x}").into_bytes()
            }
        }
        _ => {
            // Floating point (`%f`, `%g`, ...), `%n` and unknown conversions:
            // their arguments are not integer-class, or not safe to honor.
            out.extend_from_slice(verbatim);
            return i;
        }
    };
    finish(out, b"", &body, width, left, false, i)
}

/// Parses a run of decimal digits, saturating.
fn digits(spec: &[u8], i: &mut usize) -> Option<usize> {
    let mut value: Option<usize> = None;
    while let Some(&b) = spec.get(*i) {
        if !b.is_ascii_digit() {
            break;
        }
        let digit = usize::from(b - b'0');
        value = Some(value.unwrap_or(0).saturating_mul(10).saturating_add(digit));
        *i += 1;
    }
    value
}

/// Widths and precisions beyond this are clamped, so a hostile format cannot
/// make the host allocate without bound.
const MAX_PADDING: usize = 4096;

fn pad_precision(text: &mut Vec<u8>, precision: Option<usize>) {
    if let Some(precision) = precision {
        let precision = precision.min(MAX_PADDING);
        if precision == 0 && text.as_slice() == b"0" {
            text.clear();
        } else if text.len() < precision {
            let mut padded = vec![b'0'; precision - text.len()];
            padded.append(text);
            *text = padded;
        }
    }
}

fn finish(
    out: &mut Vec<u8>,
    prefix: &[u8],
    body: &[u8],
    width: Option<usize>,
    left: bool,
    zero: bool,
    consumed: usize,
) -> usize {
    let len = prefix.len() + body.len();
    let pad = width.unwrap_or(0).min(MAX_PADDING).saturating_sub(len);
    if left {
        out.extend_from_slice(prefix);
        out.extend_from_slice(body);
        out.resize(out.len() + pad, b' ');
    } else if zero {
        out.extend_from_slice(prefix);
        out.resize(out.len() + pad, b'0');
        out.extend_from_slice(body);
    } else {
        out.resize(out.len() + pad, b' ');
        out.extend_from_slice(prefix);
        out.extend_from_slice(body);
    }
    consumed
}

fn signed(word: usize, length: Width) -> i64 {
    match length {
        Width::Char => i64::from(word as u8 as i8),
        Width::Short => i64::from(word as u16 as i16),
        Width::Int => i64::from(word as u32 as i32),
        Width::Long => word as u64 as i64,
    }
}

fn unsigned(word: usize, length: Width) -> u64 {
    match length {
        Width::Char => u64::from(word as u8),
        Width::Short => u64::from(word as u16),
        Width::Int => u64::from(word as u32),
        Width::Long => word as u64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Arguments from a list, with strings from a table keyed by address.
    struct List<'a> {
        words: std::slice::Iter<'a, usize>,
        strings: &'a [(usize, &'a [u8])],
    }

    impl Arguments for List<'_> {
        fn next_word(&mut self) -> Option<usize> {
            self.words.next().copied()
        }

        fn string(&mut self, address: usize, limit: usize) -> Vec<u8> {
            let text = self
                .strings
                .iter()
                .find(|(a, _)| *a == address)
                .map_or(&b"?"[..], |(_, s)| s);
            text[..text.len().min(limit)].to_vec()
        }
    }

    fn fmt(format: &str, words: &[usize]) -> String {
        let strings: &[(usize, &[u8])] = &[(0x1000, b"hello"), (0x2000, b"world")];
        format_message(
            format.as_bytes(),
            &mut List {
                words: words.iter(),
                strings,
            },
        )
    }

    #[test]
    fn strings_and_integers() {
        assert_eq!(fmt("%s: %s", &[0x1000, 0x2000]), "hello: world");
        assert_eq!(fmt("level %d of %u", &[usize::MAX, 7]), "level -1 of 7");
        assert_eq!(fmt("%5d|%-5d|%05d", &[42, 42, 42]), "   42|42   |00042");
        assert_eq!(fmt("%x %X %#x %o", &[255, 255, 255, 8]), "ff FF 0xff 10");
        assert_eq!(fmt("%lld %zu", &[usize::MAX, 3]), "-1 3");
        assert_eq!(fmt("%.3s|%.*s", &[0x1000, 2, 0x2000]), "hel|wo");
        assert_eq!(fmt("%c%c", &[b'o' as usize, b'k' as usize]), "ok");
        assert_eq!(fmt("100%%", &[]), "100%");
        assert_eq!(fmt("%s", &[0]), "(null)");
        assert_eq!(fmt("%p %p", &[0, 0x10]), "(nil) 0x10");
        assert_eq!(fmt("%.5d", &[42]), "00042");
        assert_eq!(fmt("%hhd %hd", &[0x1ff, 0x1ffff]), "-1 -1");
    }

    #[test]
    fn missing_arguments_and_odd_conversions_stay_verbatim() {
        assert_eq!(fmt("%s and %s", &[0x1000]), "hello and %s");
        assert_eq!(fmt("%f %d", &[5]), "%f 5");
        assert_eq!(fmt("%n", &[1]), "%n");
        assert_eq!(fmt("trailing %", &[]), "trailing %");
        assert_eq!(fmt("trailing %-0l", &[]), "trailing %-0l");
        assert_eq!(fmt("%*d", &[]), "%*d");
        assert_eq!(fmt("line\n\n", &[]), "line");
    }

    #[test]
    fn hostile_widths_are_clamped() {
        let text = fmt("%99999999999999999999d", &[1]);
        assert_eq!(text.len(), MAX_PADDING);
        let text = fmt("%.99999999s", &[0x1000]);
        assert_eq!(text, "hello");
    }
}
