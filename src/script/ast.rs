//! Syntax tree of a parsed linker script.
//!
//! The tree keeps the script's structure and spelling (assignment operators,
//! sort keywords, section types) so later stages can apply GNU ld's rules and
//! print useful diagnostics. Names are raw bytes: scripts are not required to
//! be UTF-8. Every statement carries a [`Span`].

use std::path::{Path, PathBuf};

use super::pattern::Pattern;

/// Where a statement starts: file, line and column.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Span {
    /// Index into [`Script::files`]: 0 for the main script, higher for
    /// `INCLUDE`d files.
    pub file: u32,
    /// 1-based line number.
    pub line: u32,
    /// 1-based byte column.
    pub column: u32,
    /// Byte offset within the file.
    pub offset: u32,
}

/// A parsed linker script, with any `INCLUDE`d files spliced in.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Script {
    /// Top-level commands, in source order.
    pub commands: Vec<Command>,
    /// The main script's path followed by the path of every included file,
    /// indexed by [`Span::file`].
    pub files: Vec<PathBuf>,
}

impl Script {
    /// The path of the file a span points into.
    #[must_use]
    pub fn file_of(&self, span: Span) -> &Path {
        usize::try_from(span.file)
            .ok()
            .and_then(|i| self.files.get(i))
            .map_or(Path::new(""), PathBuf::as_path)
    }

    /// Builds an error located at `span`, for problems found after parsing
    /// (for example while evaluating an expression).
    #[must_use]
    pub fn error_at(&self, span: Span, message: impl Into<String>) -> super::ScriptError {
        super::ScriptError::new(
            self.file_of(span),
            span.line,
            span.column,
            u64::from(span.offset),
            message,
        )
    }
}

/// A top-level command with its position.
#[derive(Clone, Debug, PartialEq)]
pub struct Command {
    /// Where the command starts.
    pub span: Span,
    /// What the command is.
    pub kind: CommandKind,
}

/// The top-level commands of the language.
#[derive(Clone, Debug, PartialEq)]
pub enum CommandKind {
    /// `ENTRY(symbol)`.
    Entry(Vec<u8>),
    /// `INPUT(files)`.
    Input(Vec<InputFile>),
    /// `GROUP(files)`: searched repeatedly like `--start-group`.
    Group(Vec<InputFile>),
    /// `LIB(files)` (GNU ld 2.46+): files treated as archive members.
    Lib(Vec<InputFile>),
    /// `OUTPUT(filename)`.
    Output(Vec<u8>),
    /// `SEARCH_DIR(path)`.
    SearchDir(Vec<u8>),
    /// `STARTUP(filename)`.
    Startup(Vec<u8>),
    /// `OUTPUT_FORMAT(default)` or `OUTPUT_FORMAT(default, big, little)`.
    OutputFormat {
        /// Format used when no `-EB`/`-EL` is given.
        default: Vec<u8>,
        /// Format for `-EB`, when given.
        big: Option<Vec<u8>>,
        /// Format for `-EL`, when given.
        little: Option<Vec<u8>>,
    },
    /// `OUTPUT_ARCH(arch)`.
    OutputArch(Vec<u8>),
    /// `TARGET(bfdname)`.
    Target(Vec<u8>),
    /// `MAP(filename)`.
    Map(Vec<u8>),
    /// `EXTERN(symbol ...)`.
    Extern(Vec<Vec<u8>>),
    /// `FORCE_COMMON_ALLOCATION`.
    ForceCommonAllocation,
    /// `FORCE_GROUP_ALLOCATION`.
    ForceGroupAllocation,
    /// `INHIBIT_COMMON_ALLOCATION`.
    InhibitCommonAllocation,
    /// `NOCROSSREFS(section ...)`.
    NoCrossRefs(Vec<Vec<u8>>),
    /// `NOCROSSREFS_TO(target section ...)`: the first section may not be
    /// referenced from the others.
    NoCrossRefsTo(Vec<Vec<u8>>),
    /// `REGION_ALIAS(alias, region)`.
    RegionAlias {
        /// The new name.
        alias: Vec<u8>,
        /// The existing memory region.
        region: Vec<u8>,
    },
    /// `INSERT AFTER section` or `INSERT BEFORE section`.
    Insert {
        /// Whether the script's `SECTIONS` go before or after the section.
        position: InsertPosition,
        /// The output section of the default script to insert relative to.
        section: Vec<u8>,
    },
    /// `LD_FEATURE(name)`, e.g. `SANE_EXPR`.
    LdFeature(Vec<u8>),
    /// A symbol assignment outside `SECTIONS`.
    Assignment(Assignment),
    /// `ASSERT(expr, message)`.
    Assert(Assert),
    /// `SECTIONS { ... }`.
    Sections(Vec<SectionsCommand>),
    /// `OVERWRITE_SECTIONS { ... }` (lld extension): output section
    /// descriptions that replace same-named ones of the default layout.
    OverwriteSections(Vec<SectionsCommand>),
    /// `MEMORY { ... }`.
    Memory(Vec<MemoryRegion>),
    /// `PHDRS { ... }`.
    Phdrs(Vec<Phdr>),
    /// `VERSION { ... }`.
    Version(Vec<VersionNode>),
}

