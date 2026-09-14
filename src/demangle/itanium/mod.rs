//! Itanium C++ ABI demangling (`_Z...`), with `c++filt`'s output format.
//!
//! [`parse`] builds a syntax tree, [`print`] renders it. The output matches
//! GNU `c++filt` (binutils) byte for byte on the names it accepts, including
//! its spacing (`std::vector<int, std::allocator<int> >`), its spelling of
//! special names (`vtable for`, `{lambda(int)#1}`), and its expansion of the
//! standard substitutions (`Ss` prints the full `std::basic_string<...>`).
//!
//! A few names `c++filt` rejects are accepted here; see
//! `docs/compatibility.md`:
//!
//! - clone suffixes after data names (`_ZL3foo.llvm.123`);
//! - Mach-O's extra underscore (`__ZN3foo3barEv`);
//! - `_GLOBAL__sub_I_<file>` static initializers, printed as
//!   `global constructors keyed to <file>`;
//! - `_BitInt` types and the ABI's trailing `_` in `GR` names.

mod ast;
mod parse;
mod print;
mod tables;

use ast::Node;
use parse::{Parser, UnresolvedNames};
use print::Printer;

use super::output::Output;

/// Demangles an Itanium name, or returns `None` if it is not one.
pub(crate) fn demangle(name: &[u8]) -> Option<String> {
    demangle_with_output(name, Output::for_input(name.len()))
}

pub(crate) fn demangle_with_output(name: &[u8], out: Output) -> Option<String> {
    let (nodes, root) = parse(name)?;
    let mut printer = Printer::new(&nodes, out);
    printer.comp(root);
    printer.out.finish()
}

/// Parses `name`, retrying with the old unresolved-name syntax if needed.
fn parse(name: &[u8]) -> Option<(Vec<Node<'_>>, ast::Id)> {
    let (parser, root) = parse_name(name, UnresolvedNames::NewFirst)?;
    let (mut parser, root) = match root {
        Some(root) => (parser, root),
        None if parser.unresolved_name_state == UnresolvedNames::TriedNew => {
            let (parser, root) = parse_name(name, UnresolvedNames::OldOnly)?;
            (parser, root?)
        }
        None => return None,
    };
    Some((std::mem::take(&mut parser.nodes), root))
}

/// The pieces of a demangled name; see [`crate::demangle::Parts`].
pub(crate) struct Pieces {
    pub(crate) full: String,
    pub(crate) name: String,
    pub(crate) scope: String,
    pub(crate) base: String,
    pub(crate) params: Option<String>,
    pub(crate) qualifiers: String,
}

/// Splits a demangled Itanium name into [`Pieces`].
pub(crate) fn pieces(name: &[u8]) -> Option<Pieces> {
    let (nodes, root) = parse(name)?;
    let print = |f: &dyn Fn(&mut Printer<'_, '_>)| -> Option<String> {
        let mut printer = Printer::new(&nodes, Output::for_input(name.len()));
        f(&mut printer);
        printer.out.finish()
    };
    let full = print(&|p| p.comp(root))?;
    let mut encoding = root;
    while let Some(&Node::Clone(inner, _)) = nodes.get(encoding as usize) {
        encoding = inner;
    }
    let (entity, function) = match nodes.get(encoding as usize)? {
        &Node::Typed(entity, function) => (entity, Some(function)),
        Node::Special(..) | Node::CtorVtable(..) | Node::RefTemp(..) | Node::Global(..) => {
            return Some(Pieces {
                name: full.clone(),
                scope: String::new(),
                base: full.clone(),
                full,
                params: None,
                qualifiers: String::new(),
            });
        }
        _ => (encoding, None),
    };
    let qualified_name = print(&|p| p.name_path(entity))?;
    let mut scope_parts = Vec::new();
    let base = scope_and_base(&nodes, entity, &mut scope_parts);
    let base_text = print(&|p| p.comp(base))?;
    let mut scope = String::new();
    for part in &scope_parts {
        if !scope.is_empty() {
            scope.push_str("::");
        }
        match *part {
            ScopePart::Node(id) => scope.push_str(&print(&|p| p.comp(id))?),
            ScopePart::DefaultArg(number) => {
                scope.push_str(&format!("{{default arg#{}}}", number.saturating_add(1)));
            }
        }
    }
    let mut params = None;
    let mut qualifiers = String::new();
    if let Some(function) = function
        && let Some(&Node::Function(_, list)) = nodes.get(function as usize)
    {
        params = Some(print(&|p| p.parameter_list(entity, list))?);
        for qual in function_qualifiers(&nodes, entity).iter().rev() {
            qualifiers.push_str(&print(&|p| p.qualifier(*qual))?);
        }
    }
    Some(Pieces {
        full,
        name: qualified_name,
        scope,
        base: base_text,
        params,
        qualifiers: qualifiers.trim_start().to_string(),
    })
}

