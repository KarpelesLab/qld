//! Parser for Itanium C++ ABI manglings.
//!
//! A recursive-descent parser over the grammar of the Itanium C++ ABI
//! (<https://itanium-cxx-abi.github.io/cxx-abi/abi.html#mangling>), plus the
//! GNU extensions `c++filt` understands. It builds [`Node`]s in an arena and
//! keeps the substitution table as node indices. Recursion is bounded by
//! [`MAX_DEPTH`], the arena by [`MAX_NODES`], and every failure returns
//! `None`.

use super::ast::{Id, Node, ParamDecl, Qual};
use super::tables::{BUILTINS, EXTRA_BUILTINS, OPERATORS, standard_substitution};
use crate::demangle::{MAX_DEPTH, MAX_NODES, PARSE_FUEL};

/// Parser state.
pub(super) struct Parser<'a> {
    input: &'a [u8],
    pos: usize,
    pub(super) nodes: Vec<Node<'a>>,
    subs: Vec<Id>,
    depth: usize,
    /// The last source name seen, which constructors and destructors print.
    last_name: Option<Id>,
    /// Parsing the type of a conversion operator.
    is_conversion: bool,
    /// Parsing an expression (where `cv` is a cast, not a conversion).
    is_expression: bool,
    /// Which forms of unresolved names to try.
    pub(super) unresolved_name_state: UnresolvedNames,
    /// Parse steps left.
    steps: usize,
}

/// How `sr` unresolved names are parsed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum UnresolvedNames {
    /// Try the new `sr <qualifier>+ E <name>` form first.
    NewFirst,
    /// The new form was used; if parsing fails, retry with `OldOnly`.
    TriedNew,
    /// Only the old `sr <type> <name>` form.
    OldOnly,
}

type R<T> = Option<T>;

fn is_digit(c: Option<u8>) -> bool {
    c.is_some_and(|c| c.is_ascii_digit())
}

fn is_lower(c: Option<u8>) -> bool {
    c.is_some_and(|c| c.is_ascii_lowercase())
}

fn is_upper(c: Option<u8>) -> bool {
    c.is_some_and(|c| c.is_ascii_uppercase())
}

impl<'a> Parser<'a> {
    pub(super) fn new(input: &'a [u8]) -> Self {
        Self {
            input,
            pos: 0,
            nodes: Vec::new(),
            subs: Vec::new(),
            depth: 0,
            last_name: None,
            is_conversion: false,
            is_expression: false,
            unresolved_name_state: UnresolvedNames::NewFirst,
            steps: PARSE_FUEL,
        }
    }

    // ----- low-level helpers -----

    fn peek(&self) -> Option<u8> {
        self.input.get(self.pos).copied()
    }

    fn peek_next(&self) -> Option<u8> {
        self.input.get(self.pos.checked_add(1)?).copied()
    }

    fn advance(&mut self, n: usize) {
        self.pos = self.pos.saturating_add(n).min(self.input.len());
    }

    fn next(&mut self) -> Option<u8> {
        let c = self.peek()?;
        self.advance(1);
        Some(c)
    }

    fn eat(&mut self, c: u8) -> bool {
        if self.peek() == Some(c) {
            self.advance(1);
            true
        } else {
            false
        }
    }

    fn expect(&mut self, c: u8) -> R<()> {
        self.eat(c).then_some(())
    }

    /// Whether the whole input has been consumed.
    pub(super) fn at_end(&self) -> bool {
        self.pos >= self.input.len()
    }

    /// Consumes `n` bytes if the input continues with `prefix`.
    pub(super) fn eat_prefix(&mut self, prefix: &[u8]) -> bool {
        if self.rest().starts_with(prefix) {
            self.advance(prefix.len());
            true
        } else {
            false
        }
    }