/// Placement of an `INSERT` command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InsertPosition {
    /// `INSERT AFTER`.
    After,
    /// `INSERT BEFORE`.
    Before,
}

/// One entry of an `INPUT`, `GROUP` or `LIB` list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputFile {
    /// What to look for.
    pub name: InputName,
    /// Inside `AS_NEEDED(...)`.
    pub as_needed: bool,
}

/// How an input list names a file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InputName {
    /// A path as written. A leading `=` or `$SYSROOT` means sysroot-relative;
    /// that is left to input resolution.
    Path(Vec<u8>),
    /// `-lname`: the library name without the `-l`.
    Library(Vec<u8>),
}

/// A symbol assignment: `sym = expr;`, `sym += expr;`, `PROVIDE(sym = expr);`
/// and so on. The target `.` is the location counter.
#[derive(Clone, Debug, PartialEq)]
pub struct Assignment {
    /// The symbol assigned, or `.`.
    pub target: Vec<u8>,
    /// The operator. `PROVIDE`, `PROVIDE_HIDDEN` and `HIDDEN` only take `=`.
    pub op: AssignOp,
    /// The right-hand side.
    pub expr: Expr,
    /// Plain, `HIDDEN`, `PROVIDE` or `PROVIDE_HIDDEN`.
    pub kind: AssignKind,
}

impl Assignment {
    /// Whether this assigns the location counter.
    #[must_use]
    pub fn is_dot(&self) -> bool {
        self.target == b"."
    }
}

/// Assignment operators.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssignOp {
    /// `=`
    Assign,
    /// `+=`
    Add,
    /// `-=`
    Sub,
    /// `*=`
    Mul,
    /// `/=`
    Div,
    /// `<<=`
    Shl,
    /// `>>=`
    Shr,
    /// `&=`
    And,
    /// `|=`
    Or,
    /// `^=`
    Xor,
}

impl AssignOp {
    /// The binary operator a compound assignment applies, or `None` for `=`.
    #[must_use]
    pub fn binary(self) -> Option<BinaryOp> {
        Some(match self {
            Self::Assign => return None,
            Self::Add => BinaryOp::Add,
            Self::Sub => BinaryOp::Sub,
            Self::Mul => BinaryOp::Mul,
            Self::Div => BinaryOp::Div,
            Self::Shl => BinaryOp::Shl,
            Self::Shr => BinaryOp::Shr,
            Self::And => BinaryOp::And,
            Self::Or => BinaryOp::Or,
            Self::Xor => BinaryOp::Xor,
        })
    }
}

/// Visibility and definition rule of an assignment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssignKind {
    /// `sym = expr`: always defines the symbol.
    Normal,
    /// `HIDDEN(sym = expr)`: defines a hidden symbol.
    Hidden,
    /// `PROVIDE(sym = expr)`: defines the symbol only if it is referenced and
    /// not defined by an input.
    Provide,
    /// `PROVIDE_HIDDEN(sym = expr)`: like `PROVIDE`, and hidden.
    ProvideHidden,
}

