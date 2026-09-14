//! Rust legacy mangling: `_ZN <ident>+ 17h<16 hex digits> E`.
//!
//! These names are well-formed Itanium names, so they are recognized by
//! their final path segment: a hash of `h` and 16 lowercase hex digits (with
//! at least 5 distinct digits, the heuristic `c++filt` uses to tell a hash
//! from a C++ identifier). Identifiers escape punctuation as `$LT$`, `$u20$`
//! and friends, and `..` stands for `::`.
//!
//! By default the hash is hidden, as `rustfilt` does; with
//! [`Options::verbose`](super::Options::verbose) it is printed as the
//! last path segment, as `c++filt` does. Trailing `.suffix` parts (such as
//! `.llvm.123`) are dropped.

use super::output::Output;

/// Demangles a legacy Rust name, or returns `None`.
pub(crate) fn demangle(name: &[u8], show_hash: bool, out: &mut Output) -> Option<()> {
    let body = name
        .strip_prefix(b"_ZN")
        .or_else(|| name.strip_prefix(b"__ZN"))?;
    if !body
        .iter()
        .all(|&c| c == b'_' || c.is_ascii_alphanumeric() || matches!(c, b'$' | b'.' | b':' | b'@'))
    {
        return None;
    }
    // Drop `.suffix` parts after the closing `E`.
    let mut len = body.len();
    let mut after_dot = true;
    while len > 0 {
        let last = *body.get(len.checked_sub(1)?)?;
        if after_dot && last == b'E' {
            break;
        }
        after_dot = last == b'.';
        len = len.checked_sub(1)?;
    }
    let body = body.get(..len.checked_sub(1)?)?;
    if body.len() <= 19 || body.get(body.len().checked_sub(19)?..)?.get(..3)? != b"17h" {
        return None;
    }

    let mut segments = Vec::new();
    let mut rest = body;
    while !rest.is_empty() {
        let (segment, next) = identifier(rest)?;
        segments.push(segment);
        rest = next;
    }
    let (&hash, path) = segments.split_last()?;
    if !is_hash(hash) {
        return None;
    }
    let shown: &[&[u8]] = if show_hash { &segments } else { path };
    for (i, segment) in shown.iter().enumerate() {
        if i > 0 {
            out.push("::");
        }
        print_segment(segment, out);
    }
    Some(())
}

/// `<decimal length> <bytes>`; a leading `0` means length 0.
fn identifier(input: &[u8]) -> Option<(&[u8], &[u8])> {
    let first = *input.first()?;
    if !first.is_ascii_digit() {
        return None;
    }
    let mut len = usize::from(first.wrapping_sub(b'0'));
    let mut at = 1usize;
    if first != b'0' {
        while let Some(&c) = input.get(at).filter(|c| c.is_ascii_digit()) {
            len = len
                .checked_mul(10)?
                .checked_add(usize::from(c.wrapping_sub(b'0')))?;
            at = at.checked_add(1)?;
        }
    }
    let end = at.checked_add(len)?;
    Some((input.get(at..end)?, input.get(end..)?))
}

/// `h` and 16 lowercase hex digits, at least 5 of them distinct.
fn is_hash(segment: &[u8]) -> bool {
    let Some(digits) = segment.strip_prefix(b"h") else {
        return false;
    };
    if digits.len() != 16 {
        return false;
    }
    let mut seen = 0u16;
    for &c in digits {
        let Some(nibble) = lower_hex(c) else {
            return false;
        };
        seen |= 1u16 << nibble;
    }
    seen.count_ones() >= 5
}

fn lower_hex(c: u8) -> Option<u32> {
    match c {
        b'0'..=b'9' => Some(u32::from(c.wrapping_sub(b'0'))),
        b'a'..=b'f' => Some(u32::from(c.wrapping_sub(b'a')).wrapping_add(10)),
        _ => None,
    }
}

/// Prints a path segment, decoding its escapes.
fn print_segment(segment: &[u8], out: &mut Output) {
    let mut rest = segment;
    // The mangler adds `_` before an escape at the start of an identifier.
    if rest.starts_with(b"_$") {
        rest = rest.get(1..).unwrap_or_default();
    }
    while let Some(&first) = rest.first() {
        match first {
            b'$' => match escape(rest) {
                Some((c, len)) => {
                    out.push_char(c);
                    rest = rest.get(len..).unwrap_or_default();
                }
                None => {
                    out.push_bytes(rest);
                    return;
                }
            },
            b'.' => {
                if rest.get(1) == Some(&b'.') {
                    out.push("::");
                    rest = rest.get(2..).unwrap_or_default();
                } else {
                    out.push(".");
                    rest = rest.get(1..).unwrap_or_default();
                }
            }
            _ => {
                let len = rest
                    .iter()
                    .position(|&c| c == b'$' || c == b'.')
                    .unwrap_or(rest.len());
                out.push_bytes(rest.get(..len).unwrap_or_default());
                rest = rest.get(len..).unwrap_or_default();
            }
        }
    }
}

/// Decodes `$..$` at the start of `input`: the character and the escape's
/// length.
fn escape(input: &[u8]) -> Option<(char, usize)> {
    let inner = input.get(1..)?;
    let end = inner.iter().position(|&c| c == b'$')?;
    let code = inner.get(..end)?;
    let c = match code {
        b"SP" => '@',
        b"BP" => '*',
        b"RF" => '&',
        b"LT" => '<',
        b"GT" => '>',
        b"LP" => '(',
        b"RP" => ')',
        b"C" => ',',
        _ => {
            let digits = code.strip_prefix(b"u")?;
            if digits.is_empty() || digits.len() > 6 {
                return None;
            }
            let mut value = 0u32;
            for &d in digits {
                value = value.checked_mul(16)?.checked_add(lower_hex(d)?)?;
            }
            let c = char::from_u32(value)?;
            if c.is_control() {
                return None;
            }
            c
        }
    };
    Some((c, end.checked_add(2)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(name: &str, show_hash: bool) -> Option<String> {
        let mut out = Output::for_input(name.len());
        demangle(name.as_bytes(), show_hash, &mut out)?;
        out.finish()
    }

    #[test]
    fn hides_or_shows_the_hash() {
        let name = "_ZN4core3ptr13drop_in_place17h0123456789abcdefE";
        assert_eq!(
            run(name, false).as_deref(),
            Some("core::ptr::drop_in_place")
        );
        assert_eq!(
            run(name, true).as_deref(),
            Some("core::ptr::drop_in_place::h0123456789abcdef")
        );
    }

    #[test]
    fn decodes_escapes() {
        let name = "_ZN58_$LT$alloc..string..String$u20$as$u20$core..fmt..Debug$GT$3fmt17h1234567890abcdefE";
        assert_eq!(
            run(name, false).as_deref(),
            Some("<alloc::string::String as core::fmt::Debug>::fmt")
        );
    }

    #[test]
    fn drops_suffixes_and_rejects_non_hashes() {
        let name = "_ZN3std2os4exit17hc50faa260b5e2cd7E.llvm.840048093204772023";
        assert_eq!(run(name, false).as_deref(), Some("std::os::exit"));
        assert_eq!(run("_ZN3foo17h0000000000000000E", false), None);
        assert_eq!(run("_ZN3foo3barEv", false), None);
    }
}
