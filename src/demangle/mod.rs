//! Symbol name demangling for diagnostics.
//!
//! **Workstream W13.** Itanium C++ ABI names (`_ZN3foo3barEv`), Rust legacy
//! (`_ZN...17h<hash>E`) and Rust v0 (`_R...`) mangling, rendered the way
//! `c++filt` and `rustfilt` show them. Used only when printing diagnostics,
//! map files and `--print-*` output; never on hot paths. Malformed names are
//! returned unchanged, never a panic.
//!
//! - [`demangle`] returns the demangled name, or the input unchanged (lossily
//!   converted to UTF-8) when it is not a mangled name.
//! - [`try_demangle`] returns `None` instead.
//! - [`Options`] selects whether Rust hashes and crate disambiguators are
//!   shown: hidden by default, as `rustfilt` does, and shown with
//!   [`Options::verbose`], which reproduces `c++filt` exactly.
//!
//! # Compatibility
//!
//! Itanium output matches GNU `c++filt` byte for byte on the names it
//! accepts (checked by the `#[ignore]`d sweep in `tests/demangle.rs` against
//! every symbol of the host's libraries). Rust output follows
//! `rustc-demangle`: in verbose mode it matches `c++filt`, which prints the
//! legacy hash, crate disambiguators and constant types. Names `c++filt`
//! rejects but that are unambiguous (Mach-O's `__Z` prefix, clone suffixes
//! after data names, `_GLOBAL__sub_I_` initializers, v0 constants of struct
//! and array type) are demangled too.
//!
//! # Robustness
//!
//! The input is untrusted. Parsing is bounded by [`MAX_INPUT`] bytes of
//! input, [`MAX_DEPTH`] levels of recursion and [`MAX_NODES`] syntax tree
//! nodes; printing by [`MAX_OUTPUT`] bytes of output and a step budget,
//! because substitutions and back-references let a short name expand
//! exponentially. Exceeding any limit makes the name "not demangleable".

#![deny(clippy::arithmetic_side_effects)]

mod itanium;
mod output;
mod rust_legacy;
mod rust_v0;

use std::borrow::Cow;

use output::Output;

/// Longest input accepted, in bytes.
pub const MAX_INPUT: usize = 1 << 16;
/// Deepest recursion while parsing or printing. Real names stay well below:
/// the libraries of a full Linux distribution need 64 levels for C++ and 128
/// for Rust v0.
pub const MAX_DEPTH: usize = 192;
/// Most syntax tree nodes built for one name.
pub const MAX_NODES: usize = 1 << 17;
/// Longest output produced, in bytes. The longest name in a sweep of a full
/// Linux distribution's libraries demangles to 24 KiB.
pub const MAX_OUTPUT: usize = 1 << 18;
/// Printing steps allowed for one name.
pub(crate) const PRINT_FUEL: usize = 1 << 20;
/// Parsing steps allowed for one name.
pub(crate) const PARSE_FUEL: usize = 1 << 18;

/// How demangled names are rendered.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Options {
    /// Show Rust legacy hashes (`::h0123456789abcdef`), v0 crate
    /// disambiguators (`core[2e27404414be4892]`) and v0 constant types
    /// (`5: usize`). With this set, the output is exactly `c++filt`'s.
    pub verbose: bool,
}

impl Options {
    /// `rustfilt`-style output: Rust hashes hidden.
    #[must_use]
    pub const fn new() -> Self {
        Self { verbose: false }
    }

    /// `c++filt`-style output: Rust hashes shown.
    #[must_use]
    pub const fn verbose() -> Self {
        Self { verbose: true }
    }
}

/// A mangling scheme.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Scheme {
    /// Itanium C++ ABI (`_Z`), used by GCC and Clang on ELF and Mach-O.
    Itanium,
    /// Rust's legacy mangling (`_ZN...17h<hash>E`).
    RustLegacy,
    /// Rust's v0 mangling (`_R`).
    RustV0,
}

/// Demangles `name`, returning it unchanged (converted to UTF-8, lossily)
/// if it is not a mangled name. Rust hashes are hidden.
#[must_use]
pub fn demangle(name: &[u8]) -> Cow<'_, str> {
    demangle_with(name, Options::new())
}

/// Demangles `name` with `options`, returning it unchanged (converted to
/// UTF-8, lossily) if it is not a mangled name.
#[must_use]
pub fn demangle_with(name: &[u8], options: Options) -> Cow<'_, str> {
    match try_demangle_with(name, options) {
        Some(text) => Cow::Owned(text),
        None => String::from_utf8_lossy(name),
    }
}

/// Demangles `name`, or returns `None` if it is not a (valid) mangled name.
/// Rust hashes are hidden.
#[must_use]
pub fn try_demangle(name: &[u8]) -> Option<String> {
    try_demangle_with(name, Options::new())
}