/// `ASSERT(expr, message)`.
#[derive(Clone, Debug, PartialEq)]
pub struct Assert {
    /// The condition; the link fails when it evaluates to zero.
    pub expr: Expr,
    /// The message printed on failure.
    pub message: Vec<u8>,
}

/// A command inside `SECTIONS` or `OVERWRITE_SECTIONS`.
#[derive(Clone, Debug, PartialEq)]
pub struct SectionsCommand {
    /// Where the command starts.
    pub span: Span,
    /// What the command is.
    pub kind: SectionsCommandKind,
}

/// Commands allowed inside `SECTIONS`.
#[derive(Clone, Debug, PartialEq)]
pub enum SectionsCommandKind {
    /// A symbol or location counter assignment.
    Assignment(Assignment),
    /// `ENTRY(symbol)`.
    Entry(Vec<u8>),
    /// `ASSERT(expr, message)`.
    Assert(Assert),
    /// An output section description.
    OutputSection(Box<OutputSection>),
    /// An `OVERLAY` description.
    Overlay(Box<Overlay>),
}

/// An output section description:
///
/// ```text
/// name [address] [(type)] : [AT(lma)] [ALIGN(align)] [ALIGN_WITH_INPUT]
///     [SUBALIGN(subalign)] [constraint]
///   { commands }
///   [>region] [AT>lma_region] [:phdr ...] [=fill] [,]
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct OutputSection {
    /// The section name; `/DISCARD/` discards what it matches.
    pub name: Vec<u8>,
    /// Explicit start address (VMA).
    pub address: Option<Expr>,
    /// The `(type)` after the address.
    pub section_type: OutputSectionType,
    /// `AT(lma)`.
    pub load_address: Option<Expr>,
    /// `ALIGN(align)`.
    pub align: Option<Expr>,
    /// `ALIGN_WITH_INPUT`.
    pub align_with_input: bool,
    /// `SUBALIGN(subalign)`.
    pub subalign: Option<Expr>,
    /// `ONLY_IF_RO`, `ONLY_IF_RW` or `SPECIAL`.
    pub constraint: SectionConstraint,
    /// The statements between the braces.
    pub commands: Vec<OutputSectionCommand>,
    /// `>region`.
    pub region: Option<Vec<u8>>,
    /// `AT>lma_region`.
    pub load_region: Option<Vec<u8>>,
    /// `:phdr` names, in order.
    pub phdrs: Vec<Vec<u8>>,
    /// `=fill`.
    pub fill: Option<Fill>,
}

impl OutputSection {
    /// Whether this is the `/DISCARD/` pseudo-section.
    #[must_use]
    pub fn is_discard(&self) -> bool {
        self.name == b"/DISCARD/"
    }
}

/// The `(type)` of an output section.
#[derive(Clone, Debug, PartialEq, Default)]
pub enum OutputSectionType {
    /// No type given.
    #[default]
    Normal,
    /// `(NOLOAD)`: allocated but not loaded.
    NoLoad,
    /// `(DSECT)`: not allocated (obsolete synonym of `INFO`).
    DSect,
    /// `(COPY)`: not allocated (obsolete synonym of `INFO`).
    Copy,
    /// `(INFO)`: not allocated.
    Info,
    /// `(OVERLAY)`: not allocated (obsolete synonym of `INFO`).
    Overlay,
    /// `(READONLY)`.
    ReadOnly,
    /// `(TYPE = type)`: the ELF section type, a number or `SHT_*` name.
    Type(Expr),
    /// `(READONLY (TYPE = type))`.
    ReadOnlyType(Expr),
}

/// Output section constraint.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SectionConstraint {
    /// None given.
    #[default]
    None,
    /// `ONLY_IF_RO`: create only if all inputs are read-only.
    OnlyIfRo,
    /// `ONLY_IF_RW`: create only if some input is writable.
    OnlyIfRw,
    /// `SPECIAL`.
    Special,
}