    /// The unconsumed input.
    pub(super) fn rest(&self) -> &'a [u8] {
        self.input.get(self.pos..).unwrap_or_default()
    }

    pub(super) fn add(&mut self, node: Node<'a>) -> R<Id> {
        if self.nodes.len() >= MAX_NODES {
            return None;
        }
        let id = Id::try_from(self.nodes.len()).ok()?;
        self.nodes.push(node);
        Some(id)
    }

    fn node(&self, id: Id) -> Option<&Node<'a>> {
        self.nodes.get(usize::try_from(id).ok()?)
    }

    fn add_sub(&mut self, id: Id) -> R<()> {
        if self.subs.len() >= MAX_NODES {
            return None;
        }
        self.subs.push(id);
        Some(())
    }

    /// Runs `f` one recursion level deeper, spending one parse step.
    fn nested<T>(&mut self, f: impl FnOnce(&mut Self) -> R<T>) -> R<T> {
        if self.depth >= MAX_DEPTH {
            return None;
        }
        // Speculative parses (conversion operator types) can revisit input,
        // so the total work is bounded separately from the input length.
        self.steps = self.steps.checked_sub(1)?;
        self.depth = self.depth.saturating_add(1);
        let result = f(self);
        self.depth = self.depth.saturating_sub(1);
        result
    }

    /// `[n] <digits>`: a possibly negative decimal number. No digits reads
    /// as 0; values beyond `i32::MAX` fail.
    fn number(&mut self) -> R<i64> {
        let negative = self.eat(b'n');
        let mut value: i64 = 0;
        while let Some(c) = self.peek().filter(u8::is_ascii_digit) {
            value = value
                .checked_mul(10)?
                .checked_add(i64::from(c.wrapping_sub(b'0')))?;
            if value > i64::from(i32::MAX) {
                return None;
            }
            self.advance(1);
        }
        Some(if negative {
            value.wrapping_neg()
        } else {
            value
        })
    }

    /// A number that must not be negative.
    fn unsigned_number(&mut self) -> R<u64> {
        u64::try_from(self.number()?).ok()
    }

    /// `_` is 0, `<number> _` is number + 1.
    fn compact_number(&mut self) -> R<u64> {
        let value = match self.peek()? {
            b'_' => 0,
            b'n' => return None,
            _ => self.unsigned_number()?.checked_add(1)?,
        };
        self.expect(b'_')?;
        Some(value)
    }

    // ----- top level -----

    /// `<encoding>` and, at the top level, clone suffixes; the input
    /// position is just after `_Z`.
    pub(super) fn mangled_name_body(&mut self, top: bool) -> R<Id> {
        let mut encoding = self.encoding(top)?;
        if top {
            while self.peek() == Some(b'.')
                && (is_lower(self.peek_next())
                    || self.peek_next() == Some(b'_')
                    || is_digit(self.peek_next()))
            {
                encoding = self.clone_suffix(encoding)?;
            }
        }
        Some(encoding)
    }

    /// `.suffix[.N]*`.
    fn clone_suffix(&mut self, encoding: Id) -> R<Id> {
        let rest = self.rest();
        let at = |i: usize| rest.get(i).copied();
        let ident = |c: Option<u8>| {
            c.is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
        };
        let mut end = 0usize;
        if at(0) == Some(b'.') && ident(at(1)) {
            end = 2;
            while ident(at(end)) {
                end = end.saturating_add(1);
            }
        }
        while at(end) == Some(b'.') && is_digit(at(end.saturating_add(1))) {
            end = end.saturating_add(2);
            while is_digit(at(end)) {
                end = end.saturating_add(1);
            }
        }
        let suffix = rest.get(..end)?;
        self.advance(end);
        let name = self.add(Node::Name(suffix))?;
        self.add(Node::Clone(encoding, name))
    }

    /// `<encoding>`.
    fn encoding(&mut self, top: bool) -> R<Id> {
        self.nested(|p| {
            if matches!(p.peek(), Some(b'G' | b'T')) {
                return p.special_name();
            }
            let name = p.name()?;
            match p.peek() {
                None | Some(b'E') => return Some(name),
                // A clone suffix right after a data name: `c++filt` rejects
                // these, but parameter lists never start with a dot.
                Some(b'.') if top => return Some(name),
                _ => {}
            }
            let has_return_type = p.has_return_type(name);
            let function = p.bare_function_type(has_return_type)?;
            if !top && matches!(p.node(name), Some(Node::Local(..))) {
                p.clear_return_type(function);
            }
            p.add(Node::Typed(name, function))
        })
    }

    fn clear_return_type(&mut self, function: Id) {
        if let Ok(index) = usize::try_from(function)
            && let Some(Node::Function(ret, _)) = self.nodes.get_mut(index)
        {
            *ret = None;
        }
    }

    /// Whether a function with this name mangles its return type.
    fn has_return_type(&self, id: Id) -> bool {
        match self.node(id) {
            Some(Node::Local(_, entity)) => self.has_return_type(*entity),
            Some(Node::Template(name, _)) => !self.is_ctor_dtor_or_conversion(*name),
            Some(Node::Qualified(qual, inner)) if qual.is_function_qualifier() => {
                self.has_return_type(*inner)
            }
            _ => false,
        }
    }

    fn is_ctor_dtor_or_conversion(&self, id: Id) -> bool {
        match self.node(id) {
            Some(Node::Qual(_, name) | Node::Local(_, name)) => {
                self.is_ctor_dtor_or_conversion(*name)
            }
            Some(Node::Ctor(_) | Node::Dtor(_) | Node::Conversion(_)) => true,
            _ => false,
        }
    }

    /// `<special-name>`.
    fn special_name(&mut self) -> R<Id> {
        let (prefix, inner) = match (self.next()?, self.next()?) {
            (b'T', b'V') => ("vtable for ", self.ty()?),
            (b'T', b'T') => ("VTT for ", self.ty()?),
            (b'T', b'I') => ("typeinfo for ", self.ty()?),
            (b'T', b'S') => ("typeinfo name for ", self.ty()?),
            (b'T', b'F') => ("typeinfo fn for ", self.ty()?),
            (b'T', b'J') => ("java Class for ", self.ty()?),
            (b'T', b'h') => {
                self.call_offset(Some(b'h'))?;
                ("non-virtual thunk to ", self.encoding(false)?)
            }
            (b'T', b'v') => {
                self.call_offset(Some(b'v'))?;
                ("virtual thunk to ", self.encoding(false)?)
            }
            (b'T', b'c') => {
                self.call_offset(None)?;
                self.call_offset(None)?;
                ("covariant return thunk to ", self.encoding(false)?)
            }
            (b'T', b'C') => {
                let derived = self.ty()?;
                if self.number()? < 0 {
                    return None;
                }
                self.expect(b'_')?;
                let base = self.ty()?;
                return self.add(Node::CtorVtable(base, derived));
            }
            (b'T', b'H') => ("TLS init function for ", self.name()?),
            (b'T', b'W') => ("TLS wrapper function for ", self.name()?),
            (b'T', b'A') => ("template parameter object for ", self.template_arg()?),
            (b'G', b'V') => ("guard variable for ", self.name()?),
            (b'G', b'R') => {
                let name = self.name()?;
                let number = self.unsigned_number()?;
                // The ABI ends the name with `_`; `c++filt` does not expect it.
                self.eat(b'_');
                return self.add(Node::RefTemp(name, number));
            }
            (b'G', b'A') => ("hidden alias for ", self.encoding(false)?),
            (b'G', b'T') => match self.next()? {
                b'n' => ("non-transaction clone for ", self.encoding(false)?),
                _ => ("transaction clone for ", self.encoding(false)?),
            },
            _ => return None,
        };
        self.add(Node::Special(prefix, inner))
    }

    /// `<call-offset> ::= h <nv-offset> _ | v <v-offset> _`.
    fn call_offset(&mut self, kind: Option<u8>) -> R<()> {
        let kind = match kind {
            Some(kind) => kind,
            None => self.next()?,
        };
        match kind {
            b'h' => {
                self.number()?;
            }
            b'v' => {
                self.number()?;
                self.expect(b'_')?;
                self.number()?;
            }
            _ => return None,
        }
        self.expect(b'_')
    }

    // ----- names -----

    /// `<name>`.
    fn name(&mut self) -> R<Id> {
        self.nested(|p| match p.peek()? {
            b'N' => p.nested_name(),
            b'Z' => p.local_name(),
            b'S' => {
                let (name, from_substitution) = if p.peek_next() == Some(b't') {
                    p.advance(2);
                    let std = p.add(Node::Text("std"))?;
                    let inner = p.unqualified_name(None, None)?;
                    (p.add(Node::Qual(std, inner))?, false)
                } else {
                    (p.substitution(false)?, true)
                };
                if p.peek() != Some(b'I') {
                    return Some(name);
                }
                if !from_substitution {
                    p.add_sub(name)?;
                }
                let args = p.template_args()?;
                p.add(Node::Template(name, args))
            }
            _ => {
                let name = p.unqualified_name(None, None)?;
                if p.peek() != Some(b'I') {
                    return Some(name);
                }
                p.add_sub(name)?;
                let args = p.template_args()?;
                p.add(Node::Template(name, args))
            }
        })
    }

    /// `N [<CV-qualifiers>] [<ref-qualifier>] <prefix> E`.
    fn nested_name(&mut self) -> R<Id> {
        self.expect(b'N')?;
        let quals = self.cv_qualifier_list(true)?;
        let ref_qual = match self.peek() {
            Some(b'R') => Some(Qual::RefThis),
            Some(b'O') => Some(Qual::RvalueRefThis),
            _ => None,
        };
        if ref_qual.is_some() {
            self.advance(1);
        }
        let mut name = self.prefix(true)?;
        self.expect(b'E')?;
        for qual in quals.iter().rev() {
            name = self.add(Node::Qualified(*qual, name))?;
        }
        if let Some(qual) = ref_qual {
            name = self.add(Node::Qualified(qual, name))?;
        }
        Some(name)
    }

    /// `<prefix> <unqualified-name>`: components up to the closing `E`.
    fn prefix(&mut self, substitutable: bool) -> R<Id> {
        let mut ret: Option<Id> = None;
        loop {
            let peek = self.peek()?;
            if peek == b'D' && matches!(self.peek_next(), Some(b'T' | b't')) {
                if ret.is_some() {
                    return None;
                }
                ret = Some(self.ty()?);
            } else if peek == b'I' {
                let name = ret?;
                let args = self.template_args()?;
                ret = Some(self.add(Node::Template(name, args))?);
            } else if peek == b'T' {
                if ret.is_some() {
                    return None;
                }
                ret = Some(self.template_param()?);
            } else if peek == b'M' {
                // Initializer scope of a lambda; already a substitution.
                self.advance(1);
                continue;
            } else {
                let mut module = None;
                if peek == b'S' {
                    let sub = self.substitution(true)?;
                    if matches!(self.node(sub), Some(Node::ModuleName(..))) {
                        module = Some(sub);
                    } else {
                        if ret.is_some() {
                            return None;
                        }
                        ret = Some(sub);
                        continue;
                    }
                }
                ret = Some(self.unqualified_name(ret, module)?);
            }
            if self.peek() == Some(b'E') {
                break;
            }
            if substitutable {
                self.add_sub(ret?)?;
            }
        }
        ret
    }

    /// `W <source-name>` module names, possibly nested and with partitions.
    fn module_name(&mut self, module: &mut Option<Id>) -> R<()> {
        while self.eat(b'W') {
            let partition = self.eat(b'P');
            let name = self.source_name()?;
            let id = self.add(Node::ModuleName(*module, name, partition))?;
            self.add_sub(id)?;
            *module = Some(id);
        }
        Some(())
    }

    /// `<unqualified-name>`, qualified by `scope` if given.
    fn unqualified_name(&mut self, scope: Option<Id>, module: Option<Id>) -> R<Id> {
        let mut module = module;
        self.module_name(&mut module)?;
        let friend = self.eat(b'F');
        let peek = self.peek()?;
        let mut ret = if peek.is_ascii_digit() {
            self.source_name()?
        } else if peek.is_ascii_lowercase() {
            let was_expression = self.is_expression;
            if peek == b'o' && self.peek_next() == Some(b'n') {
                self.advance(2);
                self.is_expression = false;
            }
            let op = self.operator_name();
            self.is_expression = was_expression;
            let op = op?;
            if matches!(self.node(op), Some(Node::Operator(info)) if info.is(b"li")) {
                let name = self.source_name()?;
                self.add(Node::Unary {
                    op,
                    operand: name,
                    postfix: false,
                })?
            } else {
                op
            }
        } else if peek == b'D' && self.peek_next() == Some(b'C') {
            self.advance(2);
            let mut names = Vec::new();
            loop {
                names.push(self.source_name()?);
                if self.eat(b'E') {
                    break;
                }
            }
            self.add(Node::Binding(names))?
        } else if peek == b'C' || peek == b'D' {
            self.ctor_dtor_name()?
        } else if peek == b'L' {
            self.advance(1);
            let name = self.source_name()?;
            self.discriminator()?;
            name
        } else if peek == b'U' {
            match self.peek_next()? {
                b'l' => self.lambda()?,
                b't' => self.unnamed_type()?,
                _ => return None,
            }
        } else {
            return None;
        };
        if let Some(module) = module {
            ret = self.add(Node::ModuleEntity(ret, module))?;
        }
        if self.peek() == Some(b'B') {
            ret = self.abi_tags(ret)?;
        }
        if friend {
            ret = self.add(Node::Friend(ret))?;
        }
        if let Some(scope) = scope {
            ret = self.add(Node::Qual(scope, ret))?;
        }
        Some(ret)
    }

    /// `<source-name> ::= <length> <identifier>`.
    fn source_name(&mut self) -> R<Id> {
        let len = usize::try_from(self.number()?)
            .ok()
            .filter(|&len| len > 0)?;
        let end = self.pos.checked_add(len)?;
        let ident = self.input.get(self.pos..end)?;
        self.pos = end;
        let node = if ident.len() >= 10
            && ident.starts_with(b"_GLOBAL_")
            && matches!(ident.get(8), Some(b'.' | b'_' | b'$'))
            && ident.get(9) == Some(&b'N')
        {
            Node::Text("(anonymous namespace)")
        } else {
            Node::Name(ident)
        };
        let id = self.add(node)?;
        self.last_name = Some(id);
        Some(id)
    }

    /// `B <source-name>` ABI tags.
    fn abi_tags(&mut self, name: Id) -> R<Id> {
        let hold = self.last_name;
        let mut ret = name;
        while self.eat(b'B') {
            let tag = self.source_name()?;
            ret = self.add(Node::AbiTag(ret, tag))?;
        }
        self.last_name = hold;
        Some(ret)
    }

    /// `_ <digit>` or `__ <number> _`, ignored.
    fn discriminator(&mut self) -> R<()> {
        if !self.eat(b'_') {
            return Some(());
        }
        let two = self.eat(b'_');
        let value = self.number()?;
        if value < 0 {
            return None;
        }
        if two && value >= 10 {
            self.expect(b'_')?;
        }
        Some(())
    }

    /// `C1`..`C5`, `CI1 <type>`, `D0`..`D5`.
    fn ctor_dtor_name(&mut self) -> R<Id> {
        match self.peek()? {
            b'C' => {
                let inheriting = self.peek_next() == Some(b'I');
                if inheriting {
                    self.advance(1);
                }
                if !matches!(self.peek_next(), Some(b'1'..=b'5')) {
                    return None;
                }
                self.advance(2);
                if inheriting {
                    self.ty()?;
                }
                let name = self.last_name?;
                self.add(Node::Ctor(name))
            }
            b'D' => {
                if !matches!(self.peek_next(), Some(b'0' | b'1' | b'2' | b'4' | b'5')) {
                    return None;
                }
                self.advance(2);
                let name = self.last_name?;
                self.add(Node::Dtor(name))
            }
            _ => None,
        }
    }

    /// `Ul [<template-head>] <lambda-sig> E [<number>] _`.
    fn lambda(&mut self) -> R<Id> {
        self.advance(2);
        let mut decls = Vec::new();
        while self.peek() == Some(b'T')
            && matches!(self.peek_next(), Some(b'y' | b'n' | b't' | b'p'))
        {
            decls.push(self.template_param_decl()?);
        }
        let head = if decls.is_empty() {
            None
        } else {
            Some(self.add(Node::TemplateArgs(decls))?)
        };
        let params = self.parameter_list()?;
        self.expect(b'E')?;
        let number = self.compact_number()?;
        self.add(Node::Lambda {
            head,
            params,
            number,
        })
    }

    /// `Ty`, `Tn <type>`, `Tt <decl>* E`, `Tp <decl>`.
    fn template_param_decl(&mut self) -> R<Id> {
        self.nested(|p| {
            p.expect(b'T')?;
            let decl = match p.next()? {
                b'y' => ParamDecl::Type,
                b'n' => ParamDecl::NonType(p.ty()?),
                b't' => {
                    let mut decls = Vec::new();
                    while !p.eat(b'E') {
                        decls.push(p.template_param_decl()?);
                    }
                    ParamDecl::Template(p.add(Node::TemplateArgs(decls))?)
                }
                b'p' => ParamDecl::Pack(p.template_param_decl()?),
                _ => return None,
            };
            p.add(Node::TemplateParamDecl(decl))
        })
    }

    /// `Ut [<number>] _`.
    fn unnamed_type(&mut self) -> R<Id> {
        self.advance(2);
        let number = self.compact_number()?;
        // Unlike lambdas, unnamed types are substitution candidates by
        // themselves (as `c++filt` has it).
        let id = self.add(Node::Unnamed(number))?;
        self.add_sub(id)?;
        Some(id)
    }

    /// `Z <encoding> E <entity> [<discriminator>]`.
    fn local_name(&mut self) -> R<Id> {
        self.expect(b'Z')?;
        let function = self.encoding(false)?;
        self.expect(b'E')?;
        let entity = if self.eat(b's') {
            self.discriminator()?;
            self.add(Node::Text("string literal"))?
        } else {
            let default_arg = if self.eat(b'd') {
                Some(self.compact_number()?)
            } else {
                None
            };
            let name = self.name()?;
            if !matches!(
                self.node(name),
                Some(Node::Lambda { .. } | Node::Unnamed(_))
            ) {
                self.discriminator()?;
            }
            match default_arg {
                Some(number) => self.add(Node::DefaultArg(number, name))?,
                None => name,
            }
        };
        if let Some(&Node::Typed(_, function_type)) = self.node(function) {
            self.clear_return_type(function_type);
        }
        self.add(Node::Local(function, entity))
    }

    /// `<substitution>`. `prefix` is true inside nested names.
    fn substitution(&mut self, _prefix: bool) -> R<Id> {
        self.expect(b'S')?;
        let c = self.next()?;
        if c == b'_' || c.is_ascii_digit() || c.is_ascii_uppercase() {
            let mut index: usize = 0;
            if c != b'_' {
                let mut c = c;
                loop {
                    let digit = match c {
                        b'0'..=b'9' => c.wrapping_sub(b'0'),
                        b'A'..=b'Z' => c.wrapping_sub(b'A').wrapping_add(10),
                        _ => return None,
                    };
                    index = index.checked_mul(36)?.checked_add(usize::from(digit))?;
                    c = self.next()?;
                    if c == b'_' {
                        break;
                    }
                }
                index = index.checked_add(1)?;
            }
            return self.subs.get(index).copied();
        }
        let (expansion, last) = standard_substitution(c)?;
        if let Some(last) = last {
            self.last_name = Some(self.add(Node::Text(last))?);
        }
        let mut id = self.add(Node::Std(expansion))?;
        if self.peek() == Some(b'B') {
            id = self.abi_tags(id)?;
            self.add_sub(id)?;
        }
        Some(id)
    }

    // ----- operators -----

    /// `<operator-name>`, including `cv <type>` and vendor operators.
    fn operator_name(&mut self) -> R<Id> {
        let c1 = self.next()?;
        let c2 = self.next()?;
        if c1 == b'v' && c2.is_ascii_digit() {
            let name = self.source_name()?;
            return self.add(Node::VendorOperator(c2.wrapping_sub(b'0'), name));
        }
        if c1 == b'c' && c2 == b'v' {
            let was_conversion = self.is_conversion;
            self.is_conversion = !self.is_expression;
            let ty = self.ty();
            let conversion = self.is_conversion;
            self.is_conversion = was_conversion;
            let ty = ty?;
            return self.add(if conversion {
                Node::Conversion(ty)
            } else {
                Node::Cast(ty)
            });
        }
        let code = [c1, c2];
        let info = OPERATORS.iter().find(|op| op.code == code)?;
        self.add(Node::Operator(info))
    }

    // ----- types -----

    fn next_is_type_qualifier(&self) -> bool {
        match self.peek() {
            Some(b'r' | b'V' | b'K') => true,
            Some(b'D') => matches!(self.peek_next(), Some(b'x' | b'o' | b'O' | b'w')),
            _ => false,
        }
    }

    /// Reads `r`, `V`, `K` and the function qualifiers `Dx`, `Do`, `DO`,
    /// `Dw`, in mangled (outermost first) order.
    fn cv_qualifier_list(&mut self, member_fn: bool) -> R<Vec<Qual>> {
        let mut quals = Vec::new();
        while self.next_is_type_qualifier() {
            let qual = match self.next()? {
                b'r' if member_fn => Qual::RestrictThis,
                b'r' => Qual::Restrict,
                b'V' if member_fn => Qual::VolatileThis,
                b'V' => Qual::Volatile,
                b'K' if member_fn => Qual::ConstThis,
                b'K' => Qual::Const,
                _ => match self.next()? {
                    b'x' => Qual::TransactionSafe,
                    b'o' => Qual::Noexcept(None),
                    b'O' => {
                        let expr = self.expression()?;
                        self.expect(b'E')?;
                        Qual::Noexcept(Some(expr))
                    }
                    b'w' => {
                        let list = self.parameter_list()?;
                        self.expect(b'E')?;
                        Qual::Throw(list)
                    }
                    _ => return None,
                },
            };
            if quals.len() >= MAX_DEPTH {
                return None;
            }
            quals.push(qual);
        }
        Some(quals)
    }

    /// `<type>`.
    fn ty(&mut self) -> R<Id> {
        self.nested(Self::ty_inner)
    }

    fn ty_inner(&mut self) -> R<Id> {
        if self.next_is_type_qualifier() {
            return self.qualified_type();
        }
        let mut substitutable = true;
        let peek = self.peek()?;
        let ret = match peek {
            b'u' => {
                self.advance(1);
                let name = self.source_name()?;
                self.add(Node::VendorType(name))?
            }
            b'a'..=b'z' => {
                let index = usize::from(peek.wrapping_sub(b'a'));
                let builtin = BUILTINS.get(index)?.as_ref()?;
                self.advance(1);
                substitutable = false;
                self.add(Node::Builtin(builtin))?
            }
            b'F' => self.function_type()?,
            b'0'..=b'9' | b'N' | b'Z' => self.name()?,
            b'A' => self.array_type()?,
            b'M' => self.pointer_to_member_type()?,
            b'T' => self.template_param_type()?,
            b'S' => {
                let next = self.peek_next();
                if is_digit(next) || next == Some(b'_') || is_upper(next) {
                    let sub = self.substitution(false)?;
                    if self.peek() == Some(b'I') {
                        let args = self.template_args()?;
                        self.add(Node::Template(sub, args))?
                    } else {
                        substitutable = false;
                        sub
                    }
                } else {
                    let name = self.name()?;
                    if matches!(self.node(name), Some(Node::Std(_))) {
                        substitutable = false;
                    }
                    name
                }
            }
            b'P' | b'R' | b'O' | b'C' | b'G' => {
                self.advance(1);
                let inner = self.ty()?;
                self.add(match peek {
                    b'P' => Node::Pointer(inner),
                    b'R' => Node::Reference(inner),
                    b'O' => Node::RvalueReference(inner),
                    b'C' => Node::Complex(inner),
                    _ => Node::Imaginary(inner),
                })?
            }
            b'U' => {
                self.advance(1);
                let name = self.source_name()?;
                let qualifier = if self.peek() == Some(b'I') {
                    let args = self.template_args()?;
                    self.add(Node::Template(name, args))?
                } else {
                    name
                };
                let inner = self.ty()?;
                self.add(Node::VendorQual(inner, qualifier))?
            }
            b'D' => {
                self.advance(1);
                let (node, subst) = self.d_type()?;
                substitutable = subst;
                node
            }
            _ => return None,
        };
        if substitutable {
            self.add_sub(ret)?;
        }
        Some(ret)
    }

    /// Types starting with `D`, after the `D`. Returns the node and whether
    /// it is a substitution candidate.
    fn d_type(&mut self) -> R<(Id, bool)> {
        let c = self.next()?;
        Some(match c {
            b'T' | b't' => {
                let expr = self.expression()?;
                self.expect(b'E')?;
                (self.add(Node::Decltype(expr))?, true)
            }
            b'p' => {
                let inner = self.ty()?;
                (self.add(Node::PackExpansion(inner))?, true)
            }
            b'a' => (self.add(Node::Text("auto"))?, false),
            b'c' => (self.add(Node::Text("decltype(auto)"))?, false),
            b'F' => {
                let bits = self.unsigned_number()?;
                let suffix = match self.next()? {
                    b'_' => None,
                    b'x' => Some(b'x'),
                    b'b' if bits == 16 => {
                        return Some((self.add(Node::Text("std::bfloat16_t"))?, false));
                    }
                    _ => return None,
                };
                (
                    self.add(Node::ExtendedBuiltin("_Float", bits, suffix))?,
                    false,
                )
            }
            b'B' | b'U' => {
                let dim = if is_digit(self.peek()) {
                    let n = self.unsigned_number()?;
                    self.add(Node::Number(n))?
                } else {
                    self.expression()?
                };
                self.expect(b'_')?;
                (self.add(Node::BitInt(c == b'B', dim))?, true)
            }
            b'v' => {
                let dim = if self.eat(b'_') {
                    self.expression()?
                } else {
                    let n = self.unsigned_number()?;
                    self.add(Node::Number(n))?
                };
                self.expect(b'_')?;
                let elem = self.ty()?;
                (self.add(Node::Vector(dim, elem))?, true)
            }
            _ => {
                let (_, builtin) = EXTRA_BUILTINS.iter().find(|(code, _)| *code == c)?;
                (self.add(Node::Builtin(builtin))?, false)
            }
        })
    }

    /// `<qualifiers> <type>`: the qualifiers wrap the type, and become
    /// member function qualifiers before a function type.
    fn qualified_type(&mut self) -> R<Id> {
        let mut quals = self.cv_qualifier_list(false)?;
        let inner = if self.peek() == Some(b'F') {
            for qual in &mut quals {
                *qual = match *qual {
                    Qual::Restrict => Qual::RestrictThis,
                    Qual::Volatile => Qual::VolatileThis,
                    Qual::Const => Qual::ConstThis,
                    other => other,
                };
            }
            self.function_type()?
        } else {
            self.ty()?
        };
        // A function type's ref-qualifier moves outside the cv-qualifiers,
        // so that they print in source order.
        let (mut base, ref_qual) = match self.node(inner) {
            Some(&Node::Qualified(q @ (Qual::RefThis | Qual::RvalueRefThis), function)) => {
                (function, Some(q))
            }
            _ => (inner, None),
        };
        for qual in quals.iter().rev() {
            base = self.add(Node::Qualified(*qual, base))?;
        }
        let ret = match ref_qual {
            Some(q) => self.add(Node::Qualified(q, base))?,
            None => base,
        };
        self.add_sub(ret)?;
        Some(ret)
    }

    /// `<template-param> [<template-args>]` in a type.
    fn template_param_type(&mut self) -> R<Id> {
        let param = self.template_param()?;
        if self.peek() != Some(b'I') {
            return Some(param);
        }
        if !self.is_conversion {
            self.add_sub(param)?;
            let args = self.template_args()?;
            return self.add(Node::Template(param, args));
        }
        // In a conversion operator's type, the arguments may belong to the
        // operator itself: they belong to the parameter only if another
        // argument list follows.
        let checkpoint = (self.pos, self.nodes.len(), self.subs.len(), self.last_name);
        let args = self.template_args();
        if let Some(args) = args
            && self.peek() == Some(b'I')
        {
            self.add_sub(param)?;
            return self.add(Node::Template(param, args));
        }
        self.pos = checkpoint.0;
        self.nodes.truncate(checkpoint.1);
        self.subs.truncate(checkpoint.2);
        self.last_name = checkpoint.3;
        Some(param)
    }

    /// `F [Y] <bare-function-type> [<ref-qualifier>] E`.
    fn function_type(&mut self) -> R<Id> {
        self.nested(|p| {
            p.expect(b'F')?;
            // `Y`: extern "C", not printed.
            p.eat(b'Y');
            let mut ret = p.bare_function_type(true)?;
            let ref_qual = match p.peek() {
                Some(b'R') => Some(Qual::RefThis),
                Some(b'O') => Some(Qual::RvalueRefThis),
                _ => None,
            };
            if let Some(qual) = ref_qual {
                p.advance(1);
                ret = p.add(Node::Qualified(qual, ret))?;
            }
            p.expect(b'E')?;
            Some(ret)
        })
    }

    /// `[J] [<return type>] <parameter types>`.
    fn bare_function_type(&mut self, has_return_type: bool) -> R<Id> {
        let has_return_type = self.eat(b'J') || has_return_type;
        let ret = if has_return_type {
            Some(self.ty()?)
        } else {
            None
        };
        let params = self.parameter_list()?;
        self.add(Node::Function(ret, params))
    }

    /// One or more parameter types; a lone `void` means none.
    fn parameter_list(&mut self) -> R<Id> {
        let mut params = Vec::new();
        loop {
            match self.peek() {
                None | Some(b'E' | b'.' | b'Q') => break,
                Some(b'R' | b'O') if self.peek_next() == Some(b'E') => break,
                _ => {}
            }
            params.push(self.ty()?);
        }
        if params.is_empty() {
            return None;
        }
        if let [only] = params.as_slice()
            && matches!(self.node(*only), Some(Node::Builtin(b)) if b.style == super::ast::LiteralStyle::Void)
        {
            params.clear();
        }
        self.add(Node::Args(params))
    }

    /// `A <dimension> _ <element type>`.
    fn array_type(&mut self) -> R<Id> {
        self.expect(b'A')?;
        let dim = match self.peek()? {
            b'_' => None,
            c if c.is_ascii_digit() => {
                let start = self.pos;
                while is_digit(self.peek()) {
                    self.advance(1);
                }
                let digits = self.input.get(start..self.pos)?;
                Some(self.add(Node::Name(digits))?)
            }
            _ => Some(self.expression()?),
        };
        self.expect(b'_')?;
        let elem = self.ty()?;
        self.add(Node::Array(dim, elem))
    }

    /// `M <class type> <member type>`.
    fn pointer_to_member_type(&mut self) -> R<Id> {
        self.expect(b'M')?;
        let class = self.ty()?;
        let member = self.ty()?;
        self.add(Node::PtrMem(class, member))
    }

    /// `T_`, `T <number> _`.
    fn template_param(&mut self) -> R<Id> {
        self.expect(b'T')?;
        let index = self.compact_number()?;
        self.add(Node::TemplateParam(index))
    }

    // ----- template arguments -----

    /// `I <template-arg>+ E` (or `J ... E` for a pack).
    fn template_args(&mut self) -> R<Id> {
        self.nested(|p| {
            let hold = p.last_name;
            if !matches!(p.peek(), Some(b'I' | b'J')) {
                return None;
            }
            p.advance(1);
            let mut args = Vec::new();
            while !p.eat(b'E') {
                args.push(p.template_arg()?);
            }
            p.last_name = hold;
            p.add(Node::TemplateArgs(args))
        })
    }

    /// `<template-arg>`.
    fn template_arg(&mut self) -> R<Id> {
        match self.peek()? {
            b'X' => {
                self.advance(1);
                let expr = self.expression()?;
                self.expect(b'E')?;
                Some(expr)
            }
            b'L' => self.expr_primary(),
            b'I' | b'J' => self.template_args(),
            _ => self.ty(),
        }
    }

    // ----- expressions -----

    /// `<expression>`.
    fn expression(&mut self) -> R<Id> {
        let was = self.is_expression;
        self.is_expression = true;
        let ret = self.expression_inner();
        self.is_expression = was;
        ret
    }

    fn expression_inner(&mut self) -> R<Id> {
        self.nested(Self::expression_body)
    }

    fn expression_body(&mut self) -> R<Id> {
        let peek = self.peek()?;
        let next = self.peek_next();
        match (peek, next) {
            (b'L', _) => return self.expr_primary(),
            (b'T', _) => return self.template_param(),
            (b's', Some(b'r')) => return self.unresolved_name(),
            (b's', Some(b'p')) => {
                self.advance(2);
                let inner = self.expression_inner()?;
                return self.add(Node::PackExpansion(inner));
            }
            (b'f', Some(b'p')) => {
                self.advance(2);
                let index = if self.eat(b'T') {
                    0
                } else {
                    self.compact_number()?.checked_add(1)?
                };
                return self.add(Node::FunctionParam(index));
            }
            (b'0'..=b'9', _) | (b'o', Some(b'n')) => {
                if peek == b'o' {
                    self.advance(2);
                }
                let name = self.unqualified_name(None, None)?;
                if self.peek() != Some(b'I') {
                    return Some(name);
                }
                let args = self.template_args()?;
                return self.add(Node::Template(name, args));
            }
            (b'i' | b't', Some(b'l')) => {
                self.advance(2);
                let ty = if peek == b't' { Some(self.ty()?) } else { None };
                self.peek_next()?;
                let list = self.expression_list(b'E')?;
                return self.add(Node::InitList(ty, list));
            }
            (b'u', _) => {
                self.advance(1);
                let name = self.source_name()?;
                let mut args = Vec::new();
                while !self.eat(b'E') {
                    args.push(self.template_arg()?);
                }
                let args = self.add(Node::TemplateArgs(args))?;
                return self.add(Node::VendorExpr(name, args));
            }
            _ => {}
        }
        self.operator_expression()
    }

    /// `sr <type> <name>`, or `sr <qualifier>+ E <name>`.
    ///
    /// The second form (GCC 11 and later) is ambiguous with the first
    /// (`sr1A1x` versus `sr1AE1x`), so it is tried first, and the whole name
    /// is parsed again with the first form if that fails.
    fn unresolved_name(&mut self) -> R<Id> {
        self.advance(2);
        let ty = if self.unresolved_name_state != UnresolvedNames::OldOnly
            && matches!(
                self.peek(),
                Some(b'0'..=b'9' | b'a'..=b'z' | b'C' | b'U' | b'L')
            ) {
            self.unresolved_name_state = UnresolvedNames::TriedNew;
            let ty = self.prefix(false)?;
            self.eat(b'E');
            ty
        } else {
            self.ty()?
        };
        let name = self.unqualified_name(Some(ty), None)?;
        if self.peek() != Some(b'I') {
            return Some(name);
        }
        let args = self.template_args()?;
        self.add(Node::Template(name, args))
    }

    fn operator_expression(&mut self) -> R<Id> {
        let op = self.operator_name()?;
        let (code, arity) = match self.node(op)? {
            Node::Operator(info) => (Some(info.code), info.arity),
            Node::VendorOperator(arity, _) => (None, *arity),
            Node::Cast(_) => (None, 1),
            _ => return None,
        };
        if code == Some(*b"st") {
            let ty = self.ty()?;
            return self.add(Node::Unary {
                op,
                operand: ty,
                postfix: false,
            });
        }
        match arity {
            0 => self.add(Node::Nullary(op)),
            1 => {
                let mut postfix = false;
                if let Some([c0, c1]) = code
                    && (c0 == b'p' || c0 == b'm')
                    && c1 == c0
                {
                    postfix = !self.eat(b'_');
                }
                let operand = if matches!(self.node(op), Some(Node::Cast(_))) && self.eat(b'_') {
                    self.expression_list(b'E')?
                } else if code == Some(*b"sP") {
                    let mut args = Vec::new();
                    while !self.eat(b'E') {
                        args.push(self.template_arg()?);
                    }
                    self.add(Node::TemplateArgs(args))?
                } else {
                    self.expression_inner()?
                };
                self.add(Node::Unary {
                    op,
                    operand,
                    postfix,
                })
            }
            2 => {
                let code = code?;
                let left = if matches!(&code, b"dc" | b"sc" | b"cc" | b"rc") {
                    self.ty()?
                } else if code[0] == b'f' {
                    self.operator_name()?
                } else if &code == b"di" {
                    self.unqualified_name(None, None)?
                } else {
                    self.expression_inner()?
                };
                let right = if &code == b"cl" {
                    self.expression_list(b'E')?
                } else if matches!(&code, b"dt" | b"pt") {
                    if matches!(
                        (self.peek(), self.peek_next()),
                        (Some(b'g'), Some(b's')) | (Some(b's'), Some(b'r'))
                    ) {
                        self.expression_inner()?
                    } else {
                        let mut name = self.unqualified_name(None, None)?;
                        if self.peek() == Some(b'I') {
                            let args = self.template_args()?;
                            name = self.add(Node::Template(name, args))?;
                        }
                        name
                    }
                } else {
                    self.expression_inner()?
                };
                self.add(Node::Binary { op, left, right })
            }
            3 => {
                let code = code?;
                let (first, second, third) = if matches!(&code, b"qu" | b"dX") {
                    let first = self.expression_inner()?;
                    let second = self.expression_inner()?;
                    let third = self.expression_inner()?;
                    (first, second, Some(third))
                } else if code[0] == b'f' {
                    let first = self.operator_name()?;
                    let second = self.expression_inner()?;
                    let third = self.expression_inner()?;
                    (first, second, Some(third))
                } else if matches!(&code, b"nw" | b"na") {
                    let placement = self.expression_list(b'_')?;
                    let ty = self.ty()?;
                    let init = if self.eat(b'E') {
                        None
                    } else if self.eat_prefix(b"pi") {
                        Some(self.expression_list(b'E')?)
                    } else if self.rest().starts_with(b"il") {
                        Some(self.expression_inner()?)
                    } else {
                        return None;
                    };
                    (placement, ty, init)
                } else {
                    return None;
                };
                self.add(Node::Trinary {
                    op,
                    first,
                    second,
                    third,
                })
            }
            _ => None,
        }
    }

    /// Expressions up to `terminator`, as a [`Node::Args`].
    fn expression_list(&mut self, terminator: u8) -> R<Id> {
        let mut list = Vec::new();
        while !self.eat(terminator) {
            list.push(self.expression_inner()?);
        }
        self.add(Node::Args(list))
    }

    /// `L <type> <value> E`, `L _Z <encoding> E`.
    fn expr_primary(&mut self) -> R<Id> {
        self.expect(b'L')?;
        let ret = if matches!(self.peek(), Some(b'_' | b'Z')) {
            self.eat(b'_');
            self.expect(b'Z')?;
            self.mangled_name_body(false)?
        } else {
            let ty = self.ty()?;
            if matches!(self.node(ty), Some(Node::Builtin(b)) if b.name == "decltype(nullptr)")
                && self.eat(b'E')
            {
                return Some(ty);
            }
            let negative = self.eat(b'n');
            let start = self.pos;
            while self.peek() != Some(b'E') {
                self.next()?;
            }
            let value = self.input.get(start..self.pos)?;
            let value = self.add(Node::Name(value))?;
            self.add(Node::Literal(ty, value, negative))?
        };
        self.expect(b'E')?;
        Some(ret)
    }
}