/// Demangles `name` with `options`, or returns `None` if it is not a
/// (valid) mangled name.
#[must_use]
pub fn try_demangle_with(name: &[u8], options: Options) -> Option<String> {
    try_demangle_scheme(name, options).map(|(text, _)| text)
}

/// Demangles `name` with `options` and reports which scheme it used.
#[must_use]
pub fn try_demangle_scheme(name: &[u8], options: Options) -> Option<(String, Scheme)> {
    if name.len() > MAX_INPUT {
        return None;
    }
    if name.starts_with(b"_R") || name.starts_with(b"__R") {
        let mut out = Output::for_input(name.len());
        rust_v0::demangle(name, options.verbose, &mut out)?;
        return out.finish().map(|text| (text, Scheme::RustV0));
    }
    let mut out = Output::for_input(name.len());
    if rust_legacy::demangle(name, options.verbose, &mut out).is_some() {
        if let Some(text) = out.finish() {
            return Some((text, Scheme::RustLegacy));
        }
        return None;
    }
    itanium::demangle(name).map(|text| (text, Scheme::Itanium))
}

/// A demangled name split into the parts that tell similar names apart.
///
/// For `_ZNK2ns3Foo3barIiEEvv` (`void ns::Foo::bar<int>() const`): `name` is
/// `ns::Foo::bar<int>`, `scope` is `ns::Foo`, `base` is `bar`, `params` is
/// `()` and `qualifiers` is `const`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Parts {
    /// The mangling scheme.
    pub scheme: Scheme,
    /// The complete demangled name, as [`try_demangle_with`] returns it.
    pub full: String,
    /// The entity's qualified name, with template arguments, without return
    /// type, parameters, member function qualifiers or clone suffixes. For
    /// special names (`vtable for X`), the whole name.
    pub name: String,
    /// The scope `name` is declared in; empty at global scope.
    pub scope: String,
    /// The innermost name without template arguments.
    pub base: String,
    /// The parameter list with its parentheses, for functions whose
    /// mangling has one (C++ only).
    pub params: Option<String>,
    /// Member function qualifiers (`const`, `volatile`, `&`, `&&`), separated
    /// by spaces.
    pub qualifiers: String,
}

/// Demangles `name` and splits it into [`Parts`], or returns `None` if it is
/// not a (valid) mangled name.
#[must_use]
pub fn parts(name: &[u8], options: Options) -> Option<Parts> {
    if name.len() > MAX_INPUT {
        return None;
    }
    let (full, scheme) = try_demangle_scheme(name, options)?;
    if scheme == Scheme::Itanium {
        let pieces = itanium::pieces(name)?;
        return Some(Parts {
            scheme,
            full: pieces.full,
            name: pieces.name,
            scope: pieces.scope,
            base: pieces.base,
            params: pieces.params,
            qualifiers: pieces.qualifiers,
        });
    }
    // Rust paths: split at the last top-level `::`, and drop generic
    // arguments from the last segment.
    let segments = split_path(&full);
    let (last, scope) = segments.split_last()?;
    let base = last
        .find('<')
        .map_or(*last, |at| last.get(..at).unwrap_or(last))
        .trim_end_matches("::")
        .to_string();
    Some(Parts {
        scheme,
        name: full.clone(),
        scope: scope.join("::"),
        base,
        full,
        params: None,
        qualifiers: String::new(),
    })
}

/// Splits a Rust path at `::` separators outside of `<>`, `()`, `[]` and
/// `{}`; a `::<` generic argument list stays with its segment.
fn split_path(path: &str) -> Vec<&str> {
    let mut segments = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    let bytes = path.as_bytes();
    let mut i = 0usize;
    while let Some(&c) = bytes.get(i) {
        match c {
            b'<' | b'(' | b'[' | b'{' => depth = depth.saturating_add(1),
            b'>' | b')' | b']' | b'}' => depth = depth.saturating_sub(1),
            b':' if depth == 0
                && bytes.get(i.saturating_add(1)) == Some(&b':')
                && bytes.get(i.saturating_add(2)) != Some(&b'<') =>
            {
                segments.push(path.get(start..i).unwrap_or_default());
                i = i.saturating_add(2);
                start = i;
                continue;
            }
            _ => {}
        }
        i = i.saturating_add(1);
    }
    segments.push(path.get(start..).unwrap_or_default());
    segments
}

/// Guesses the scheme of `name` from its prefix, without validating it.
#[must_use]
pub fn scheme(name: &[u8]) -> Option<Scheme> {
    let name = name
        .strip_prefix(b"_")
        .filter(|n| n.starts_with(b"_"))
        .unwrap_or(name);
    if name.starts_with(b"_R") {
        Some(Scheme::RustV0)
    } else if name.starts_with(b"_ZN") && is_legacy_rust_shape(name) {
        Some(Scheme::RustLegacy)
    } else if name.starts_with(b"_Z") || name.starts_with(b"_GLOBAL_") {
        Some(Scheme::Itanium)
    } else {
        None
    }
}