/// A fill pattern: `=fill` after an output section or `FILL(fill)`.
#[derive(Clone, Debug, PartialEq)]
pub struct Fill {
    /// The expression.
    pub expr: Expr,
    /// When the expression is a bare `0x` literal, its hex digits. GNU ld
    /// then uses all of them (one byte per two digits) instead of a four
    /// byte big-endian value. See `eval::fill_pattern`.
    pub hex_digits: Option<Vec<u8>>,
}

/// An `OVERLAY` description:
///
/// ```text
/// OVERLAY [start] : [NOCROSSREFS] [AT(lma)] [SUBALIGN(align)]
///   { name { commands } [:phdr ...] [=fill] ... }
///   [>region] [AT>lma_region] [:phdr ...] [=fill] [,]
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct Overlay {
    /// Start address of every section in the overlay.
    pub address: Option<Expr>,
    /// `NOCROSSREFS`.
    pub no_cross_refs: bool,
    /// `AT(lma)`: load address of the first section.
    pub load_address: Option<Expr>,
    /// `SUBALIGN(align)`.
    pub subalign: Option<Expr>,
    /// The sections.
    pub sections: Vec<OverlaySection>,
    /// `>region`.
    pub region: Option<Vec<u8>>,
    /// `AT>lma_region`.
    pub load_region: Option<Vec<u8>>,
    /// `:phdr` names.
    pub phdrs: Vec<Vec<u8>>,
    /// `=fill`.
    pub fill: Option<Fill>,
}

/// One section of an `OVERLAY`.
#[derive(Clone, Debug, PartialEq)]
pub struct OverlaySection {
    /// Where the section starts.
    pub span: Span,
    /// The section name.
    pub name: Vec<u8>,
    /// Statements between the braces.
    pub commands: Vec<OutputSectionCommand>,
    /// `:phdr` names.
    pub phdrs: Vec<Vec<u8>>,
    /// `=fill`.
    pub fill: Option<Fill>,
}

/// A statement inside an output section description.
#[derive(Clone, Debug, PartialEq)]
pub struct OutputSectionCommand {
    /// Where the statement starts.
    pub span: Span,
    /// What the statement is.
    pub kind: OutputSectionCommandKind,
}

/// Statements allowed inside an output section description.
#[derive(Clone, Debug, PartialEq)]
pub enum OutputSectionCommandKind {
    /// A symbol or location counter assignment.
    Assignment(Assignment),
    /// An input section description such as `KEEP(*(.init))`.
    Input(InputSectionDescription),
    /// `BYTE(expr)`, `SHORT`, `LONG`, `QUAD` or `SQUAD`.
    Data {
        /// Which command, which fixes the size.
        size: DataSize,
        /// The value stored.
        expr: Expr,
    },
    /// `FILL(expr)`.
    Fill(Fill),
    /// `ASCIZ "string"`: a NUL-terminated string.
    Asciz(Vec<u8>),
    /// `LINKER_VERSION`: the linker's version string.
    LinkerVersion,
    /// `CREATE_OBJECT_SYMBOLS`.
    CreateObjectSymbols,
    /// `CONSTRUCTORS` or `SORT(CONSTRUCTORS)`.
    Constructors {
        /// Written as `SORT(CONSTRUCTORS)` or `SORT_BY_NAME(CONSTRUCTORS)`.
        sorted: bool,
    },
    /// `ASSERT(expr, message)`.
    Assert(Assert),
}

/// Size of a data command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DataSize {
    /// `BYTE`: 1 byte.
    Byte,
    /// `SHORT`: 2 bytes.
    Short,
    /// `LONG`: 4 bytes.
    Long,
    /// `QUAD`: 8 bytes.
    Quad,
    /// `SQUAD`: 8 bytes, sign-extended on 32-bit hosts in GNU ld.
    SQuad,
}

impl DataSize {
    /// Size in bytes.
    #[must_use]
    pub fn bytes(self) -> u64 {
        match self {
            Self::Byte => 1,
            Self::Short => 2,
            Self::Long => 4,
            Self::Quad | Self::SQuad => 8,
        }
    }
}

