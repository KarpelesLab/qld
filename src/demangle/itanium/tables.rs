//! Fixed tables of the Itanium mangling: builtin types, operators and
//! standard substitutions.

use super::ast::{Builtin, LiteralStyle, Operator};

const fn builtin(name: &'static str, style: LiteralStyle) -> Option<Builtin> {
    Some(Builtin { name, style })
}

/// Builtin types with one-letter codes, indexed by `code - b'a'`.
pub(super) static BUILTINS: [Option<Builtin>; 26] = [
    builtin("signed char", LiteralStyle::Default),   // a
    builtin("bool", LiteralStyle::Bool),             // b
    builtin("char", LiteralStyle::Default),          // c
    builtin("double", LiteralStyle::Float),          // d
    builtin("long double", LiteralStyle::Float),     // e
    builtin("float", LiteralStyle::Float),           // f
    builtin("__float128", LiteralStyle::Float),      // g
    builtin("unsigned char", LiteralStyle::Default), // h
    builtin("int", LiteralStyle::Int),               // i
    builtin("unsigned int", LiteralStyle::Unsigned), // j
    None,                                            // k
    builtin("long", LiteralStyle::Long),             // l
    builtin("unsigned long", LiteralStyle::UnsignedLong), // m
    builtin("__int128", LiteralStyle::Default),      // n
    builtin("unsigned __int128", LiteralStyle::Default), // o
    None,                                            // p
    None,                                            // q
    None,                                            // r
    builtin("short", LiteralStyle::Default),         // s
    builtin("unsigned short", LiteralStyle::Default), // t
    None,                                            // u (vendor type)
    builtin("void", LiteralStyle::Void),             // v
    builtin("wchar_t", LiteralStyle::Default),       // w
    builtin("long long", LiteralStyle::LongLong),    // x
    builtin("unsigned long long", LiteralStyle::UnsignedLongLong), // y
    builtin("...", LiteralStyle::Default),           // z
];

/// Builtin types with `D` codes, by second letter.
pub(super) static EXTRA_BUILTINS: [(u8, Builtin); 8] = [
    (
        b'd',
        Builtin {
            name: "decimal64",
            style: LiteralStyle::Default,
        },
    ),
    (
        b'e',
        Builtin {
            name: "decimal128",
            style: LiteralStyle::Default,
        },
    ),
    (
        b'f',
        Builtin {
            name: "decimal32",
            style: LiteralStyle::Default,
        },
    ),
    (
        b'h',
        Builtin {
            name: "half",
            style: LiteralStyle::Float,
        },
    ),
    (
        b'u',
        Builtin {
            name: "char8_t",
            style: LiteralStyle::Default,
        },
    ),
    (
        b's',
        Builtin {
            name: "char16_t",
            style: LiteralStyle::Default,
        },
    ),
    (
        b'i',
        Builtin {
            name: "char32_t",
            style: LiteralStyle::Default,
        },
    ),
    (
        b'n',
        Builtin {
            name: "decltype(nullptr)",
            style: LiteralStyle::Default,
        },
    ),
];

const fn op(code: &[u8; 2], name: &'static str, arity: u8) -> Operator {
    Operator {
        code: *code,
        name,
        arity,
    }
}

/// Operators, with their spelling and operand count in expressions.
pub(super) static OPERATORS: [Operator; 73] = [
    op(b"aN", "&=", 2),
    op(b"aS", "=", 2),
    op(b"aa", "&&", 2),
    op(b"ad", "&", 1),
    op(b"an", "&", 2),
    op(b"at", "alignof ", 1),
    op(b"aw", "co_await ", 1),
    op(b"az", "alignof ", 1),
    op(b"cc", "const_cast", 2),
    op(b"cl", "()", 2),
    op(b"cm", ",", 2),
    op(b"co", "~", 1),
    op(b"dV", "/=", 2),
    op(b"dX", "[...]=", 3),
    op(b"da", "delete[] ", 1),
    op(b"dc", "dynamic_cast", 2),
    op(b"de", "*", 1),
    op(b"di", "=", 2),
    op(b"dl", "delete ", 1),
    op(b"ds", ".*", 2),
    op(b"dt", ".", 2),
    op(b"dv", "/", 2),
    op(b"dx", "]=", 2),
    op(b"eO", "^=", 2),
    op(b"eo", "^", 2),
    op(b"eq", "==", 2),
    op(b"fL", "...", 3),
    op(b"fR", "...", 3),
    op(b"fl", "...", 2),
    op(b"fr", "...", 2),
    op(b"ge", ">=", 2),
    op(b"gs", "::", 1),
    op(b"gt", ">", 2),
    op(b"ix", "[]", 2),
    op(b"lS", "<<=", 2),
    op(b"le", "<=", 2),
    op(b"li", "operator\"\" ", 1),
    op(b"ls", "<<", 2),
    op(b"lt", "<", 2),
    op(b"mI", "-=", 2),
    op(b"mL", "*=", 2),
    op(b"mi", "-", 2),
    op(b"ml", "*", 2),
    op(b"mm", "--", 1),
    op(b"na", "new[]", 3),
    op(b"ne", "!=", 2),
    op(b"ng", "-", 1),
    op(b"nt", "!", 1),
    op(b"nw", "new", 3),
    op(b"nx", "noexcept", 1),
    op(b"oR", "|=", 2),
    op(b"oo", "||", 2),
    op(b"or", "|", 2),
    op(b"pL", "+=", 2),
    op(b"pl", "+", 2),
    op(b"pm", "->*", 2),
    op(b"pp", "++", 1),
    op(b"ps", "+", 1),
    op(b"pt", "->", 2),
    op(b"qu", "?", 3),
    op(b"rM", "%=", 2),
    op(b"rS", ">>=", 2),
    op(b"rc", "reinterpret_cast", 2),
    op(b"rm", "%", 2),
    op(b"rs", ">>", 2),
    op(b"sP", "sizeof...", 1),
    op(b"sZ", "sizeof...", 1),
    op(b"sc", "static_cast", 2),
    op(b"ss", "<=>", 2),
    op(b"st", "sizeof ", 1),
    op(b"sz", "sizeof ", 1),
    op(b"tr", "throw", 0),
    op(b"tw", "throw ", 1),
];

/// A standard substitution (`Sa`, `Ss`, ...): the expansion `c++filt`
/// prints, and the class name constructors and destructors use.
pub(super) fn standard_substitution(code: u8) -> Option<(&'static str, Option<&'static str>)> {
    Some(match code {
        b't' => ("std", None),
        b'a' => ("std::allocator", Some("allocator")),
        b'b' => ("std::basic_string", Some("basic_string")),
        b's' => (
            "std::basic_string<char, std::char_traits<char>, std::allocator<char> >",
            Some("basic_string"),
        ),
        b'i' => (
            "std::basic_istream<char, std::char_traits<char> >",
            Some("basic_istream"),
        ),
        b'o' => (
            "std::basic_ostream<char, std::char_traits<char> >",
            Some("basic_ostream"),
        ),
        b'd' => (
            "std::basic_iostream<char, std::char_traits<char> >",
            Some("basic_iostream"),
        ),
        _ => return None,
    })
}