/// The member function qualifiers wrapping a function name, outermost
/// first (including those of a local entity).
fn function_qualifiers(nodes: &[Node<'_>], mut id: ast::Id) -> Vec<ast::Id> {
    let mut out = Vec::new();
    while let Some(node) = nodes.get(id as usize) {
        match *node {
            Node::Qualified(qual, inner) if qual.is_function_qualifier() => {
                out.push(id);
                id = inner;
            }
            Node::Local(_, entity) | Node::DefaultArg(_, entity) => id = entity,
            _ => break,
        }
    }
    out
}

/// A component of a name's scope.
enum ScopePart {
    /// A node printed as it is: a namespace, a class, a local function.
    Node(ast::Id),
    /// `{default arg#N}`.
    DefaultArg(u64),
}

/// Collects the scope of a name into `scope` and returns its innermost
/// component, without template arguments or ABI tags.
fn scope_and_base(nodes: &[Node<'_>], id: ast::Id, scope: &mut Vec<ScopePart>) -> ast::Id {
    let Some(node) = nodes.get(id as usize) else {
        return id;
    };
    match *node {
        Node::Qualified(qual, inner) if qual.is_function_qualifier() => {
            scope_and_base(nodes, inner, scope)
        }
        Node::Local(function, entity) => {
            scope.push(ScopePart::Node(function));
            scope_and_base(nodes, entity, scope)
        }
        Node::DefaultArg(number, entity) => {
            scope.push(ScopePart::DefaultArg(number));
            scope_and_base(nodes, entity, scope)
        }
        Node::Qual(outer, name) => {
            scope.push(ScopePart::Node(outer));
            scope_and_base(nodes, name, scope)
        }
        Node::Template(name, _)
        | Node::AbiTag(name, _)
        | Node::ModuleEntity(name, _)
        | Node::Friend(name) => scope_and_base(nodes, name, scope),
        _ => id,
    }
}

/// Parses `name`. The outer `None` means the name does not look like an
/// Itanium name at all; the inner one that parsing failed.
fn parse_name(name: &[u8], unresolved: UnresolvedNames) -> Option<(Parser<'_>, Option<ast::Id>)> {
    let body = name
        .strip_prefix(b"_Z")
        .or_else(|| name.strip_prefix(b"__Z"));
    if let Some(body) = body {
        let mut parser = Parser::new(body);
        parser.unresolved_name_state = unresolved;
        let root = parser.mangled_name_body(true).filter(|_| parser.at_end());
        return Some((parser, root));
    }
    let (prefix, rest) = global_constructor(name)?;
    let mut parser = Parser::new(rest);
    parser.unresolved_name_state = unresolved;
    let inner = if parser.eat_prefix(b"_Z") {
        parser.mangled_name_body(false).filter(|_| parser.at_end())
    } else {
        parser.add(Node::Name(rest))
    };
    let root = inner.and_then(|inner| parser.add(Node::Global(prefix, inner)));
    Some((parser, root))
}

/// `_GLOBAL_[._$][ID]_<name>` and GCC's `_GLOBAL__sub_[ID]_<name>`.
fn global_constructor(name: &[u8]) -> Option<(&'static str, &[u8])> {
    let rest = name.strip_prefix(b"_GLOBAL_")?;
    let (kind, rest) = if let Some(rest) = rest.strip_prefix(b"_sub_") {
        (rest.first()?, rest.get(1..)?)
    } else {
        if !matches!(rest.first(), Some(b'.' | b'_' | b'$')) {
            return None;
        }
        (rest.get(1)?, rest.get(2..)?)
    };
    let rest = rest.strip_prefix(b"_")?;
    if rest.is_empty() {
        return None;
    }
    let prefix = match kind {
        b'I' => "global constructors keyed to ",
        b'D' => "global destructors keyed to ",
        _ => return None,
    };
    Some((prefix, rest))
}

#[cfg(test)]
mod tests;