/// An input section description:
///
/// ```text
/// [KEEP(] [INPUT_SECTION_FLAGS(flags)] file_spec [(section_spec ...)] [)]
/// ```
///
/// Use [`InputSectionDescription::matches`] to test an input section.
#[derive(Clone, Debug, PartialEq)]
pub struct InputSectionDescription {
    /// Wrapped in `KEEP(...)`: exempt from garbage collection.
    pub keep: bool,
    /// `INPUT_SECTION_FLAGS(...)` requirements.
    pub flags: Vec<SectionFlag>,
    /// Which files.
    pub file: FileSpec,
    /// Which sections. `None` for a bare file name (`foo.o` alone means all
    /// of its sections). The old `[ .a .b ]` form has `file` matching all.
    pub sections: Option<Vec<SectionSpec>>,
}

/// One `INPUT_SECTION_FLAGS` entry, such as `SHF_WRITE` or `!SHF_WRITE`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SectionFlag {
    /// The flag name, e.g. `SHF_ALLOC`, or a number.
    pub name: Vec<u8>,
    /// Written with `!`: the flag must be clear.
    pub negated: bool,
}

/// The file part of an input section description.
#[derive(Clone, Debug, PartialEq)]
pub struct FileSpec {
    /// The file name pattern; `archive:member` forms are handled by
    /// [`FileSpec::matches`].
    pub pattern: Pattern,
    /// `EXCLUDE_FILE(...)` before the file name.
    pub exclude: Vec<Pattern>,
    /// Sort mode for the files matched: [`SortMode::None`], [`SortMode::Name`]
    /// or [`SortMode::NoSort`].
    pub sort: SortMode,
    /// `REVERSE(...)`.
    pub reverse: bool,
}

/// One section pattern inside the parentheses of an input section
/// description.
#[derive(Clone, Debug, PartialEq)]
pub struct SectionSpec {
    /// The section name pattern.
    pub pattern: Pattern,
    /// `EXCLUDE_FILE(...)` written before this pattern.
    pub exclude_files: Vec<Pattern>,
    /// How matching sections are sorted.
    pub sort: SortMode,
    /// `REVERSE(...)`.
    pub reverse: bool,
}

/// Sorting requested with the `SORT_*` keywords.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SortMode {
    /// No sort keyword: input order.
    #[default]
    None,
    /// `SORT` or `SORT_BY_NAME`.
    Name,
    /// `SORT_BY_ALIGNMENT`: decreasing alignment.
    Alignment,
    /// `SORT_BY_NAME(SORT_BY_ALIGNMENT(...))`.
    NameAlignment,
    /// `SORT_BY_ALIGNMENT(SORT_BY_NAME(...))`.
    AlignmentName,
    /// `SORT_BY_INIT_PRIORITY`: by the number in `.init_array.N` names.
    InitPriority,
    /// `SORT_NONE`: input order even when `--sort-section` is given.
    NoSort,
}

/// A `MEMORY` region: `name [(attributes)] : ORIGIN = expr, LENGTH = expr`.
#[derive(Clone, Debug, PartialEq)]
pub struct MemoryRegion {
    /// Where the region starts.
    pub span: Span,
    /// The region name.
    pub name: Vec<u8>,
    /// The `(rwx)` attributes.
    pub attributes: MemoryAttributes,
    /// `ORIGIN`.
    pub origin: Expr,
    /// `LENGTH`.
    pub length: Expr,
}

/// Memory region attributes: which kinds of sections a region accepts.
///
/// `flags` holds attributes listed plainly and `not_flags` those after a
/// `!`; each is a set of the `MEMORY_*` bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct MemoryAttributes {
    /// Attributes a section may have to be placed here.
    pub flags: u8,
    /// Attributes a section must not have to be placed here.
    pub not_flags: u8,
}

impl MemoryAttributes {
    /// `R`: read-only sections.
    pub const READ_ONLY: u8 = 1;
    /// `W`: read/write sections.
    pub const WRITE: u8 = 2;
    /// `X`: executable sections.
    pub const EXEC: u8 = 4;
    /// `A`: allocatable sections.
    pub const ALLOC: u8 = 8;
    /// `I` or `L`: initialized (loaded) sections.
    pub const LOAD: u8 = 16;
}

