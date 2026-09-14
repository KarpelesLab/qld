//! The syntax tree the Itanium parser builds and the printer walks.
//!
//! Nodes live in an arena (`Vec<Node>`) and refer to each other by index, so
//! substitutions and template parameters can share subtrees. The tree is
//! therefore a DAG, and the printer may visit a node many times.
//!
//! The node kinds follow the shape `c++filt` output is defined by: names,
//! qualifiers that wrap what they qualify, and expression operators that
//! carry their operator as a separate node.

/// Index of a node in the arena.
pub(super) type Id = u32;

/// How an operator is written in an expression.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Operator {
    /// Two-letter mangled code.
    pub(super) code: [u8; 2],
    /// Spelling. Word operators keep a trailing space for expressions
    /// (`sizeof `), which is dropped after `operator`.
    pub(super) name: &'static str,
    /// Number of operands in an expression.
    pub(super) arity: u8,
}

impl Operator {
    pub(super) fn is(&self, code: &[u8; 2]) -> bool {
        &self.code == code
    }
}

/// How a builtin type's literals are written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LiteralStyle {
    /// `(type)value`.
    Default,
    /// `value`.
    Int,
    /// `valueu`.
    Unsigned,
    /// `valuel`.
    Long,
    /// `valueul`.
    UnsignedLong,
    /// `valuell`.
    LongLong,
    /// `valueull`.
    UnsignedLongLong,
    /// `true`/`false`.
    Bool,
    /// `(type)[hex]`.
    Float,
    /// The `v` builtin: dropped from single-element parameter lists.
    Void,
}

/// A builtin type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Builtin {
    pub(super) name: &'static str,
    pub(super) style: LiteralStyle,
}

/// Qualifiers that wrap the node they qualify.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Qual {
    Restrict,
    Volatile,
    Const,
    /// Qualifiers of a member function (`this`).
    RestrictThis,
    VolatileThis,
    ConstThis,
    /// `&` ref-qualifier of a member function.
    RefThis,
    /// `&&` ref-qualifier of a member function.
    RvalueRefThis,
    TransactionSafe,
    /// `noexcept`, with an optional expression.
    Noexcept(Option<Id>),
    /// `throw(types)`, with the type list.
    Throw(Id),
}

impl Qual {
    /// Whether this qualifies a function type rather than an object type.
    pub(super) fn is_function_qualifier(self) -> bool {
        !matches!(self, Self::Restrict | Self::Volatile | Self::Const)
    }
}

/// A template parameter declaration in a lambda's template head.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ParamDecl {
    /// `Ty`: `typename $T`.
    Type,
    /// `Tn <type>`: `type $N`.
    NonType(Id),
    /// `Tt <decl>* E`: `template<decls> class $TT`, with a
    /// [`Node::TemplateArgs`] of declarations.
    Template(Id),
    /// `Tp <decl>`: a pack of the inner declaration.
    Pack(Id),
}

/// A syntax tree node.
#[derive(Clone, Debug)]
pub(super) enum Node<'a> {
    /// An identifier from the input.
    Name(&'a [u8]),
    /// A name with fixed spelling: `std`, `auto`, `(anonymous namespace)`.
    Text(&'static str),
    /// `scope::name`.
    Qual(Id, Id),
    /// `function::entity`.
    Local(Id, Id),
    /// A function encoding: name and function type.
    Typed(Id, Id),
    /// `name<args>`: the arguments are a [`Node::TemplateArgs`].
    Template(Id, Id),
    /// `T_` is 0.
    TemplateParam(u64),
    /// `{parm#N}`; 0 is `this`.
    FunctionParam(u64),
    /// A constructor, printing the given class name.
    Ctor(Id),
    /// A destructor, printing `~` and the given class name.
    Dtor(Id),
    /// `vtable for X` and the other special names.
    Special(&'static str, Id),
    /// `construction vtable for A-in-B`.
    CtorVtable(Id, Id),
    /// `reference temporary #N for X`: name and number.
    RefTemp(Id, u64),
    /// A standard substitution, with its expansion.
    Std(&'static str),
    /// A qualifier wrapping a type or name.
    Qualified(Qual, Id),
    Pointer(Id),
    Reference(Id),
    RvalueReference(Id),
    Complex(Id),
    Imaginary(Id),
    Builtin(&'static Builtin),
    /// `_FloatN`, `_FloatNx`: name, number and suffix.
    ExtendedBuiltin(&'static str, u64, Option<u8>),
    /// `_BitInt(N)`.
    BitInt(bool, Id),
    /// A vendor extended type.
    VendorType(Id),
    /// A function type: return type and parameter list
    /// ([`Node::Args`]).
    Function(Option<Id>, Id),
    /// `elem [dim]`.
    Array(Option<Id>, Id),
    /// Pointer to member: class and member type.
    PtrMem(Id, Id),
    /// `elem __vector(dim)`.
    Vector(Id, Id),
    /// A vendor qualifier: qualified type and qualifier name.
    VendorQual(Id, Id),
    /// Function parameters or an expression list.
    Args(Vec<Id>),
    /// Template arguments, or an argument pack.
    TemplateArgs(Vec<Id>),
    /// `type{elems}`: optional type and [`Node::Args`].
    InitList(Option<Id>, Id),
    Operator(&'static Operator),
    /// `operator name`: operand count and name.
    VendorOperator(u8, Id),
    /// `(type)` in an expression.
    Cast(Id),
    /// `operator type`.
    Conversion(Id),
    /// An operator with no operand.
    Nullary(Id),
    /// Operator and operand; `postfix` for `x++`.
    Unary {
        op: Id,
        operand: Id,
        postfix: bool,
    },
    Binary {
        op: Id,
        left: Id,
        right: Id,
    },
    Trinary {
        op: Id,
        first: Id,
        second: Id,
        third: Option<Id>,
    },
    /// A literal: type, value text, negative.
    Literal(Id, Id, bool),
    /// Vendor expression: name and arguments.
    VendorExpr(Id, Id),
    Number(u64),
    /// `decltype (expr)`.
    Decltype(Id),
    PackExpansion(Id),
    /// `global constructors keyed to X` (or destructors).
    Global(&'static str, Id),
    /// `{lambda<head>(params)#N}`: template head ([`Node::TemplateArgs`] of
    /// [`Node::TemplateParamDecl`]), parameters ([`Node::Args`]) and number.
    Lambda {
        head: Option<Id>,
        params: Id,
        number: u64,
    },
    /// `{unnamed type#N}`.
    Unnamed(u64),
    /// `encoding [clone .suffix]`.
    Clone(Id, Id),
    /// `name[abi:tag]`.
    AbiTag(Id, Id),
    /// `[a, b]`.
    Binding(Vec<Id>),
    /// `name@module`.
    ModuleEntity(Id, Id),
    /// A module name: parent, name and whether it is a partition.
    ModuleName(Option<Id>, Id, bool),
    /// `{default arg#N}::entity`.
    DefaultArg(u64, Id),
    /// `name[friend]`.
    Friend(Id),
    /// A lambda's template parameter declaration.
    TemplateParamDecl(ParamDecl),
}