fn is_legacy_rust_shape(name: &[u8]) -> bool {
    let end = name.iter().rposition(|&c| c == b'E').unwrap_or(name.len());
    name.get(..end)
        .and_then(|body| body.len().checked_sub(19).and_then(|at| body.get(at..)))
        .is_some_and(|tail| tail.starts_with(b"17h"))
}

/// Whether `name` looks like a mangled name of any supported scheme.
#[must_use]
pub fn is_mangled(name: &[u8]) -> bool {
    scheme(name).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_basics() {
        assert_eq!(demangle(b"_ZN3foo3barEv"), "foo::bar()");
        assert_eq!(demangle(b"main"), "main");
        assert_eq!(demangle(b"_Z"), "_Z");
        assert_eq!(try_demangle(b"memcpy"), None);
        assert_eq!(demangle(b"\xff_Zx"), "\u{fffd}_Zx");
        assert_eq!(scheme(b"_R3foo"), Some(Scheme::RustV0));
        assert_eq!(
            scheme(b"_ZN3foo17h0123456789abcdefE"),
            Some(Scheme::RustLegacy)
        );
        assert_eq!(scheme(b"_ZN3fooE"), Some(Scheme::Itanium));
        assert!(!is_mangled(b"foo"));
    }

    #[test]
    fn parts_of_cpp_names() {
        let p = parts(b"_ZNK2ns3Foo3barIiEEvv", Options::new()).unwrap();
        assert_eq!(p.full, "void ns::Foo::bar<int>() const");
        assert_eq!(p.name, "ns::Foo::bar<int>");
        assert_eq!(p.scope, "ns::Foo");
        assert_eq!(p.base, "bar");
        assert_eq!(p.params.as_deref(), Some("()"));
        assert_eq!(p.qualifiers, "const");

        let p = parts(b"_ZN1AIiE1fIcEET_", Options::new());
        assert!(p.is_none(), "{p:?}");
        let p = parts(b"_ZN1AIiE1fIcEEvT_", Options::new()).unwrap();
        assert_eq!(p.params.as_deref(), Some("(char)"));
        assert_eq!(p.scope, "A<int>");

        let p = parts(b"_Z3fooi", Options::new()).unwrap();
        assert_eq!(
            (p.name.as_str(), p.scope.as_str(), p.base.as_str()),
            ("foo", "", "foo")
        );
        assert_eq!(p.params.as_deref(), Some("(int)"));

        let p = parts(b"_ZZ4mainENKUlvE_clEv", Options::new()).unwrap();
        assert_eq!(p.name, "main::{lambda()#1}::operator()");
        assert_eq!(p.scope, "main::{lambda()#1}");
        let p = parts(b"_ZZ1fvENKUlvE_clEv", Options::new()).unwrap();
        assert_eq!(p.scope, "f()::{lambda()#1}");
        assert_eq!(p.base, "operator()");
        assert_eq!(p.qualifiers, "const");

        let p = parts(b"_ZNO1S1gEv", Options::new()).unwrap();
        assert_eq!(p.qualifiers, "&&");
        let p = parts(b"_ZN2ns1xE", Options::new()).unwrap();
        assert_eq!(
            (p.scope.as_str(), p.base.as_str(), p.params),
            ("ns", "x", None)
        );
        let p = parts(b"_ZTVN2ns1AE", Options::new()).unwrap();
        assert_eq!(p.name, "vtable for ns::A");
        let p = parts(b"_ZN2ns1AC2Ev", Options::new()).unwrap();
        assert_eq!((p.scope.as_str(), p.base.as_str()), ("ns::A", "A"));
    }

    #[test]
    fn parts_of_rust_names() {
        let p = parts(
            b"_ZN4core3ptr13drop_in_place17h0123456789abcdefE",
            Options::new(),
        )
        .unwrap();
        assert_eq!(
            (p.scope.as_str(), p.base.as_str()),
            ("core::ptr", "drop_in_place")
        );
        let p = parts(
            b"_RINvNtCs3XFJfFEDSOQ_4core3ptr13drop_in_placeNtNtCs5MFCHAZFjYk_12regex_syntax3hir5ClassEBK_",
            Options::new(),
        )
        .unwrap();
        assert_eq!(p.base, "drop_in_place");
        assert_eq!(p.scope, "core::ptr");
    }

    #[test]
    fn rust_hash_option() {
        let name = b"_ZN4core3fmt5write17h0123456789abcdefE";
        assert_eq!(demangle(name), "core::fmt::write");
        assert_eq!(
            demangle_with(name, Options::verbose()),
            "core::fmt::write::h0123456789abcdef"
        );
    }
}