/// A `PHDRS` entry:
/// `name type [FILEHDR] [PHDRS] [AT(address)] [FLAGS(flags)];`.
#[derive(Clone, Debug, PartialEq)]
pub struct Phdr {
    /// Where the entry starts.
    pub span: Span,
    /// The name output sections refer to with `:name`.
    pub name: Vec<u8>,
    /// The segment type. `PT_*` names are already turned into numbers.
    pub phdr_type: Expr,
    /// `FILEHDR`: the segment includes the ELF file header.
    pub filehdr: bool,
    /// `PHDRS`: the segment includes the program headers.
    pub phdrs: bool,
    /// `AT(address)`: physical address.
    pub at: Option<Expr>,
    /// `FLAGS(flags)`: `p_flags`.
    pub flags: Option<Expr>,
}

/// A version node in a `VERSION` command or version script.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct VersionNode {
    /// Where the node starts.
    pub span: Span,
    /// The version name; `None` for the anonymous `{ ... };` node.
    pub name: Option<Vec<u8>>,
    /// Patterns of symbols given this version.
    pub globals: Vec<VersionPattern>,
    /// Patterns of symbols made local.
    pub locals: Vec<VersionPattern>,
    /// Versions this one inherits from.
    pub dependencies: Vec<Vec<u8>>,
}

/// A symbol pattern in a version node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VersionPattern {
    /// The pattern text.
    pub pattern: Vec<u8>,
    /// The `extern "lang"` block it is in (`C++`, `Java`), if any.
    pub language: Option<Vec<u8>>,
    /// Written in quotes: matched literally, without wildcards.
    pub literal: bool,
}

/// An expression.
#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    /// An integer literal.
    Number(u64),
    /// A symbol reference.
    Symbol(Vec<u8>),
    /// The location counter `.`.
    Dot,
    /// A unary operator.
    Unary(UnaryOp, Box<Expr>),
    /// A binary operator.
    Binary(BinaryOp, Box<Expr>, Box<Expr>),
    /// `cond ? then : else`.
    Conditional(Box<Expr>, Box<Expr>, Box<Expr>),
    /// `ABSOLUTE(expr)`.
    Absolute(Box<Expr>),
    /// `ADDR(section)`.
    Addr(Vec<u8>),
    /// `ALIGN(align)`: `.` rounded up.
    Align(Box<Expr>),
    /// `ALIGN(expr, align)`.
    AlignExpr(Box<Expr>, Box<Expr>),
    /// `ALIGNOF(section)`.
    AlignOf(Vec<u8>),
    /// `BLOCK(align)`: synonym of `ALIGN(align)`.
    Block(Box<Expr>),
    /// `DATA_SEGMENT_ALIGN(maxpagesize, commonpagesize)`.
    DataSegmentAlign(Box<Expr>, Box<Expr>),
    /// `DATA_SEGMENT_END(expr)`.
    DataSegmentEnd(Box<Expr>),
    /// `DATA_SEGMENT_RELRO_END(offset, expr)`.
    DataSegmentRelroEnd(Box<Expr>, Box<Expr>),
    /// `DEFINED(symbol)`.
    Defined(Vec<u8>),
    /// `LENGTH(region)`.
    Length(Vec<u8>),
    /// `LOADADDR(section)`.
    LoadAddr(Vec<u8>),
    /// `LOG2CEIL(expr)`.
    Log2Ceil(Box<Expr>),
    /// `MAX(a, b)`.
    Max(Box<Expr>, Box<Expr>),
    /// `MIN(a, b)`.
    Min(Box<Expr>, Box<Expr>),
    /// `NEXT(align)`.
    Next(Box<Expr>),
    /// `ORIGIN(region)`.
    Origin(Vec<u8>),
    /// `SEGMENT_START(segment, default)`.
    SegmentStart(Vec<u8>, Box<Expr>),
    /// `SIZEOF(section)`.
    SizeOf(Vec<u8>),
    /// `SIZEOF_HEADERS`.
    SizeOfHeaders,
    /// `CONSTANT(MAXPAGESIZE)` or `CONSTANT(COMMONPAGESIZE)`; other names are
    /// kept and rejected at evaluation, as GNU ld does.
    Constant(Vec<u8>),
    /// `ASSERT(expr, message)` used as a value.
    Assert(Box<Expr>, Vec<u8>),
}

