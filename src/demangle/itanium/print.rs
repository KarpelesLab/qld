//! Printer for the Itanium syntax tree, producing `c++filt`'s spelling.
//!
//! C++ declarator syntax puts part of a type after the name it declares
//! (`void (*f())(int)`). The printer handles this the way GNU's demangler
//! does: pointers, references, qualifiers and names are pushed on a stack
//! of pending *modifiers* while the type they modify is printed, and a
//! function or array type prints the pending modifiers at the right place
//! (inside parentheses, before its parameter list or dimension). Modifiers
//! nobody printed are printed by whoever pushed them.
//!
//! Template parameters are resolved while printing, against a stack of the
//! templates whose arguments are in scope.
//!
//! The stacks are arenas indexed by position and truncated when the frame
//! that pushed onto them returns. Every visit spends fuel, recursion is
//! capped, and a node may only be printed while it is already being printed
//! at most once, which is what stops cyclic template references.

use super::ast::{Id, LiteralStyle, Node, Operator, ParamDecl, Qual};
use crate::demangle::MAX_DEPTH;
use crate::demangle::output::Output;

#[derive(Clone, Copy, Debug)]
struct Mod {
    node: Id,
    printed: bool,
    templates: Option<usize>,
    next: Option<usize>,
}

#[derive(Clone, Copy, Debug)]
struct Template {
    decl: Id,
    next: Option<usize>,
}

pub(super) struct Printer<'n, 'a> {
    nodes: &'n [Node<'a>],
    pub(super) out: Output,
    mods: Vec<Mod>,
    modifiers: Option<usize>,
    templates_arena: Vec<Template>,
    templates: Option<usize>,
    current_template: Option<Id>,
    pack_index: i64,
    lambda_arg: u32,
    lambda_head: Option<Id>,
    printing: Vec<u8>,
    depth: usize,
    stack: Vec<Id>,
    saved_scopes: Vec<(Id, Vec<Id>)>,
}