impl Expr {
    /// Calls `f` with the name of every symbol the expression reads, in
    /// source order, including those in `DEFINED`. Symbol resolution uses
    /// this to treat script references as undefined references.
    pub fn for_each_symbol(&self, f: &mut dyn FnMut(&[u8])) {
        match self {
            Self::Symbol(name) | Self::Defined(name) => f(name),
            Self::Number(_)
            | Self::Dot
            | Self::Addr(_)
            | Self::AlignOf(_)
            | Self::Length(_)
            | Self::LoadAddr(_)
            | Self::Origin(_)
            | Self::SizeOf(_)
            | Self::SizeOfHeaders
            | Self::Constant(_) => {}
            Self::Unary(_, a)
            | Self::Absolute(a)
            | Self::Align(a)
            | Self::Block(a)
            | Self::DataSegmentEnd(a)
            | Self::Log2Ceil(a)
            | Self::Next(a)
            | Self::SegmentStart(_, a)
            | Self::Assert(a, _) => a.for_each_symbol(f),
            Self::Binary(_, a, b)
            | Self::AlignExpr(a, b)
            | Self::DataSegmentAlign(a, b)
            | Self::DataSegmentRelroEnd(a, b)
            | Self::Max(a, b)
            | Self::Min(a, b) => {
                a.for_each_symbol(f);
                b.for_each_symbol(f);
            }
            Self::Conditional(c, a, b) => {
                c.for_each_symbol(f);
                a.for_each_symbol(f);
                b.for_each_symbol(f);
            }
        }
    }

    /// Calls `f` with the name of every symbol whose value the expression
    /// reads: [`Expr::for_each_symbol`] without the names only tested with
    /// `DEFINED`, which GNU ld does not treat as references.
    pub fn for_each_value_symbol(&self, f: &mut dyn FnMut(&[u8])) {
        match self {
            Self::Symbol(name) => f(name),
            Self::Unary(_, a)
            | Self::Absolute(a)
            | Self::Align(a)
            | Self::Block(a)
            | Self::DataSegmentEnd(a)
            | Self::Log2Ceil(a)
            | Self::Next(a)
            | Self::SegmentStart(_, a)
            | Self::Assert(a, _) => a.for_each_value_symbol(f),
            Self::Binary(_, a, b)
            | Self::AlignExpr(a, b)
            | Self::DataSegmentAlign(a, b)
            | Self::DataSegmentRelroEnd(a, b)
            | Self::Max(a, b)
            | Self::Min(a, b) => {
                a.for_each_value_symbol(f);
                b.for_each_value_symbol(f);
            }
            Self::Conditional(c, a, b) => {
                c.for_each_value_symbol(f);
                a.for_each_value_symbol(f);
                b.for_each_value_symbol(f);
            }
            _ => {}
        }
    }

    /// The symbol whose type an assignment of this expression copies, as
    /// GNU ld's `exp_fold_tree` does: the only symbol the expression reads
    /// (`DEFINED` does not count, nor do the conditions of `?:`; both
    /// branches do, where GNU ld counts only the one taken).
    #[must_use]
    pub fn type_source(&self) -> Option<&[u8]> {
        fn walk<'e>(expr: &'e Expr, found: &mut Option<&'e [u8]>, count: &mut u32) {
            match expr {
                Expr::Symbol(name) => {
                    *count = count.saturating_add(1);
                    *found = Some(name);
                }
                Expr::Conditional(_, a, b) => {
                    walk(a, found, count);
                    walk(b, found, count);
                }
                Expr::Unary(_, a)
                | Expr::Absolute(a)
                | Expr::Align(a)
                | Expr::Block(a)
                | Expr::DataSegmentEnd(a)
                | Expr::Log2Ceil(a)
                | Expr::Next(a)
                | Expr::SegmentStart(_, a)
                | Expr::Assert(a, _) => walk(a, found, count),
                Expr::Binary(_, a, b)
                | Expr::AlignExpr(a, b)
                | Expr::DataSegmentAlign(a, b)
                | Expr::DataSegmentRelroEnd(a, b)
                | Expr::Max(a, b)
                | Expr::Min(a, b) => {
                    walk(a, found, count);
                    walk(b, found, count);
                }
                _ => {}
            }
        }
        let mut found = None;
        let mut count = 0u32;
        walk(self, &mut found, &mut count);
        found.filter(|_| count == 1)
    }

    /// Whether the expression reads the location counter, directly or
    /// through `ALIGN`, `NEXT` or `DATA_SEGMENT_ALIGN`.
    #[must_use]
    pub fn uses_dot(&self) -> bool {
        match self {
            Self::Dot | Self::Align(_) | Self::Block(_) | Self::Next(_) => true,
            Self::DataSegmentAlign(..) => true,
            Self::Number(_)
            | Self::Symbol(_)
            | Self::Defined(_)
            | Self::Addr(_)
            | Self::AlignOf(_)
            | Self::Length(_)
            | Self::LoadAddr(_)
            | Self::Origin(_)
            | Self::SizeOf(_)
            | Self::SizeOfHeaders
            | Self::Constant(_) => false,
            Self::Unary(_, a)
            | Self::Absolute(a)
            | Self::DataSegmentEnd(a)
            | Self::Log2Ceil(a)
            | Self::SegmentStart(_, a)
            | Self::Assert(a, _) => a.uses_dot(),
            Self::Binary(_, a, b)
            | Self::AlignExpr(a, b)
            | Self::DataSegmentRelroEnd(a, b)
            | Self::Max(a, b)
            | Self::Min(a, b) => a.uses_dot() || b.uses_dot(),
            Self::Conditional(c, a, b) => c.uses_dot() || a.uses_dot() || b.uses_dot(),
        }
    }
}

/// Unary operators.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnaryOp {
    /// `-`
    Neg,
    /// `!`
    Not,
    /// `~`
    BitNot,
}

/// Binary operators, with C precedence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinaryOp {
    /// `*`
    Mul,
    /// `/` (signed)
    Div,
    /// `%` (signed)
    Rem,
    /// `+`
    Add,
    /// `-`
    Sub,
    /// `<<`
    Shl,
    /// `>>`
    Shr,
    /// `<`
    Lt,
    /// `>`
    Gt,
    /// `<=`
    Le,
    /// `>=`
    Ge,
    /// `==`
    Eq,
    /// `!=`
    Ne,
    /// `&`
    And,
    /// `^`
    Xor,
    /// `|`
    Or,
    /// `&&`
    LogicalAnd,
    /// `||`
    LogicalOr,
}

impl InputSectionDescription {
    /// Tests an input section against this description.
    ///
    /// `file` is the input file's path, or the member name for an archive
    /// member, whose archive path is then `archive`. Returns the index of
    /// the first section spec that matches (0 for a description without a
    /// section list), which tells the caller which sort mode applies.
    #[must_use]
    pub fn matches(&self, file: &[u8], archive: Option<&[u8]>, section: &[u8]) -> Option<usize> {
        if !self.file.matches(file, archive) {
            return None;
        }
        let Some(sections) = &self.sections else {
            return Some(0);
        };
        sections.iter().position(|spec| {
            spec.pattern.matches(section)
                && !spec
                    .exclude_files
                    .iter()
                    .any(|p| super::pattern::file_matches(p, file, archive, true))
        })
    }
}

impl FileSpec {
    /// Whether an input file matches the file part, including
    /// `EXCLUDE_FILE`. Arguments as in [`InputSectionDescription::matches`].
    #[must_use]
    pub fn matches(&self, file: &[u8], archive: Option<&[u8]>) -> bool {
        super::pattern::file_matches(&self.pattern, file, archive, false)
            && !self
                .exclude
                .iter()
                .any(|p| super::pattern::file_matches(p, file, archive, true))
    }
}