impl<'n, 'a> Printer<'n, 'a> {
    pub(super) fn new(nodes: &'n [Node<'a>], out: Output) -> Self {
        Self {
            nodes,
            out,
            mods: Vec::new(),
            modifiers: None,
            templates_arena: Vec::new(),
            templates: None,
            current_template: None,
            pack_index: 0,
            lambda_arg: 0,
            lambda_head: None,
            printing: vec![0; nodes.len()],
            depth: 0,
            stack: Vec::new(),
            saved_scopes: Vec::new(),
        }
    }

    fn node(&self, id: Id) -> Option<&'n Node<'a>> {
        self.nodes.get(usize::try_from(id).ok()?)
    }

    fn fail(&mut self) {
        self.out.fail();
    }

    fn last_char(&self) -> Option<char> {
        self.out.last_char()
    }

    fn push_mod(&mut self, node: Id) -> usize {
        let index = self.mods.len();
        self.mods.push(Mod {
            node,
            printed: false,
            templates: self.templates,
            next: self.modifiers,
        });
        self.modifiers = Some(index);
        index
    }

    fn mod_at(&self, index: usize) -> Option<Mod> {
        self.mods.get(index).copied()
    }

    fn set_printed(&mut self, index: usize) {
        if let Some(m) = self.mods.get_mut(index) {
            m.printed = true;
        }
    }

    fn push_template(&mut self, decl: Id) -> usize {
        let index = self.templates_arena.len();
        self.templates_arena.push(Template {
            decl,
            next: self.templates,
        });
        self.templates = Some(index);
        index
    }

    fn template_next(&self, index: Option<usize>) -> Option<usize> {
        index
            .and_then(|i| self.templates_arena.get(i))
            .and_then(|t| t.next)
    }

    /// Prints one node.
    pub(super) fn comp(&mut self, id: Id) {
        if self.out.failed() || !self.out.step() {
            return;
        }
        let Ok(index) = usize::try_from(id) else {
            return self.fail();
        };
        let Some(&count) = self.printing.get(index) else {
            return self.fail();
        };
        if count > 1 || self.depth >= MAX_DEPTH {
            return self.fail();
        }
        if let Some(slot) = self.printing.get_mut(index) {
            *slot = count.saturating_add(1);
        }
        self.depth = self.depth.saturating_add(1);
        self.stack.push(id);
        let (mods, templates) = (self.mods.len(), self.templates_arena.len());
        self.comp_inner(id);
        self.mods.truncate(mods.max(self.live_mods()));
        self.templates_arena
            .truncate(templates.max(self.live_templates()));
        self.stack.pop();
        self.depth = self.depth.saturating_sub(1);
        if let Some(slot) = self.printing.get_mut(index) {
            *slot = slot.saturating_sub(1);
        }
    }

    /// One past the highest modifier index still reachable from the current
    /// stack head, so truncation never frees a live entry.
    fn live_mods(&self) -> usize {
        self.modifiers.map_or(0, |i| i.saturating_add(1))
    }

    fn live_templates(&self) -> usize {
        self.templates.map_or(0, |i| i.saturating_add(1))
    }

    fn comp_inner(&mut self, id: Id) {
        let Some(node) = self.node(id) else {
            return self.fail();
        };
        match *node {
            Node::Name(bytes) => self.out.push_bytes(bytes),
            Node::Text(text) | Node::Std(text) => self.out.push(text),
            Node::AbiTag(name, tag) => {
                self.comp(name);
                self.out.push("[abi:");
                self.comp(tag);
                self.out.push("]");
            }
            Node::Binding(ref names) => {
                self.out.push("[");
                for (i, &name) in names.iter().enumerate() {
                    if i > 0 {
                        self.out.push(", ");
                    }
                    self.comp(name);
                }
                self.out.push("]");
            }
            Node::ModuleEntity(name, module) => {
                self.comp(name);
                self.out.push("@");
                self.comp(module);
            }
            Node::ModuleName(parent, name, partition) => {
                if let Some(parent) = parent {
                    self.comp(parent);
                }
                if partition {
                    self.out.push(":");
                } else if parent.is_some() {
                    self.out.push(".");
                }
                self.comp(name);
            }
            Node::Qual(scope, name) | Node::Local(scope, name) => {
                self.comp(scope);
                self.out.push("::");
                self.default_arg_then(name);
            }
            Node::Typed(name, function) => self.typed_name(name, function),
            Node::Template(name, args) => self.template(id, name, args),
            Node::TemplateParam(index) => self.template_param(index),
            Node::FunctionParam(0) => self.out.push("this"),
            Node::FunctionParam(n) => {
                self.out.push("{parm#");
                self.out.push_u64(n);
                self.out.push("}");
            }
            Node::Ctor(name) => self.comp(name),
            Node::Dtor(name) => {
                self.out.push("~");
                self.comp(name);
            }
            Node::Special(prefix, inner) | Node::Global(prefix, inner) => {
                self.out.push(prefix);
                self.comp(inner);
            }
            Node::CtorVtable(base, derived) => {
                self.out.push("construction vtable for ");
                self.comp(base);
                self.out.push("-in-");
                self.comp(derived);
            }
            Node::RefTemp(name, number) => {
                self.out.push("reference temporary #");
                self.out.push_u64(number);
                self.out.push(" for ");
                self.comp(name);
            }
            Node::Qualified(qual, inner) => {
                if matches!(qual, Qual::Restrict | Qual::Volatile | Qual::Const) {
                    // A qualifier already pending (from an array, or `T const`
                    // with `T` itself const) is printed once.
                    let mut cursor = self.modifiers;
                    while let Some(m) = cursor.and_then(|i| self.mod_at(i)) {
                        if !m.printed {
                            let pending = match self.node(m.node) {
                                Some(&Node::Qualified(
                                    q @ (Qual::Restrict | Qual::Volatile | Qual::Const),
                                    _,
                                )) => q,
                                _ => break,
                            };
                            if pending == qual {
                                self.comp(inner);
                                return;
                            }
                        }
                        cursor = m.next;
                    }
                }
                self.modifier(id, inner);
            }
            Node::Reference(inner) | Node::RvalueReference(inner) => self.reference(id, inner),
            Node::Pointer(inner) | Node::Complex(inner) | Node::Imaginary(inner) => {
                self.modifier(id, inner);
            }
            Node::VendorQual(inner, _) => self.modifier(id, inner),
            Node::Builtin(builtin) => self.out.push(builtin.name),
            Node::ExtendedBuiltin(name, bits, suffix) => {
                self.out.push(name);
                self.out.push_u64(bits);
                if let Some(suffix) = suffix {
                    self.out.push_char(char::from(suffix));
                }
            }
            Node::BitInt(signed, dim) => {
                self.out.push(if signed {
                    "_BitInt("
                } else {
                    "unsigned _BitInt("
                });
                self.comp(dim);
                self.out.push(")");
            }
            Node::VendorType(name) => self.comp(name),
            Node::Function(ret, params) => self.function(id, ret, params),
            Node::Array(dim, elem) => self.array(id, dim, elem),
            Node::PtrMem(_, inner) | Node::Vector(_, inner) => {
                let m = self.push_mod(id);
                self.comp(inner);
                let entry = self.mod_at(m);
                if entry.is_some_and(|e| !e.printed) {
                    self.print_mod(id);
                }
                self.modifiers = entry.and_then(|e| e.next);
            }
            Node::Args(ref list) | Node::TemplateArgs(ref list) => self.list(list),
            Node::InitList(ty, list) => {
                if let Some(ty) = ty {
                    self.comp(ty);
                }
                self.out.push("{");
                self.comp(list);
                self.out.push("}");
            }
            Node::Operator(op) => {
                self.out.push("operator");
                if op
                    .name
                    .as_bytes()
                    .first()
                    .is_some_and(u8::is_ascii_lowercase)
                {
                    self.out.push(" ");
                }
                self.out.push(op.name.strip_suffix(' ').unwrap_or(op.name));
            }
            Node::VendorOperator(_, name) => {
                self.out.push("operator ");
                self.comp(name);
            }
            Node::Conversion(ty) => {
                self.out.push("operator ");
                self.conversion(ty);
            }
            Node::Cast(ty) => self.comp(ty),
            Node::Nullary(op) => self.expr_op(op),
            Node::Unary {
                op,
                operand,
                postfix,
            } => self.unary(op, operand, postfix),
            Node::Binary { op, left, right } => self.binary(op, left, right),
            Node::Trinary {
                op,
                first,
                second,
                third,
            } => self.trinary(op, first, second, third),
            Node::Literal(ty, value, negative) => self.literal(ty, value, negative),
            Node::VendorExpr(name, args) => {
                self.comp(name);
                self.comp(args);
            }
            Node::Number(n) => self.out.push_u64(n),
            Node::Decltype(expr) => {
                self.out.push("decltype (");
                self.comp(expr);
                self.out.push(")");
            }
            Node::PackExpansion(inner) => self.pack_expansion(inner),
            Node::Lambda {
                head,
                params,
                number,
            } => {
                self.out.push("{lambda");
                if let Some(head) = head {
                    self.out.push("<");
                    self.template_head(head, true);
                    self.out.push(">");
                }
                self.out.push("(");
                let hold = self.lambda_head;
                self.lambda_head = head;
                self.lambda_arg = self.lambda_arg.saturating_add(1);
                self.comp(params);
                self.lambda_arg = self.lambda_arg.saturating_sub(1);
                self.lambda_head = hold;
                self.out.push(")#");
                self.out.push_u64(number.saturating_add(1));
                self.out.push("}");
            }
            Node::Unnamed(number) => {
                self.out.push("{unnamed type#");
                self.out.push_u64(number.saturating_add(1));
                self.out.push("}");
            }
            Node::Clone(encoding, suffix) => {
                self.comp(encoding);
                self.out.push(" [clone ");
                self.comp(suffix);
                self.out.push("]");
            }
            Node::DefaultArg(..) => self.default_arg_then(id),
            Node::Friend(name) => {
                self.comp(name);
                self.out.push("[friend]");
            }
            Node::TemplateParamDecl(_) => self.param_decl(id, None),
        }
    }

    /// Prints a function's name without its return type, parameters and
    /// member function qualifiers.
    pub(super) fn name_path(&mut self, id: Id) {
        match self.node(id) {
            Some(&Node::Qualified(qual, inner)) if qual.is_function_qualifier() => {
                self.name_path(inner);
            }
            Some(&Node::Local(function, entity)) => {
                self.comp(function);
                self.out.push("::");
                let mut entity = entity;
                if let Some(&Node::DefaultArg(number, inner)) = self.node(entity) {
                    self.out.push("{default arg#");
                    self.out.push_u64(number.saturating_add(1));
                    self.out.push("}::");
                    entity = inner;
                }
                self.name_path(entity);
            }
            _ => self.comp(id),
        }
    }

    /// Prints a function's parameter list, `(int, char)`, with the
    /// function's template arguments in scope.
    pub(super) fn parameter_list(&mut self, name: Id, params: Id) {
        let template = self.typed_name_template(name);
        if let Some(template) = template {
            self.push_template(template);
        }
        self.out.push("(");
        self.comp(params);
        self.out.push(")");
    }

    /// Prints one member function qualifier, with its leading space.
    pub(super) fn qualifier(&mut self, id: Id) {
        self.print_mod(id);
    }

    /// The template whose arguments a function's parameter types refer to:
    /// the function name itself, looking through qualifiers and local
    /// scopes.
    fn typed_name_template(&self, name: Id) -> Option<Id> {
        let mut typed = name;
        loop {
            match self.node(typed)? {
                &Node::Qualified(q, inner) if q.is_function_qualifier() => typed = inner,
                &Node::Local(_, entity) => typed = entity,
                &Node::DefaultArg(_, inner) => typed = inner,
                Node::Template(..) => return Some(typed),
                _ => return None,
            }
        }
    }

    /// Prints `{default arg#N}::` for a default argument scope, then the
    /// entity.
    fn default_arg_then(&mut self, id: Id) {
        match self.node(id) {
            Some(&Node::DefaultArg(number, entity)) => {
                self.out.push("{default arg#");
                self.out.push_u64(number.saturating_add(1));
                self.out.push("}::");
                self.comp(entity);
            }
            _ => self.comp(id),
        }
    }

    /// A comma-separated list. Elements that print nothing (empty packs)
    /// drop their separator when nothing follows them.
    fn list(&mut self, list: &[Id]) {
        let mut marks = Vec::new();
        for (i, &item) in list.iter().enumerate() {
            if i > 0 {
                self.out.push(", ");
                marks.push(self.out.len());
            }
            self.comp(item);
        }
        for mark in marks.into_iter().rev() {
            if self.out.len() == mark {
                self.out.truncate(mark.saturating_sub(2));
            }
        }
    }

    // ----- modifiers -----

    fn modifier(&mut self, id: Id, inner: Id) {
        let m = self.push_mod(id);
        self.comp(inner);
        let entry = self.mod_at(m);
        if entry.is_some_and(|e| !e.printed) {
            self.print_mod(id);
        }
        self.modifiers = entry.and_then(|e| e.next);
    }

    /// References, with reference collapsing through template parameters:
    /// `T&` with `T = int&&` prints `int&`.
    fn reference(&mut self, id: Id, inner: Id) {
        let mut dc = id;
        let mut sub = inner;
        let mut mod_inner = None;
        let mut restore = None;
        if self.lambda_arg == 0
            && let Some(&Node::TemplateParam(index)) = self.node(sub)
        {
            match self.saved_scopes.iter().position(|(c, _)| *c == sub) {
                None => {
                    let mut chain = Vec::new();
                    let mut cursor = self.templates;
                    while let Some(t) = cursor.and_then(|i| self.templates_arena.get(i)) {
                        chain.push(t.decl);
                        cursor = t.next;
                    }
                    self.saved_scopes.push((sub, chain));
                }
                Some(scope) => {
                    let top = self.stack.len().saturating_sub(1);
                    let found = self
                        .stack
                        .iter()
                        .enumerate()
                        .any(|(i, &c)| c == sub || (c == id && i != top));
                    if !found {
                        restore = Some(self.templates);
                        let chain = self
                            .saved_scopes
                            .get(scope)
                            .map(|(_, chain)| chain.clone())
                            .unwrap_or_default();
                        self.templates = None;
                        for &decl in chain.iter().rev() {
                            self.push_template(decl);
                        }
                    }
                }
            }
            let arg = self.lookup_template_argument(index);
            let arg = match arg.and_then(|a| self.node(a).map(|n| (a, n))) {
                Some((a, Node::TemplateArgs(_))) => {
                    self.index_template_argument(a, self.pack_index)
                }
                other => other.map(|(a, _)| a),
            };
            let Some(arg) = arg else {
                if let Some(templates) = restore {
                    self.templates = templates;
                }
                return self.fail();
            };
            sub = arg;
        }
        let same_kind = matches!(
            (self.node(id), self.node(sub)),
            (
                Some(Node::RvalueReference(_)),
                Some(Node::RvalueReference(_))
            )
        );
        match self.node(sub) {
            Some(Node::Reference(_)) => dc = sub,
            _ if same_kind => dc = sub,
            Some(&Node::RvalueReference(inner)) => mod_inner = Some(inner),
            _ => {}
        }
        let inner = match mod_inner {
            Some(inner) => inner,
            None => match self.node(dc) {
                Some(&Node::Reference(inner) | &Node::RvalueReference(inner)) => inner,
                _ => sub,
            },
        };
        self.modifier(dc, inner);
        if let Some(templates) = restore {
            self.templates = templates;
        }
    }

    fn is_function_qualifier(&self, id: Id) -> bool {
        matches!(self.node(id), Some(Node::Qualified(q, _)) if q.is_function_qualifier())
    }

    fn print_mod(&mut self, id: Id) {
        let Some(node) = self.node(id) else {
            return self.fail();
        };
        match *node {
            Node::Qualified(qual, _) => match qual {
                Qual::Restrict | Qual::RestrictThis => self.out.push(" restrict"),
                Qual::Volatile | Qual::VolatileThis => self.out.push(" volatile"),
                Qual::Const | Qual::ConstThis => self.out.push(" const"),
                Qual::TransactionSafe => self.out.push(" transaction_safe"),
                Qual::Noexcept(expr) => {
                    self.out.push(" noexcept");
                    if let Some(expr) = expr {
                        self.out.push("(");
                        self.comp(expr);
                        self.out.push(")");
                    }
                }
                Qual::Throw(list) => {
                    self.out.push(" throw(");
                    self.comp(list);
                    self.out.push(")");
                }
                Qual::RefThis => self.out.push(" &"),
                Qual::RvalueRefThis => self.out.push(" &&"),
            },
            Node::VendorQual(_, qualifier) => {
                self.out.push(" ");
                self.comp(qualifier);
            }
            Node::Pointer(_) => self.out.push("*"),
            Node::Reference(_) => self.out.push("&"),
            Node::RvalueReference(_) => self.out.push("&&"),
            Node::Complex(_) => self.out.push(" _Complex"),
            Node::Imaginary(_) => self.out.push(" _Imaginary"),
            Node::PtrMem(class, _) => {
                if self.last_char() != Some('(') {
                    self.out.push(" ");
                }
                self.comp(class);
                self.out.push("::*");
            }
            Node::Typed(name, _) => self.comp(name),
            Node::Vector(dim, _) => {
                self.out.push(" __vector(");
                self.comp(dim);
                self.out.push(")");
            }
            _ => self.comp(id),
        }
    }

    fn print_mod_list(&mut self, mods: Option<usize>, suffix: bool) {
        let mut cursor = mods;
        while let Some(index) = cursor {
            if self.out.failed() {
                return;
            }
            let Some(m) = self.mod_at(index) else {
                return;
            };
            if m.printed || (!suffix && self.is_function_qualifier(m.node)) {
                cursor = m.next;
                continue;
            }
            self.set_printed(index);
            let hold = self.templates;
            self.templates = m.templates;
            match self.node(m.node) {
                Some(&Node::Function(_, params)) => {
                    self.function_type(params, m.next);
                    self.templates = hold;
                    return;
                }
                Some(&Node::Array(dim, _)) => {
                    self.array_type(dim, m.next);
                    self.templates = hold;
                    return;
                }
                Some(&Node::Local(function, entity)) => {
                    let hold_mods = self.modifiers;
                    self.modifiers = None;
                    self.comp(function);
                    self.modifiers = hold_mods;
                    self.out.push("::");
                    let mut entity = entity;
                    if let Some(&Node::DefaultArg(number, inner)) = self.node(entity) {
                        self.out.push("{default arg#");
                        self.out.push_u64(number.saturating_add(1));
                        self.out.push("}::");
                        entity = inner;
                    }
                    while let Some(&Node::Qualified(q, inner)) = self.node(entity) {
                        if !q.is_function_qualifier() {
                            break;
                        }
                        entity = inner;
                    }
                    self.comp(entity);
                    self.templates = hold;
                    return;
                }
                _ => {}
            }
            self.print_mod(m.node);
            self.templates = hold;
            cursor = m.next;
        }
    }

    // ----- functions and arrays -----

    fn typed_name(&mut self, name: Id, function: Id) {
        let hold_modifiers = self.modifiers;
        self.modifiers = None;
        let mut slots: Vec<usize> = Vec::new();
        let mut typed = name;
        loop {
            if slots.len() >= 4 {
                return self.fail();
            }
            slots.push(self.push_mod(typed));
            match self.node(typed) {
                Some(&Node::Qualified(q, inner)) if q.is_function_qualifier() => typed = inner,
                _ => break,
            }
        }
        if let Some(&Node::Local(_, entity)) = self.node(typed) {
            typed = entity;
            if let Some(&Node::DefaultArg(_, inner)) = self.node(typed) {
                typed = inner;
            }
            while let Some(&Node::Qualified(q, inner)) = self.node(typed) {
                if !q.is_function_qualifier() {
                    break;
                }
                if slots.len() >= 4 {
                    return self.fail();
                }
                let Some(&last) = slots.last() else {
                    return self.fail();
                };
                let Some(local) = self.mod_at(last) else {
                    return self.fail();
                };
                let copy = self.mods.len();
                self.mods.push(Mod {
                    next: Some(last),
                    ..local
                });
                self.modifiers = Some(copy);
                if let Some(slot) = self.mods.get_mut(last) {
                    *slot = Mod {
                        node: typed,
                        printed: false,
                        templates: self.templates,
                        next: local.next,
                    };
                }
                slots.push(copy);
                typed = inner;
            }
        }
        let template = matches!(self.node(typed), Some(Node::Template(..)));
        let hold_templates = self.templates;
        if template {
            self.push_template(typed);
        }
        self.comp(function);
        if template {
            self.templates = hold_templates;
        }
        for &slot in slots.iter().rev() {
            if let Some(m) = self.mod_at(slot)
                && !m.printed
            {
                self.out.push(" ");
                self.print_mod(m.node);
            }
        }
        self.modifiers = hold_modifiers;
    }

    fn function(&mut self, id: Id, ret: Option<Id>, params: Id) {
        if let Some(ret) = ret {
            let m = self.push_mod(id);
            self.comp(ret);
            let entry = self.mod_at(m);
            self.modifiers = entry.and_then(|e| e.next);
            if entry.is_some_and(|e| e.printed) {
                return;
            }
            self.out.push(" ");
        }
        let mods = self.modifiers;
        self.function_type(params, mods);
    }

    fn function_type(&mut self, params: Id, mods: Option<usize>) {
        let mut need_paren = false;
        let mut need_space = false;
        let mut cursor = mods;
        while let Some(m) = cursor.and_then(|i| self.mod_at(i)) {
            if m.printed {
                break;
            }
            match self.node(m.node) {
                Some(Node::Pointer(_) | Node::Reference(_) | Node::RvalueReference(_)) => {
                    need_paren = true;
                }
                Some(
                    Node::Qualified(Qual::Restrict | Qual::Volatile | Qual::Const, _)
                    | Node::VendorQual(..)
                    | Node::Complex(_)
                    | Node::Imaginary(_)
                    | Node::PtrMem(..),
                ) => {
                    need_space = true;
                    need_paren = true;
                }
                _ => {}
            }
            if need_paren {
                break;
            }
            cursor = m.next;
        }
        if need_paren {
            if !need_space && !matches!(self.last_char(), Some('(' | '*')) {
                need_space = true;
            }
            if need_space && self.last_char() != Some(' ') {
                self.out.push(" ");
            }
            self.out.push("(");
        }
        let hold = self.modifiers;
        self.modifiers = None;
        self.print_mod_list(mods, false);
        if need_paren {
            self.out.push(")");
        }
        self.out.push("(");
        self.comp(params);
        self.out.push(")");
        self.print_mod_list(mods, true);
        self.modifiers = hold;
    }

    fn array(&mut self, id: Id, dim: Option<Id>, elem: Id) {
        let hold = self.modifiers;
        let first = self.push_mod(id);
        // A qualified array is printed as an array of qualified elements.
        let mut copies = Vec::new();
        let mut cursor = hold;
        while let Some(index) = cursor {
            let Some(m) = self.mod_at(index) else {
                break;
            };
            if !matches!(
                self.node(m.node),
                Some(Node::Qualified(
                    Qual::Restrict | Qual::Volatile | Qual::Const,
                    _
                ))
            ) {
                break;
            }
            if !m.printed {
                if copies.len() >= 3 {
                    return self.fail();
                }
                let copy = self.mods.len();
                self.mods.push(Mod {
                    next: self.modifiers,
                    printed: false,
                    ..m
                });
                self.modifiers = Some(copy);
                self.set_printed(index);
                copies.push(copy);
            }
            cursor = m.next;
        }
        self.comp(elem);
        self.modifiers = hold;
        if self.mod_at(first).is_some_and(|m| m.printed) {
            return;
        }
        for &copy in copies.iter().rev() {
            if let Some(m) = self.mod_at(copy) {
                self.print_mod(m.node);
            }
        }
        let mods = self.modifiers;
        self.array_type(dim, mods);
    }

    fn array_type(&mut self, dim: Option<Id>, mods: Option<usize>) {
        let mut need_space = true;
        if mods.is_some() {
            let mut need_paren = false;
            let mut cursor = mods;
            while let Some(m) = cursor.and_then(|i| self.mod_at(i)) {
                if !m.printed {
                    if matches!(self.node(m.node), Some(Node::Array(..))) {
                        need_space = false;
                    } else {
                        need_paren = true;
                        need_space = true;
                    }
                    break;
                }
                cursor = m.next;
            }
            if need_paren {
                self.out.push(" (");
            }
            self.print_mod_list(mods, false);
            if need_paren {
                self.out.push(")");
            }
        }
        if need_space {
            self.out.push(" ");
        }
        self.out.push("[");
        if let Some(dim) = dim {
            self.comp(dim);
        }
        self.out.push("]");
    }

    // ----- templates -----

    fn template(&mut self, id: Id, name: Id, args: Id) {
        let hold_current = self.current_template;
        self.current_template = Some(id);
        let hold_mods = self.modifiers;
        self.modifiers = None;
        self.comp(name);
        self.template_args(args);
        self.modifiers = hold_mods;
        self.current_template = hold_current;
    }

    fn template_args(&mut self, args: Id) {
        if self.last_char() == Some('<') {
            self.out.push(" ");
        }
        self.out.push("<");
        self.comp(args);
        if self.last_char() == Some('>') {
            self.out.push(" ");
        }
        self.out.push(">");
    }

    fn lookup_template_argument(&mut self, index: u64) -> Option<Id> {
        let Some(top) = self.templates.and_then(|i| self.templates_arena.get(i)) else {
            self.fail();
            return None;
        };
        let Some(&Node::Template(_, args)) = self.node(top.decl) else {
            return None;
        };
        self.index_template_argument(args, i64::try_from(index).ok()?)
    }

    fn index_template_argument(&self, args: Id, index: i64) -> Option<Id> {
        if index < 0 {
            return Some(args);
        }
        match self.node(args) {
            Some(Node::TemplateArgs(list)) => list.get(usize::try_from(index).ok()?).copied(),
            _ => None,
        }
    }

    fn template_param(&mut self, index: u64) {
        if self.lambda_arg > 0 {
            return self.lambda_param(index);
        }
        let arg = self.lookup_template_argument(index);
        let arg = match arg.and_then(|a| self.node(a).map(|n| (a, n))) {
            Some((a, Node::TemplateArgs(_))) => self.index_template_argument(a, self.pack_index),
            other => other.map(|(a, _)| a),
        };
        let Some(arg) = arg else {
            return self.fail();
        };
        let hold = self.templates;
        self.templates = self.template_next(hold);
        self.comp(arg);
        self.templates = hold;
    }

    /// A template parameter in a lambda signature: `auto:N`, or the name
    /// its template head gives it.
    fn lambda_param(&mut self, index: u64) {
        let decl = self.lambda_head.and_then(|head| match self.node(head) {
            Some(Node::TemplateArgs(list)) => list.get(usize::try_from(index).ok()?).copied(),
            _ => None,
        });
        match decl {
            Some(decl) => {
                let prefix = self.decl_prefix(decl);
                self.out.push("$");
                self.out.push(prefix);
                self.out.push_u64(index);
            }
            None => {
                self.out.push("auto:");
                self.out.push_u64(index.saturating_add(1));
            }
        }
    }

    fn decl_prefix(&self, decl: Id) -> &'static str {
        match self.node(decl) {
            Some(Node::TemplateParamDecl(ParamDecl::Type)) => "T",
            Some(Node::TemplateParamDecl(ParamDecl::NonType(_))) => "N",
            Some(Node::TemplateParamDecl(ParamDecl::Template(_))) => "TT",
            Some(&Node::TemplateParamDecl(ParamDecl::Pack(inner))) => self.decl_prefix(inner),
            _ => "T",
        }
    }

    /// A lambda's template head: declarations separated by commas, named
    /// `$T0`, `$N1`, ... when `named`.
    fn template_head(&mut self, head: Id, named: bool) {
        let Some(Node::TemplateArgs(list)) = self.node(head) else {
            return self.fail();
        };
        for (i, &decl) in list.iter().enumerate() {
            if i > 0 {
                self.out.push(", ");
            }
            let index = if named { u64::try_from(i).ok() } else { None };
            self.param_decl(decl, index);
        }
    }

    fn param_decl(&mut self, decl: Id, index: Option<u64>) {
        let Some(Node::TemplateParamDecl(kind)) = self.node(decl) else {
            return self.fail();
        };
        let mut kind = *kind;
        let mut pack = false;
        while let ParamDecl::Pack(inner) = kind {
            pack = true;
            match self.node(inner) {
                Some(Node::TemplateParamDecl(k)) => kind = *k,
                _ => return self.fail(),
            }
        }
        match kind {
            ParamDecl::Type => self.out.push("typename"),
            ParamDecl::NonType(ty) => self.comp(ty),
            ParamDecl::Template(list) => {
                self.out.push("template<");
                self.template_head(list, false);
                self.out.push("> class");
            }
            ParamDecl::Pack(_) => {}
        }
        if pack {
            self.out.push("...");
        }
        if let Some(index) = index {
            let prefix = self.decl_prefix(decl);
            self.out.push(" $");
            self.out.push(prefix);
            self.out.push_u64(index);
        }
    }

    fn conversion(&mut self, ty: Id) {
        let hold = self.templates;
        if let Some(current) = self.current_template {
            self.push_template(current);
        }
        match self.node(ty) {
            Some(&Node::Template(name, args)) => {
                self.comp(name);
                self.templates = hold;
                self.template_args(args);
            }
            _ => {
                self.comp(ty);
                self.templates = hold;
            }
        }
    }

    // ----- expressions -----

    fn operator_info(&self, op: Id) -> Option<&'static Operator> {
        match self.node(op) {
            Some(Node::Operator(info)) => Some(info),
            _ => None,
        }
    }

    fn expr_op(&mut self, op: Id) {
        match self.operator_info(op) {
            Some(info) => self.out.push(info.name),
            None => self.comp(op),
        }
    }

    fn subexpr(&mut self, id: Id) {
        let simple = matches!(
            self.node(id),
            Some(
                Node::Name(_)
                    | Node::Text(_)
                    | Node::Qual(..)
                    | Node::InitList(..)
                    | Node::FunctionParam(_)
            )
        );
        if !simple {
            self.out.push("(");
        }
        self.comp(id);
        if !simple {
            self.out.push(")");
        }
    }

    fn unary(&mut self, op: Id, operand: Id, postfix: bool) {
        let info = self.operator_info(op);
        let code = info.map(|i| i.code);
        let mut operand = operand;
        if code == Some(*b"ad")
            && let Some(&Node::Typed(name, function)) = self.node(operand)
            && matches!(self.node(name), Some(Node::Qual(..)))
            && matches!(self.node(function), Some(Node::Function(..)))
        {
            // The address of a member function: no parameter list.
            operand = name;
        }
        if postfix && info.is_some() {
            self.subexpr(operand);
            self.expr_op(op);
            return;
        }
        if code == Some(*b"sZ") {
            let pack = self.find_pack(operand);
            let len = self.pack_length(pack);
            self.out.push_u64(len);
            return;
        }
        if code == Some(*b"sP") {
            let len = self.args_length(operand);
            self.out.push_u64(len);
            return;
        }
        if let Some(&Node::Cast(ty)) = self.node(op) {
            self.out.push("(");
            self.comp(ty);
            self.out.push(")");
        } else {
            self.expr_op(op);
        }
        match code {
            Some(c) if &c == b"gs" => self.comp(operand),
            Some(c) if &c == b"st" || &c == b"nx" => {
                self.out.push("(");
                self.comp(operand);
                self.out.push(")");
            }
            _ => self.subexpr(operand),
        }
    }

    fn binary(&mut self, op: Id, left: Id, right: Id) {
        let Some(info) = self.operator_info(op) else {
            return self.fail();
        };
        let code = info.code;
        if matches!(&code, b"dc" | b"sc" | b"cc" | b"rc") {
            self.expr_op(op);
            self.out.push("<");
            self.comp(left);
            self.out.push(">(");
            self.comp(right);
            self.out.push(")");
            return;
        }
        if code[0] == b'f' {
            return self.fold(code, left, right, None);
        }
        if code[0] == b'd' && matches!(code[1], b'i' | b'x') {
            return self.designated_init(code, left, None, right);
        }
        let greater = info.name == ">";
        if greater {
            self.out.push("(");
        }
        if &code == b"cl"
            && let Some(&Node::Typed(name, function)) = self.node(left)
        {
            // A call: the callee's parameter types are not printed.
            if !matches!(self.node(function), Some(Node::Function(..))) {
                return self.fail();
            }
            self.subexpr(name);
        } else {
            self.subexpr(left);
        }
        if &code == b"ix" {
            self.out.push("[");
            self.comp(right);
            self.out.push("]");
        } else {
            if &code != b"cl" {
                self.expr_op(op);
            }
            self.subexpr(right);
        }
        if greater {
            self.out.push(")");
        }
    }

    fn trinary(&mut self, op: Id, first: Id, second: Id, third: Option<Id>) {
        let Some(info) = self.operator_info(op) else {
            return self.fail();
        };
        let code = info.code;
        if code[0] == b'f' {
            return self.fold(code, first, second, third);
        }
        if &code == b"dX" {
            return self.designated_init(code, first, Some(second), third.unwrap_or(second));
        }
        if &code == b"qu" {
            let Some(third) = third else {
                return self.fail();
            };
            self.subexpr(first);
            self.expr_op(op);
            self.subexpr(second);
            self.out.push(" : ");
            self.subexpr(third);
            return;
        }
        self.out.push("new ");
        if matches!(self.node(first), Some(Node::Args(list)) if !list.is_empty()) {
            self.subexpr(first);
            self.out.push(" ");
        }
        self.comp(second);
        if let Some(third) = third {
            self.subexpr(third);
        }
    }

    fn fold(&mut self, code: [u8; 2], operator: Id, first: Id, second: Option<Id>) {
        let save = self.pack_index;
        self.pack_index = -1;
        match code[1] {
            b'l' => {
                self.out.push("(...");
                self.expr_op(operator);
                self.subexpr(first);
                self.out.push(")");
            }
            b'r' => {
                self.out.push("(");
                self.subexpr(first);
                self.expr_op(operator);
                self.out.push("...)");
            }
            _ => {
                let Some(second) = second else {
                    self.pack_index = save;
                    return self.fail();
                };
                self.out.push("(");
                self.subexpr(first);
                self.expr_op(operator);
                self.out.push("...");
                self.expr_op(operator);
                self.subexpr(second);
                self.out.push(")");
            }
        }
        self.pack_index = save;
    }

    fn designated_init(&mut self, code: [u8; 2], first: Id, range_end: Option<Id>, value: Id) {
        if code[1] == b'i' {
            self.out.push(".");
        } else {
            self.out.push("[");
        }
        self.comp(first);
        if let Some(end) = range_end {
            self.out.push(" ... ");
            self.comp(end);
        }
        if code[1] != b'i' {
            self.out.push("]");
        }
        let chained = match self.node(value) {
            Some(&Node::Binary { op, .. } | &Node::Trinary { op, .. }) => self
                .operator_info(op)
                .is_some_and(|i| i.code[0] == b'd' && matches!(i.code[1], b'i' | b'x' | b'X')),
            _ => false,
        };
        if chained {
            self.comp(value);
        } else {
            self.out.push("=");
            self.subexpr(value);
        }
    }

    fn literal(&mut self, ty: Id, value: Id, negative: bool) {
        let style = match self.node(ty) {
            Some(Node::Builtin(builtin)) => builtin.style,
            _ => LiteralStyle::Default,
        };
        let suffix = match style {
            LiteralStyle::Int => Some(""),
            LiteralStyle::Unsigned => Some("u"),
            LiteralStyle::Long => Some("l"),
            LiteralStyle::UnsignedLong => Some("ul"),
            LiteralStyle::LongLong => Some("ll"),
            LiteralStyle::UnsignedLongLong => Some("ull"),
            _ => None,
        };
        if let Some(suffix) = suffix
            && matches!(self.node(value), Some(Node::Name(_)))
        {
            if negative {
                self.out.push("-");
            }
            self.comp(value);
            self.out.push(suffix);
            return;
        }
        if style == LiteralStyle::Bool
            && !negative
            && let Some(Node::Name(text)) = self.node(value)
        {
            match *text {
                b"0" => return self.out.push("false"),
                b"1" => return self.out.push("true"),
                _ => {}
            }
        }
        self.out.push("(");
        self.comp(ty);
        self.out.push(")");
        if negative {
            self.out.push("-");
        }
        let float = style == LiteralStyle::Float;
        if float {
            self.out.push("[");
        }
        self.comp(value);
        if float {
            self.out.push("]");
        }
    }

    fn pack_expansion(&mut self, inner: Id) {
        let Some(pack) = self.find_pack(inner) else {
            self.subexpr(inner);
            self.out.push("...");
            return;
        };
        let len = self.pack_length(Some(pack));
        for i in 0..len {
            self.pack_index = i64::try_from(i).unwrap_or(i64::MAX);
            self.comp(inner);
            if i.saturating_add(1) < len {
                self.out.push(", ");
            }
        }
    }

    fn pack_length(&self, pack: Option<Id>) -> u64 {
        match pack.and_then(|p| self.node(p)) {
            Some(Node::TemplateArgs(list)) => u64::try_from(list.len()).unwrap_or(u64::MAX),
            _ => 0,
        }
    }

    fn args_length(&mut self, args: Id) -> u64 {
        let Some(Node::TemplateArgs(list)) = self.node(args) else {
            return 0;
        };
        let mut count: u64 = 0;
        for &item in list {
            let add = match self.node(item) {
                Some(&Node::PackExpansion(inner)) => {
                    let pack = self.find_pack(inner);
                    self.pack_length(pack)
                }
                _ => 1,
            };
            count = count.saturating_add(add);
        }
        count
    }

    /// Finds the template argument pack an expansion pattern refers to.
    fn find_pack(&mut self, id: Id) -> Option<Id> {
        if !self.out.step() {
            return None;
        }
        let node = self.node(id)?;
        let children: Vec<Id> = match *node {
            Node::TemplateParam(index) => {
                let arg = self.lookup_template_argument(index)?;
                return matches!(self.node(arg), Some(Node::TemplateArgs(_))).then_some(arg);
            }
            Node::PackExpansion(_)
            | Node::Lambda { .. }
            | Node::Name(_)
            | Node::Text(_)
            | Node::AbiTag(..)
            | Node::Operator(_)
            | Node::Builtin(_)
            | Node::ExtendedBuiltin(..)
            | Node::Std(_)
            | Node::FunctionParam(_)
            | Node::Unnamed(_)
            | Node::DefaultArg(..)
            | Node::Number(_) => return None,
            Node::VendorOperator(_, name) | Node::Ctor(name) | Node::Dtor(name) => vec![name],
            _ => self.children(id),
        };
        for child in children {
            if self.depth >= MAX_DEPTH {
                return None;
            }
            self.depth = self.depth.saturating_add(1);
            let found = self.find_pack(child);
            self.depth = self.depth.saturating_sub(1);
            if found.is_some() {
                return found;
            }
        }
        None
    }

    /// The children of a node, in left-to-right order.
    fn children(&self, id: Id) -> Vec<Id> {
        let Some(node) = self.node(id) else {
            return Vec::new();
        };
        match *node {
            Node::Qual(a, b)
            | Node::Local(a, b)
            | Node::Typed(a, b)
            | Node::Template(a, b)
            | Node::CtorVtable(a, b)
            | Node::PtrMem(a, b)
            | Node::Vector(a, b)
            | Node::VendorQual(a, b)
            | Node::Literal(a, b, _)
            | Node::VendorExpr(a, b)
            | Node::Clone(a, b)
            | Node::ModuleEntity(a, b) => vec![a, b],
            Node::Special(_, a)
            | Node::Global(_, a)
            | Node::RefTemp(a, _)
            | Node::Pointer(a)
            | Node::Reference(a)
            | Node::RvalueReference(a)
            | Node::Complex(a)
            | Node::Imaginary(a)
            | Node::BitInt(_, a)
            | Node::VendorType(a)
            | Node::Cast(a)
            | Node::Conversion(a)
            | Node::Nullary(a)
            | Node::Decltype(a)
            | Node::Friend(a) => vec![a],
            Node::Qualified(qual, inner) => match qual {
                Qual::Noexcept(Some(e)) | Qual::Throw(e) => vec![inner, e],
                _ => vec![inner],
            },
            Node::Function(ret, params) => ret.into_iter().chain([params]).collect(),
            Node::Array(dim, elem) => dim.into_iter().chain([elem]).collect(),
            Node::Args(ref list) | Node::TemplateArgs(ref list) | Node::Binding(ref list) => {
                list.clone()
            }
            Node::InitList(ty, list) => ty.into_iter().chain([list]).collect(),
            Node::Unary {
                op,
                operand,
                postfix,
            } => {
                if postfix {
                    vec![op, operand, operand]
                } else {
                    vec![op, operand]
                }
            }
            Node::Binary { op, left, right } => vec![op, left, right],
            Node::Trinary {
                op,
                first,
                second,
                third,
            } => [op, first, second].into_iter().chain(third).collect(),
            Node::ModuleName(parent, name, _) => parent.into_iter().chain([name]).collect(),
            Node::TemplateParamDecl(decl) => match decl {
                ParamDecl::Type => Vec::new(),
                ParamDecl::NonType(a) | ParamDecl::Template(a) | ParamDecl::Pack(a) => vec![a],
            },
            _ => Vec::new(),
        }
    }
}
