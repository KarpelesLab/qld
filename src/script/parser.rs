//! Recursive-descent parser for GNU linker scripts.
//!
//! The grammar follows GNU ld's `ldgram.y`. Each token is requested in the
//! lexer state GNU would be in at that point (see [`super::lexer`]), so file
//! names, section patterns and expressions tokenize as they do in GNU ld.
//!
//! Where GNU ld rejects a script that has an obvious meaning, the parser is
//! sometimes more lenient: a stray `;` inside `SECTIONS`, an empty `INPUT()`,
//! output section attributes in any order, a keyword used as a symbol name
//! where no keyword could appear, and `local:` before `global:` in version
//! nodes. It never gives a script GNU ld accepts a different meaning.

#![deny(clippy::arithmetic_side_effects)]

use std::borrow::Cow;
use std::io;
use std::path::{Path, PathBuf};

use super::ast::*;
use super::error::ScriptError;
use super::lexer::{LexError, Lexer, Mode, Punct, Token, TokenKind};
use super::pattern::Pattern;

/// Deepest `INCLUDE` nesting accepted, as in GNU ld.
const MAX_INCLUDE_DEPTH: usize = 10;
/// Deepest expression accepted: bounds recursion in the parser, the
/// evaluator and `Drop`, so hostile input cannot overflow the stack.
pub(crate) const MAX_EXPR_DEPTH: usize = 128;
/// Deepest nesting of `AS_NEEDED` and version `extern` blocks.
const MAX_LIST_DEPTH: usize = 32;

/// Reads files named by `INCLUDE`.
///
/// The parser does no I/O itself. Tests use an in-memory implementation;
/// the linker uses [`FsReader`] with its library search path.
pub trait ScriptReader {
    /// Finds and reads the script `name` (as written after `INCLUDE`),
    /// included from the script at `from`. Returns the resolved path, used in
    /// diagnostics, and the file contents.
    ///
    /// # Errors
    ///
    /// Any I/O error, including not finding the file.
    fn read_include(&mut self, name: &[u8], from: &Path) -> io::Result<(PathBuf, Vec<u8>)>;
}

/// A [`ScriptReader`] that rejects every `INCLUDE`, for scripts that must be
/// self-contained (such as a `libc.so` input script).
#[derive(Clone, Copy, Debug, Default)]
pub struct NoIncludes;

impl ScriptReader for NoIncludes {
    fn read_include(&mut self, _name: &[u8], _from: &Path) -> io::Result<(PathBuf, Vec<u8>)> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "INCLUDE is not allowed here",
        ))
    }
}

/// A [`ScriptReader`] that reads from the file system.
///
/// Like GNU ld, it tries the name as given (relative to the current
/// directory), then each search directory in order.
#[derive(Clone, Debug, Default)]
pub struct FsReader {
    /// Directories to search, usually the `-L` paths.
    pub search_dirs: Vec<PathBuf>,
}

impl ScriptReader for FsReader {
    fn read_include(&mut self, name: &[u8], _from: &Path) -> io::Result<(PathBuf, Vec<u8>)> {
        let name = bytes_to_path(name);
        let mut candidates = vec![name.clone()];
        if name.is_relative() {
            candidates.extend(self.search_dirs.iter().map(|dir| dir.join(&name)));
        }
        let mut last_error = None;
        for candidate in candidates {
            match std::fs::read(&candidate) {
                Ok(bytes) => return Ok((candidate, bytes)),
                Err(error) => last_error = Some(error),
            }
        }
        Err(last_error.unwrap_or_else(|| io::Error::from(io::ErrorKind::NotFound)))
    }
}

#[cfg(unix)]
fn bytes_to_path(bytes: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
}

#[cfg(not(unix))]
fn bytes_to_path(bytes: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
}

/// Parses a linker script.
///
/// `path` names the script in diagnostics. `INCLUDE`d files are read through
/// `reader` and spliced in place.
///
/// # Errors
///
/// A [`ScriptError`] with the file, line and column of the first problem.
pub fn parse_script(
    source: &[u8],
    path: &Path,
    reader: &mut dyn ScriptReader,
) -> Result<Script, ScriptError> {
    let mut parser = Parser::new(source, path, reader);
    let mut commands = Vec::new();
    parser.script_commands(&mut commands)?;
    Ok(Script {
        commands,
        files: parser.files,
    })
}

/// Parses a version script, as given to `--version-script`: a sequence of
/// version nodes without the surrounding `VERSION { }`. Dynamic list files
/// (`--dynamic-list`) use the same syntax with anonymous nodes.
///
/// # Errors
///
/// A [`ScriptError`] for the first syntax error.
pub fn parse_version_script(source: &[u8], path: &Path) -> Result<Vec<VersionNode>, ScriptError> {
    let mut reader = NoIncludes;
    let mut parser = Parser::new(source, path, &mut reader);
    let nodes = parser.version_nodes()?;
    parser.expect_eof(Mode::VersionScript)?;
    Ok(nodes)
}

/// Parses a single expression, such as the value of `-Ttext` style options
/// that accept expressions.
///
/// # Errors
///
/// A [`ScriptError`] for a syntax error or trailing input.
pub fn parse_expression(source: &[u8], path: &Path) -> Result<Expr, ScriptError> {
    let mut reader = NoIncludes;
    let mut parser = Parser::new(source, path, &mut reader);
    let expr = parser.expr()?;
    parser.expect_eof(Mode::Expr)?;
    Ok(expr)
}

/// Parses the argument of `--defsym`: `symbol=expression`, tokenized as an
/// expression (so `a-b=1` is invalid, as in GNU ld).
///
/// # Errors
///
/// A [`ScriptError`] for a syntax error or trailing input.
pub fn parse_defsym(source: &[u8]) -> Result<Assignment, ScriptError> {
    let mut reader = NoIncludes;
    let mut parser = Parser::new(source, Path::new("--defsym"), &mut reader);
    let assignment = parser.assignment(Mode::Expr)?;
    parser.expect_eof(Mode::Expr)?;
    Ok(assignment)
}

/// Keywords, recognized by the parser only where they are valid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kw {
    Absolute,
    Addr,
    After,
    Align,
    AlignOf,
    AlignWithInput,
    Asciz,
    AsNeeded,
    Assert,
    At,
    Before,
    Bind,
    Block,
    Byte,
    Constant,
    Constructors,
    Copy,
    CreateObjectSymbols,
    DataSegmentAlign,
    DataSegmentEnd,
    DataSegmentRelroEnd,
    Defined,
    DSect,
    Entry,
    ExcludeFile,
    Extern,
    Fill,
    Float,
    ForceCommonAllocation,
    ForceGroupAllocation,
    Group,
    Hidden,
    Hll,
    Include,
    Info,
    InhibitCommonAllocation,
    Input,
    InputSectionFlags,
    Insert,
    Keep,
    LdFeature,
    Length,
    Lib,
    LinkerVersion,
    LoadAddr,
    Log2Ceil,
    Long,
    Map,
    Max,
    Memory,
    Min,
    Next,
    NoCrossRefs,
    NoCrossRefsTo,
    NoFloat,
    NoLoad,
    OnlyIfRo,
    OnlyIfRw,
    Origin,
    Output,
    OutputArch,
    OutputFormat,
    Overlay,
    OverwriteSections,
    Phdrs,
    Provide,
    ProvideHidden,
    Quad,
    ReadOnly,
    RegionAlias,
    Reverse,
    SearchDir,
    Sections,
    SegmentStart,
    Short,
    SizeOf,
    SizeOfHeaders,
    Sort,
    SortByAlignment,
    SortByInitPriority,
    SortNone,
    Special,
    SQuad,
    Startup,
    SubAlign,
    Syslib,
    Target,
    Type,
    Version,
}

fn keyword(text: &[u8]) -> Option<Kw> {
    Some(match text {
        b"ABSOLUTE" => Kw::Absolute,
        b"ADDR" => Kw::Addr,
        b"AFTER" => Kw::After,
        b"ALIGN" => Kw::Align,
        b"ALIGNOF" => Kw::AlignOf,
        b"ALIGN_WITH_INPUT" => Kw::AlignWithInput,
        b"ASCIZ" => Kw::Asciz,
        b"AS_NEEDED" => Kw::AsNeeded,
        b"ASSERT" => Kw::Assert,
        b"AT" => Kw::At,
        b"BEFORE" => Kw::Before,
        b"BIND" => Kw::Bind,
        b"BLOCK" => Kw::Block,
        b"BYTE" => Kw::Byte,
        b"CONSTANT" => Kw::Constant,
        b"CONSTRUCTORS" => Kw::Constructors,
        b"COPY" => Kw::Copy,
        b"CREATE_OBJECT_SYMBOLS" => Kw::CreateObjectSymbols,
        b"DATA_SEGMENT_ALIGN" => Kw::DataSegmentAlign,
        b"DATA_SEGMENT_END" => Kw::DataSegmentEnd,
        b"DATA_SEGMENT_RELRO_END" => Kw::DataSegmentRelroEnd,
        b"DEFINED" => Kw::Defined,
        b"DSECT" => Kw::DSect,
        b"ENTRY" => Kw::Entry,
        b"EXCLUDE_FILE" => Kw::ExcludeFile,
        b"EXTERN" => Kw::Extern,
        b"FILL" => Kw::Fill,
        b"FLOAT" => Kw::Float,
        b"FORCE_COMMON_ALLOCATION" => Kw::ForceCommonAllocation,
        b"FORCE_GROUP_ALLOCATION" => Kw::ForceGroupAllocation,
        b"GROUP" => Kw::Group,
        b"HIDDEN" => Kw::Hidden,
        b"HLL" => Kw::Hll,
        b"INCLUDE" => Kw::Include,
        b"INFO" => Kw::Info,
        b"INHIBIT_COMMON_ALLOCATION" => Kw::InhibitCommonAllocation,
        b"INPUT" => Kw::Input,
        b"INPUT_SECTION_FLAGS" => Kw::InputSectionFlags,
        b"INSERT" => Kw::Insert,
        b"KEEP" => Kw::Keep,
        b"LD_FEATURE" => Kw::LdFeature,
        b"LENGTH" => Kw::Length,
        b"LIB" => Kw::Lib,
        b"LINKER_VERSION" => Kw::LinkerVersion,
        b"LOADADDR" => Kw::LoadAddr,
        b"LOG2CEIL" => Kw::Log2Ceil,
        b"LONG" => Kw::Long,
        b"MAP" => Kw::Map,
        b"MAX" => Kw::Max,
        b"MEMORY" => Kw::Memory,
        b"MIN" => Kw::Min,
        b"NEXT" => Kw::Next,
        b"NOCROSSREFS" => Kw::NoCrossRefs,
        b"NOCROSSREFS_TO" => Kw::NoCrossRefsTo,
        b"NOFLOAT" => Kw::NoFloat,
        b"NOLOAD" => Kw::NoLoad,
        b"ONLY_IF_RO" => Kw::OnlyIfRo,
        b"ONLY_IF_RW" => Kw::OnlyIfRw,
        b"ORIGIN" => Kw::Origin,
        b"OUTPUT" => Kw::Output,
        b"OUTPUT_ARCH" => Kw::OutputArch,
        b"OUTPUT_FORMAT" => Kw::OutputFormat,
        b"OVERLAY" => Kw::Overlay,
        b"OVERWRITE_SECTIONS" => Kw::OverwriteSections,
        b"PHDRS" => Kw::Phdrs,
        b"PROVIDE" => Kw::Provide,
        b"PROVIDE_HIDDEN" => Kw::ProvideHidden,
        b"QUAD" => Kw::Quad,
        b"READONLY" => Kw::ReadOnly,
        b"REGION_ALIAS" => Kw::RegionAlias,
        b"REVERSE" => Kw::Reverse,
        b"SEARCH_DIR" => Kw::SearchDir,
        b"SECTIONS" => Kw::Sections,
        b"SEGMENT_START" => Kw::SegmentStart,
        b"SHORT" => Kw::Short,
        b"SIZEOF" => Kw::SizeOf,
        b"SIZEOF_HEADERS" => Kw::SizeOfHeaders,
        b"SORT" | b"SORT_BY_NAME" => Kw::Sort,
        b"SORT_BY_ALIGNMENT" => Kw::SortByAlignment,
        b"SORT_BY_INIT_PRIORITY" => Kw::SortByInitPriority,
        b"SORT_NONE" => Kw::SortNone,
        b"SPECIAL" => Kw::Special,
        b"SQUAD" => Kw::SQuad,
        b"STARTUP" => Kw::Startup,
        b"SUBALIGN" => Kw::SubAlign,
        b"SYSLIB" => Kw::Syslib,
        b"TARGET" => Kw::Target,
        b"TYPE" => Kw::Type,
        b"VERSION" => Kw::Version,
        _ => return None,
    })
}

/// Sort keywords in an input section description, outermost first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SortKw {
    Name,
    Alignment,
    InitPriority,
    NoSort,
    Reverse,
}

/// A parsed `SORT_*(... EXCLUDE_FILE(...) name ...)` wrapper.
struct WildSpec {
    chain: Vec<SortKw>,
    exclude: Vec<Pattern>,
    name: Vec<u8>,
    start: usize,
}

fn section_sort(chain: &[SortKw]) -> Option<(SortMode, bool)> {
    use SortKw::{Alignment as A, InitPriority as P, Name as N, NoSort as X, Reverse as R};
    Some(match chain {
        [] => (SortMode::None, false),
        [R] | [N, R] | [N, N, R] | [R, N] => (SortMode::Name, true),
        [N] | [N, N] => (SortMode::Name, false),
        [A] | [A, A] => (SortMode::Alignment, false),
        [A, R] | [A, A, R] => (SortMode::Alignment, true),
        [X] => (SortMode::NoSort, false),
        [X, R] => (SortMode::NoSort, true),
        [N, A] => (SortMode::NameAlignment, false),
        [N, A, R] => (SortMode::NameAlignment, true),
        [A, N] => (SortMode::AlignmentName, false),
        [A, N, R] => (SortMode::AlignmentName, true),
        [P] => (SortMode::InitPriority, false),
        [P, R] | [R, P] => (SortMode::InitPriority, true),
        _ => return None,
    })
}

fn file_sort(chain: &[SortKw]) -> Option<(SortMode, bool)> {
    use SortKw::{Name as N, NoSort as X, Reverse as R};
    Some(match chain {
        [] => (SortMode::None, false),
        [R] | [N, R] | [R, N] => (SortMode::Name, true),
        [N] => (SortMode::Name, false),
        [X] | [X, R] => (SortMode::NoSort, false),
        _ => return None,
    })
}

fn assign_op(kind: TokenKind) -> Option<AssignOp> {
    let TokenKind::Punct(p) = kind else {
        return None;
    };
    Some(match p {
        Punct::Assign => AssignOp::Assign,
        Punct::AddAssign => AssignOp::Add,
        Punct::SubAssign => AssignOp::Sub,
        Punct::MulAssign => AssignOp::Mul,
        Punct::DivAssign => AssignOp::Div,
        Punct::ShlAssign => AssignOp::Shl,
        Punct::ShrAssign => AssignOp::Shr,
        Punct::AndAssign => AssignOp::And,
        Punct::OrAssign => AssignOp::Or,
        Punct::XorAssign => AssignOp::Xor,
        _ => return None,
    })
}

/// Binary operators with their precedence (higher binds tighter).
fn binary_op(kind: TokenKind) -> Option<(BinaryOp, u8)> {
    let TokenKind::Punct(p) = kind else {
        return None;
    };
    Some(match p {
        Punct::OrOr => (BinaryOp::LogicalOr, 1),
        Punct::AndAnd => (BinaryOp::LogicalAnd, 2),
        Punct::Pipe => (BinaryOp::Or, 3),
        Punct::Caret => (BinaryOp::Xor, 4),
        Punct::Amp => (BinaryOp::And, 5),
        Punct::EqEq => (BinaryOp::Eq, 6),
        Punct::Ne => (BinaryOp::Ne, 6),
        Punct::Lt => (BinaryOp::Lt, 7),
        Punct::Gt => (BinaryOp::Gt, 7),
        Punct::Le => (BinaryOp::Le, 7),
        Punct::Ge => (BinaryOp::Ge, 7),
        Punct::Shl => (BinaryOp::Shl, 8),
        Punct::Shr => (BinaryOp::Shr, 8),
        Punct::Plus => (BinaryOp::Add, 9),
        Punct::Minus => (BinaryOp::Sub, 9),
        Punct::Star => (BinaryOp::Mul, 10),
        Punct::Slash => (BinaryOp::Div, 10),
        Punct::Percent => (BinaryOp::Rem, 10),
        _ => return None,
    })
}

/// `PT_*` names GNU ld accepts in `PHDRS`.
fn phdr_type_value(name: &[u8]) -> Option<u64> {
    Some(match name {
        b"PT_NULL" => 0,
        b"PT_LOAD" => 1,
        b"PT_DYNAMIC" => 2,
        b"PT_INTERP" => 3,
        b"PT_NOTE" => 4,
        b"PT_SHLIB" => 5,
        b"PT_PHDR" => 6,
        b"PT_TLS" => 7,
        b"PT_GNU_EH_FRAME" => 0x6474_e550,
        b"PT_GNU_STACK" => 0x6474_e551,
        b"PT_GNU_RELRO" => 0x6474_e552,
        b"PT_GNU_PROPERTY" => 0x6474_e553,
        _ => return None,
    })
}

/// Errors are boxed inside the parser so the `Result`s held in each
/// recursive frame stay small.
type PResult<T> = Result<T, Box<ScriptError>>;

struct Parser<'r, 's> {
    lex: Lexer<'s>,
    /// Index of the file being read in `files`.
    file: u32,
    files: Vec<PathBuf>,
    reader: &'r mut dyn ScriptReader,
    include_depth: usize,
    /// The most recent integer literal read by the expression parser, for
    /// fill patterns.
    last_int: Option<Token>,
}

impl<'r, 's> Parser<'r, 's> {
    fn new(source: &'s [u8], path: &Path, reader: &'r mut dyn ScriptReader) -> Self {
        Self {
            lex: Lexer::new(Cow::Borrowed(source)),
            file: 0,
            files: vec![path.to_path_buf()],
            reader,
            include_depth: 0,
            last_int: None,
        }
    }

    // ----- token helpers -------------------------------------------------

    fn error_at(&self, offset: usize, message: impl Into<String>) -> Box<ScriptError> {
        let (line, column) = self.lex.line_col(offset);
        let file = usize::try_from(self.file)
            .ok()
            .and_then(|i| self.files.get(i))
            .cloned()
            .unwrap_or_default();
        Box::new(ScriptError::new(
            file,
            line,
            column,
            u64::try_from(offset).unwrap_or(u64::MAX),
            message,
        ))
    }

    fn lex_error(&self, error: LexError) -> Box<ScriptError> {
        self.error_at(error.offset, error.message)
    }

    fn span(&self, token: &Token) -> Span {
        let (line, column) = self.lex.line_col(token.start);
        Span {
            file: self.file,
            line,
            column,
            offset: u32::try_from(token.start).unwrap_or(u32::MAX),
        }
    }

    fn peek(&mut self, mode: Mode) -> PResult<Token> {
        self.lex.peek(mode).map_err(|e| self.lex_error(e))
    }

    fn next(&mut self, mode: Mode) -> PResult<Token> {
        self.lex.next(mode).map_err(|e| self.lex_error(e))
    }

    /// The token after the next one, both read in `mode`.
    fn peek2(&mut self, mode: Mode) -> PResult<Token> {
        let saved = self.lex.save();
        let result = self.next(mode).and_then(|_| self.next(mode));
        self.lex.restore(saved);
        result
    }

    fn text(&self, token: &Token) -> &[u8] {
        self.lex.text(token)
    }

    fn owned_text(&self, token: &Token) -> Vec<u8> {
        self.text(token).to_vec()
    }

    /// The keyword an unquoted name token spells, if any.
    fn kw(&self, token: &Token) -> Option<Kw> {
        if token.kind == TokenKind::Name {
            keyword(self.text(token))
        } else {
            None
        }
    }

    fn describe(&self, token: &Token) -> String {
        match token.kind {
            TokenKind::Eof => "end of file".into(),
            _ => format!(
                "`{}'",
                String::from_utf8_lossy(self.lex.slice(token.start, token.end))
            ),
        }
    }

    fn unexpected(&self, token: &Token, expected: &str) -> Box<ScriptError> {
        self.error_at(
            token.start,
            format!(
                "syntax error: expected {expected}, found {}",
                self.describe(token)
            ),
        )
    }

    fn is_punct(token: &Token, punct: Punct) -> bool {
        token.kind == TokenKind::Punct(punct)
    }

    fn expect_punct(&mut self, mode: Mode, punct: Punct) -> PResult<Token> {
        let token = self.next(mode)?;
        if Self::is_punct(&token, punct) {
            Ok(token)
        } else {
            Err(self.unexpected(&token, &format!("`{}'", punct.text())))
        }
    }

    fn eat_punct(&mut self, mode: Mode, punct: Punct) -> PResult<bool> {
        let token = self.peek(mode)?;
        if Self::is_punct(&token, punct) {
            self.next(mode)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Reads a name or quoted string.
    fn expect_name(&mut self, mode: Mode, what: &str) -> PResult<Vec<u8>> {
        let token = self.next(mode)?;
        match token.kind {
            TokenKind::Name | TokenKind::Quoted => Ok(self.owned_text(&token)),
            _ => Err(self.unexpected(&token, what)),
        }
    }

    /// `( name )` with every token in `mode`.
    fn paren_name(&mut self, mode: Mode, what: &str) -> PResult<Vec<u8>> {
        self.expect_punct(mode, Punct::LParen)?;
        let name = self.expect_name(mode, what)?;
        self.expect_punct(mode, Punct::RParen)?;
        Ok(name)
    }

    fn expect_eof(&mut self, mode: Mode) -> PResult<()> {
        let token = self.next(mode)?;
        if token.kind == TokenKind::Eof {
            Ok(())
        } else {
            Err(self.unexpected(&token, "end of input"))
        }
    }

    /// Parses the contents of an included file with `body`, then returns to
    /// the including file.
    fn include<T>(
        &mut self,
        name_token: &Token,
        body: impl FnOnce(&mut Self) -> PResult<T>,
    ) -> PResult<T> {
        if self.include_depth >= MAX_INCLUDE_DEPTH {
            return Err(self.error_at(name_token.start, "includes nested too deeply"));
        }
        let name = self.owned_text(name_token);
        let from = usize::try_from(self.file)
            .ok()
            .and_then(|i| self.files.get(i))
            .cloned()
            .unwrap_or_default();
        let (path, bytes) = self.reader.read_include(&name, &from).map_err(|error| {
            self.error_at(
                name_token.start,
                format!(
                    "cannot open linker script file {}: {error}",
                    String::from_utf8_lossy(&name)
                ),
            )
        })?;
        let index = u32::try_from(self.files.len())
            .map_err(|_| self.error_at(name_token.start, "too many included files"))?;
        self.files.push(path);
        let saved_lexer = std::mem::replace(&mut self.lex, Lexer::new(Cow::Owned(bytes)));
        let saved_file = std::mem::replace(&mut self.file, index);
        self.include_depth = self.include_depth.saturating_add(1);
        let result = body(self);
        self.include_depth = self.include_depth.saturating_sub(1);
        self.file = saved_file;
        self.lex = saved_lexer;
        result
    }

    // ----- top level -----------------------------------------------------

    /// Top-level commands until end of file.
    fn script_commands(&mut self, out: &mut Vec<Command>) -> PResult<()> {
        loop {
            let token = self.peek(Mode::Script)?;
            let span = self.span(&token);
            let kind = match token.kind {
                TokenKind::Eof => return Ok(()),
                TokenKind::Punct(Punct::Semi) => {
                    self.next(Mode::Script)?;
                    continue;
                }
                TokenKind::Name if self.kw(&token) == Some(Kw::Include) => {
                    self.next(Mode::Script)?;
                    let name = self.next(Mode::Script)?;
                    if !matches!(name.kind, TokenKind::Name | TokenKind::Quoted) {
                        return Err(self.unexpected(&name, "a file name"));
                    }
                    self.include(&name, |p| p.script_commands(out))?;
                    continue;
                }
                TokenKind::Name | TokenKind::Quoted => match self.top_level_command(&token)? {
                    Some(kind) => kind,
                    None => continue,
                },
                _ => return Err(self.unexpected(&token, "a command")),
            };
            out.push(Command { span, kind });
        }
    }

    /// One top-level command starting at `token`. `None` for commands that
    /// produce nothing (obsolete ones, and `INCLUDE`, whose commands are
    /// pushed directly).
    fn top_level_command(&mut self, token: &Token) -> PResult<Option<CommandKind>> {
        const S: Mode = Mode::Script;
        let Some(kw) = self.kw(token) else {
            let assignment = self.assignment_statement(S)?;
            return Ok(Some(CommandKind::Assignment(assignment)));
        };
        let followed_by_paren = Self::is_punct(&self.peek2(S)?, Punct::LParen);
        let kind = match kw {
            Kw::Memory => {
                self.next(S)?;
                self.expect_punct(S, Punct::LBrace)?;
                let mut regions = Vec::new();
                self.memory_regions(&mut regions, false)?;
                self.expect_punct(S, Punct::RBrace)?;
                CommandKind::Memory(regions)
            }
            Kw::Sections | Kw::OverwriteSections => {
                self.next(S)?;
                self.expect_punct(S, Punct::LBrace)?;
                let mut commands = Vec::new();
                self.sections_commands(&mut commands, false)?;
                self.expect_punct(S, Punct::RBrace)?;
                if kw == Kw::Sections {
                    CommandKind::Sections(commands)
                } else {
                    CommandKind::OverwriteSections(commands)
                }
            }
            Kw::Phdrs => {
                self.next(S)?;
                CommandKind::Phdrs(self.phdrs()?)
            }
            Kw::Version => {
                self.next(S)?;
                self.expect_punct(Mode::VersionScript, Punct::LBrace)?;
                let nodes = self.version_nodes()?;
                self.expect_punct(Mode::VersionScript, Punct::RBrace)?;
                CommandKind::Version(nodes)
            }
            Kw::Startup => {
                self.next(S)?;
                CommandKind::Startup(self.paren_name(S, "a file name")?)
            }
            Kw::Hll | Kw::Syslib => {
                self.next(S)?;
                self.expect_punct(S, Punct::LParen)?;
                loop {
                    let t = self.next(S)?;
                    match t.kind {
                        TokenKind::Punct(Punct::RParen) => break,
                        TokenKind::Name | TokenKind::Quoted | TokenKind::Punct(Punct::Comma) => {}
                        _ => return Err(self.unexpected(&t, "a file name or `)'")),
                    }
                }
                return Ok(None);
            }
            Kw::Float | Kw::NoFloat => {
                self.next(S)?;
                return Ok(None);
            }
            Kw::Target => {
                self.next(S)?;
                CommandKind::Target(self.paren_name(S, "a target name")?)
            }
            Kw::SearchDir => {
                self.next(S)?;
                CommandKind::SearchDir(self.paren_name(S, "a directory")?)
            }
            Kw::Output => {
                self.next(S)?;
                CommandKind::Output(self.paren_name(S, "a file name")?)
            }
            Kw::OutputArch => {
                self.next(S)?;
                CommandKind::OutputArch(self.paren_name(S, "an architecture")?)
            }
            Kw::Map => {
                self.next(S)?;
                CommandKind::Map(self.paren_name(S, "a file name")?)
            }
            Kw::LdFeature => {
                self.next(S)?;
                CommandKind::LdFeature(self.paren_name(S, "a feature name")?)
            }
            Kw::Entry => {
                self.next(S)?;
                CommandKind::Entry(self.paren_name(S, "a symbol name")?)
            }
            Kw::OutputFormat => {
                self.next(S)?;
                self.expect_punct(S, Punct::LParen)?;
                let default = self.expect_name(S, "an output format")?;
                let (big, little) = if self.eat_punct(S, Punct::Comma)? {
                    let big = self.expect_name(S, "an output format")?;
                    self.expect_punct(S, Punct::Comma)?;
                    let little = self.expect_name(S, "an output format")?;
                    (Some(big), Some(little))
                } else {
                    (None, None)
                };
                self.expect_punct(S, Punct::RParen)?;
                CommandKind::OutputFormat {
                    default,
                    big,
                    little,
                }
            }
            Kw::ForceCommonAllocation => {
                self.next(S)?;
                CommandKind::ForceCommonAllocation
            }
            Kw::ForceGroupAllocation => {
                self.next(S)?;
                CommandKind::ForceGroupAllocation
            }
            Kw::InhibitCommonAllocation => {
                self.next(S)?;
                CommandKind::InhibitCommonAllocation
            }
            Kw::Input | Kw::Group | Kw::Lib => {
                self.next(S)?;
                self.expect_punct(S, Punct::LParen)?;
                let mut files = Vec::new();
                self.input_list(&mut files, false, 0)?;
                match kw {
                    Kw::Input => CommandKind::Input(files),
                    Kw::Group => CommandKind::Group(files),
                    _ => CommandKind::Lib(files),
                }
            }
            Kw::NoCrossRefs | Kw::NoCrossRefsTo => {
                self.next(S)?;
                self.expect_punct(S, Punct::LParen)?;
                let mut sections = Vec::new();
                loop {
                    let t = self.next(S)?;
                    match t.kind {
                        TokenKind::Punct(Punct::RParen) => break,
                        TokenKind::Punct(Punct::Comma) if !sections.is_empty() => {}
                        TokenKind::Name | TokenKind::Quoted => sections.push(self.owned_text(&t)),
                        _ => return Err(self.unexpected(&t, "a section name or `)'")),
                    }
                }
                if kw == Kw::NoCrossRefs {
                    CommandKind::NoCrossRefs(sections)
                } else {
                    CommandKind::NoCrossRefsTo(sections)
                }
            }
            Kw::Extern => {
                self.next(S)?;
                self.expect_punct(S, Punct::LParen)?;
                let mut symbols = Vec::new();
                loop {
                    let t = self.next(Mode::Expr)?;
                    match t.kind {
                        TokenKind::Punct(Punct::RParen) if !symbols.is_empty() => break,
                        TokenKind::Punct(Punct::Comma) if !symbols.is_empty() => {}
                        TokenKind::Name | TokenKind::Quoted => symbols.push(self.owned_text(&t)),
                        _ => return Err(self.unexpected(&t, "a symbol name")),
                    }
                }
                CommandKind::Extern(symbols)
            }
            Kw::Insert => {
                self.next(S)?;
                let t = self.next(S)?;
                let position = match self.kw(&t) {
                    Some(Kw::After) => InsertPosition::After,
                    Some(Kw::Before) => InsertPosition::Before,
                    _ => return Err(self.unexpected(&t, "`AFTER' or `BEFORE'")),
                };
                let section = self.expect_name(S, "an output section name")?;
                CommandKind::Insert { position, section }
            }
            Kw::RegionAlias => {
                self.next(S)?;
                self.expect_punct(S, Punct::LParen)?;
                let alias = self.expect_name(S, "a region alias")?;
                self.expect_punct(S, Punct::Comma)?;
                let region = self.expect_name(S, "a memory region name")?;
                self.expect_punct(S, Punct::RParen)?;
                CommandKind::RegionAlias { alias, region }
            }
            Kw::Assert if followed_by_paren => {
                self.next(S)?;
                CommandKind::Assert(self.assert_body()?)
            }
            _ => CommandKind::Assignment(self.assignment_statement(S)?),
        };
        Ok(Some(kind))
    }

    // ----- input lists ---------------------------------------------------

    /// The contents of `INPUT(`, `GROUP(` or `AS_NEEDED(`, through the `)`.
    fn input_list(
        &mut self,
        out: &mut Vec<InputFile>,
        as_needed: bool,
        depth: usize,
    ) -> PResult<()> {
        const I: Mode = Mode::InputList;
        if depth > MAX_LIST_DEPTH {
            let t = self.peek(I)?;
            return Err(self.error_at(t.start, "AS_NEEDED nested too deeply"));
        }
        let mut first = true;
        loop {
            let t = self.next(I)?;
            match t.kind {
                TokenKind::Punct(Punct::RParen) => return Ok(()),
                TokenKind::Punct(Punct::Comma) if !first => continue,
                TokenKind::Name
                    if self.kw(&t) == Some(Kw::AsNeeded)
                        && Self::is_punct(&self.peek(I)?, Punct::LParen) =>
                {
                    self.next(I)?;
                    self.input_list(out, true, depth.saturating_add(1))?;
                }
                TokenKind::Name | TokenKind::Quoted => out.push(InputFile {
                    name: InputName::Path(self.owned_text(&t)),
                    as_needed,
                }),
                TokenKind::LibName => out.push(InputFile {
                    name: InputName::Library(self.owned_text(&t)),
                    as_needed,
                }),
                _ => return Err(self.unexpected(&t, "a file name or `)'")),
            }
            first = false;
        }
    }

    // ----- assignments ---------------------------------------------------

    /// An assignment followed by its `;` or `,` separator.
    fn assignment_statement(&mut self, mode: Mode) -> PResult<Assignment> {
        let assignment = self.assignment(mode)?;
        let t = self.next(Mode::Expr)?;
        if Self::is_punct(&t, Punct::Semi) || Self::is_punct(&t, Punct::Comma) {
            Ok(assignment)
        } else {
            Err(self.unexpected(&t, "`;'"))
        }
    }

    /// `name op expr`, `HIDDEN(name = expr)`, `PROVIDE(name = expr)` or
    /// `PROVIDE_HIDDEN(name = expr)`, with names read in `mode`.
    fn assignment(&mut self, mode: Mode) -> PResult<Assignment> {
        let t = self.next(mode)?;
        let wrapper = match self.kw(&t) {
            Some(Kw::Hidden) => Some(AssignKind::Hidden),
            Some(Kw::Provide) => Some(AssignKind::Provide),
            Some(Kw::ProvideHidden) => Some(AssignKind::ProvideHidden),
            _ => None,
        };
        if let Some(kind) = wrapper
            && Self::is_punct(&self.peek(mode)?, Punct::LParen)
        {
            self.next(mode)?;
            let target = self.expect_name(mode, "a symbol name")?;
            self.expect_punct(mode, Punct::Assign)?;
            let expr = self.expr()?;
            self.expect_punct(Mode::Expr, Punct::RParen)?;
            return Ok(Assignment {
                target,
                op: AssignOp::Assign,
                expr,
                kind,
            });
        }
        if !matches!(t.kind, TokenKind::Name | TokenKind::Quoted) {
            return Err(self.unexpected(&t, "a symbol name"));
        }
        let target = self.owned_text(&t);
        let op_token = self.next(mode)?;
        let Some(op) = assign_op(op_token.kind) else {
            return Err(self.unexpected(&op_token, "an assignment operator"));
        };
        let expr = self.expr()?;
        Ok(Assignment {
            target,
            op,
            expr,
            kind: AssignKind::Normal,
        })
    }

    /// `( expr , message )` after `ASSERT`.
    fn assert_body(&mut self) -> PResult<Assert> {
        self.expect_punct(Mode::Expr, Punct::LParen)?;
        let expr = self.expr()?;
        self.expect_punct(Mode::Expr, Punct::Comma)?;
        let message = self.expect_name(Mode::Expr, "a message")?;
        self.expect_punct(Mode::Expr, Punct::RParen)?;
        Ok(Assert { expr, message })
    }

    // ----- MEMORY --------------------------------------------------------

    fn memory_regions(&mut self, out: &mut Vec<MemoryRegion>, included: bool) -> PResult<()> {
        const S: Mode = Mode::Script;
        loop {
            let t = self.peek(S)?;
            match t.kind {
                TokenKind::Eof if included => return Ok(()),
                TokenKind::Punct(Punct::RBrace) if !included => return Ok(()),
                TokenKind::Punct(Punct::Comma) => {
                    self.next(S)?;
                }
                TokenKind::Name if self.kw(&t) == Some(Kw::Include) => {
                    self.next(S)?;
                    let name = self.next(S)?;
                    if !matches!(name.kind, TokenKind::Name | TokenKind::Quoted) {
                        return Err(self.unexpected(&name, "a file name"));
                    }
                    self.include(&name, |p| p.memory_regions(out, true))?;
                }
                TokenKind::Name | TokenKind::Quoted => {
                    let region = self.memory_region()?;
                    out.push(region);
                }
                _ => return Err(self.unexpected(&t, "a memory region")),
            }
        }
    }

    fn memory_region(&mut self) -> PResult<MemoryRegion> {
        const S: Mode = Mode::Script;
        let name_token = self.next(S)?;
        let span = self.span(&name_token);
        let name = self.owned_text(&name_token);
        let mut attributes = MemoryAttributes::default();
        if self.eat_punct(S, Punct::LParen)? {
            let mut invert_next = false;
            loop {
                let t = self.next(S)?;
                match t.kind {
                    TokenKind::Punct(Punct::RParen) => break,
                    TokenKind::Punct(Punct::Bang) => invert_next = true,
                    TokenKind::Name => {
                        let mut invert = invert_next;
                        invert_next = false;
                        for &c in self.lex.text(&t) {
                            let bit = match c.to_ascii_uppercase() {
                                b'!' => {
                                    invert = !invert;
                                    continue;
                                }
                                b'R' => MemoryAttributes::READ_ONLY,
                                b'W' => MemoryAttributes::WRITE,
                                b'X' => MemoryAttributes::EXEC,
                                b'A' => MemoryAttributes::ALLOC,
                                b'I' | b'L' => MemoryAttributes::LOAD,
                                _ => {
                                    return Err(self.error_at(
                                        t.start,
                                        format!("invalid character {} in flags", c as char),
                                    ));
                                }
                            };
                            if invert {
                                attributes.not_flags |= bit;
                            } else {
                                attributes.flags |= bit;
                            }
                        }
                    }
                    _ => return Err(self.unexpected(&t, "memory attributes")),
                }
            }
        }
        self.expect_punct(S, Punct::Colon)?;
        let origin_kw = self.next(S)?;
        if !matches!(self.text(&origin_kw), b"ORIGIN" | b"o" | b"org")
            || origin_kw.kind != TokenKind::Name
        {
            return Err(self.unexpected(&origin_kw, "`ORIGIN'"));
        }
        self.expect_punct(S, Punct::Assign)?;
        let origin = self.expr()?;
        self.eat_punct(Mode::Expr, Punct::Comma)?;
        let length_kw = self.next(S)?;
        if !matches!(self.text(&length_kw), b"LENGTH" | b"l" | b"len")
            || length_kw.kind != TokenKind::Name
        {
            return Err(self.unexpected(&length_kw, "`LENGTH'"));
        }
        self.expect_punct(S, Punct::Assign)?;
        let length = self.expr()?;
        Ok(MemoryRegion {
            span,
            name,
            attributes,
            origin,
            length,
        })
    }

    // ----- PHDRS ---------------------------------------------------------

    fn phdrs(&mut self) -> PResult<Vec<Phdr>> {
        const S: Mode = Mode::Script;
        const E: Mode = Mode::Expr;
        self.expect_punct(S, Punct::LBrace)?;
        let mut out = Vec::new();
        loop {
            let t = self.next(S)?;
            match t.kind {
                TokenKind::Punct(Punct::RBrace) => return Ok(out),
                TokenKind::Name | TokenKind::Quoted => {}
                _ => return Err(self.unexpected(&t, "a program header name or `}'")),
            }
            let span = self.span(&t);
            let name = self.owned_text(&t);
            let type_start = self.peek(E)?.start;
            let mut phdr_type = self.expr()?;
            if let Expr::Symbol(type_name) = &phdr_type {
                match phdr_type_value(type_name) {
                    Some(value) => phdr_type = Expr::Number(value),
                    None => {
                        return Err(self.error_at(
                            type_start,
                            format!(
                                "unknown phdr type `{}' (try integer literal)",
                                String::from_utf8_lossy(type_name)
                            ),
                        ));
                    }
                }
            }
            let mut phdr = Phdr {
                span,
                name,
                phdr_type,
                filehdr: false,
                phdrs: false,
                at: None,
                flags: None,
            };
            loop {
                let q = self.next(E)?;
                if Self::is_punct(&q, Punct::Semi) {
                    break;
                }
                if q.kind != TokenKind::Name {
                    return Err(self.unexpected(&q, "`;'"));
                }
                let word = self.owned_text(&q);
                let value = if Self::is_punct(&self.peek(E)?, Punct::LParen) {
                    self.next(E)?;
                    let value = self.expr()?;
                    self.expect_punct(E, Punct::RParen)?;
                    Some(value)
                } else {
                    None
                };
                match (word.as_slice(), value) {
                    (b"FILEHDR", None) => phdr.filehdr = true,
                    (b"PHDRS", None) => phdr.phdrs = true,
                    (b"AT", Some(value)) => phdr.at = Some(value),
                    (b"FLAGS", Some(value)) => phdr.flags = Some(value),
                    _ => {
                        return Err(self.error_at(
                            q.start,
                            format!("PHDRS syntax error at `{}'", String::from_utf8_lossy(&word)),
                        ));
                    }
                }
            }
            out.push(phdr);
        }
    }

    // ----- SECTIONS ------------------------------------------------------

    fn sections_commands(&mut self, out: &mut Vec<SectionsCommand>, included: bool) -> PResult<()> {
        const S: Mode = Mode::Script;
        loop {
            let t = self.peek(S)?;
            let span = self.span(&t);
            let kind = match t.kind {
                TokenKind::Eof if included => return Ok(()),
                TokenKind::Punct(Punct::RBrace) if !included => return Ok(()),
                TokenKind::Punct(Punct::Semi) => {
                    self.next(S)?;
                    continue;
                }
                TokenKind::Name | TokenKind::Quoted => {
                    let second = self.peek2(S)?;
                    match self.kw(&t) {
                        Some(Kw::Entry) if Self::is_punct(&second, Punct::LParen) => {
                            self.next(S)?;
                            SectionsCommandKind::Entry(self.paren_name(S, "a symbol name")?)
                        }
                        Some(Kw::Assert) if Self::is_punct(&second, Punct::LParen) => {
                            self.next(S)?;
                            SectionsCommandKind::Assert(self.assert_body()?)
                        }
                        Some(Kw::Include) => {
                            self.next(S)?;
                            let name = self.next(S)?;
                            if !matches!(name.kind, TokenKind::Name | TokenKind::Quoted) {
                                return Err(self.unexpected(&name, "a file name"));
                            }
                            self.include(&name, |p| p.sections_commands(out, true))?;
                            continue;
                        }
                        Some(Kw::Overlay) => {
                            self.next(S)?;
                            SectionsCommandKind::Overlay(Box::new(self.overlay()?))
                        }
                        Some(Kw::Group) => {
                            // SVR3 compatibility: `GROUP addr : { sections }`
                            // sets `.` and lists sections.
                            self.next(S)?;
                            let (address, _) = self.section_address_and_type()?;
                            self.expect_punct(Mode::Expr, Punct::Colon)?;
                            self.expect_punct(S, Punct::LBrace)?;
                            if let Some(expr) = address {
                                out.push(SectionsCommand {
                                    span,
                                    kind: SectionsCommandKind::Assignment(Assignment {
                                        target: b".".to_vec(),
                                        op: AssignOp::Assign,
                                        expr,
                                        kind: AssignKind::Normal,
                                    }),
                                });
                            }
                            self.sections_commands(out, false)?;
                            self.expect_punct(S, Punct::RBrace)?;
                            continue;
                        }
                        Some(Kw::Provide | Kw::ProvideHidden | Kw::Hidden)
                            if Self::is_punct(&second, Punct::LParen) =>
                        {
                            SectionsCommandKind::Assignment(self.assignment_statement(S)?)
                        }
                        _ if assign_op(second.kind).is_some() => {
                            SectionsCommandKind::Assignment(self.assignment_statement(S)?)
                        }
                        _ => SectionsCommandKind::OutputSection(Box::new(self.output_section()?)),
                    }
                }
                _ => return Err(self.unexpected(&t, "an output section or assignment")),
            };
            out.push(SectionsCommand { span, kind });
        }
    }

    /// Whether the `(` at the next position opens a section type rather
    /// than a parenthesized address.
    fn paren_opens_type(&mut self) -> PResult<bool> {
        let saved = self.lex.save();
        let result = (|| {
            if !Self::is_punct(&self.next(Mode::Expr)?, Punct::LParen) {
                return Ok(false);
            }
            let t = self.next(Mode::Expr)?;
            Ok(Self::is_punct(&t, Punct::RParen)
                || matches!(
                    self.kw(&t),
                    Some(
                        Kw::NoLoad
                            | Kw::DSect
                            | Kw::Copy
                            | Kw::Info
                            | Kw::Overlay
                            | Kw::ReadOnly
                            | Kw::Type
                    )
                ))
        })();
        self.lex.restore(saved);
        result
    }

    /// The optional address and `(type)` of an output section header, up to
    /// but not including the `:`.
    fn section_address_and_type(&mut self) -> PResult<(Option<Expr>, OutputSectionType)> {
        const E: Mode = Mode::Expr;
        let t = self.peek(E)?;
        let mut address = None;
        if self.kw(&t) == Some(Kw::Bind) && Self::is_punct(&self.peek2(E)?, Punct::LParen) {
            // SVR3 compatibility: `BIND(addr) [BLOCK(align)]`.
            self.next(E)?;
            self.next(E)?;
            address = Some(self.expr()?);
            self.expect_punct(E, Punct::RParen)?;
            let t = self.peek(E)?;
            if self.kw(&t) == Some(Kw::Block) {
                self.next(E)?;
                self.expect_punct(E, Punct::LParen)?;
                self.expr()?;
                self.expect_punct(E, Punct::RParen)?;
            }
        } else if !Self::is_punct(&t, Punct::Colon) && !self.paren_opens_type()? {
            address = Some(self.expr()?);
        }
        let mut section_type = OutputSectionType::Normal;
        if self.paren_opens_type()? {
            self.next(E)?;
            let t = self.next(E)?;
            if !Self::is_punct(&t, Punct::RParen) {
                section_type = match self.kw(&t) {
                    Some(Kw::NoLoad) => OutputSectionType::NoLoad,
                    Some(Kw::DSect) => OutputSectionType::DSect,
                    Some(Kw::Copy) => OutputSectionType::Copy,
                    Some(Kw::Info) => OutputSectionType::Info,
                    Some(Kw::Overlay) => OutputSectionType::Overlay,
                    Some(Kw::Type) => {
                        self.expect_punct(E, Punct::Assign)?;
                        OutputSectionType::Type(self.expr()?)
                    }
                    Some(Kw::ReadOnly) => {
                        if self.eat_punct(E, Punct::LParen)? {
                            let ty = self.next(E)?;
                            if self.kw(&ty) != Some(Kw::Type) {
                                return Err(self.unexpected(&ty, "`TYPE'"));
                            }
                            self.expect_punct(E, Punct::Assign)?;
                            let value = self.expr()?;
                            self.expect_punct(E, Punct::RParen)?;
                            OutputSectionType::ReadOnlyType(value)
                        } else {
                            OutputSectionType::ReadOnly
                        }
                    }
                    _ => return Err(self.unexpected(&t, "a section type")),
                };
                self.expect_punct(E, Punct::RParen)?;
            }
        }
        Ok((address, section_type))
    }

    fn output_section(&mut self) -> PResult<OutputSection> {
        const E: Mode = Mode::Expr;
        let name_token = self.next(Mode::Script)?;
        let name = self.owned_text(&name_token);
        let (address, section_type) = self.section_address_and_type()?;
        self.expect_punct(E, Punct::Colon)?;
        let mut section = OutputSection {
            name,
            address,
            section_type,
            load_address: None,
            align: None,
            align_with_input: false,
            subalign: None,
            constraint: SectionConstraint::None,
            commands: Vec::new(),
            region: None,
            load_region: None,
            phdrs: Vec::new(),
            fill: None,
        };
        loop {
            let t = self.next(E)?;
            if Self::is_punct(&t, Punct::LBrace) {
                break;
            }
            let duplicate = match self.kw(&t) {
                Some(Kw::At) => section.load_address.replace(self.paren_expr()?).is_some(),
                Some(Kw::Align) => section.align.replace(self.paren_expr()?).is_some(),
                Some(Kw::SubAlign) => section.subalign.replace(self.paren_expr()?).is_some(),
                Some(Kw::AlignWithInput) => std::mem::replace(&mut section.align_with_input, true),
                Some(kw @ (Kw::OnlyIfRo | Kw::OnlyIfRw | Kw::Special)) => {
                    let previous = std::mem::replace(
                        &mut section.constraint,
                        match kw {
                            Kw::OnlyIfRo => SectionConstraint::OnlyIfRo,
                            Kw::OnlyIfRw => SectionConstraint::OnlyIfRw,
                            _ => SectionConstraint::Special,
                        },
                    );
                    previous != SectionConstraint::None
                }
                _ => return Err(self.unexpected(&t, "`{'")),
            };
            if duplicate {
                return Err(self.error_at(t.start, "syntax error: duplicate section attribute"));
            }
        }
        self.section_statements(&mut section.commands, false)?;
        self.expect_punct(Mode::Wild, Punct::RBrace)?;
        self.section_trailer(
            &mut section.region,
            &mut section.load_region,
            &mut section.phdrs,
            &mut section.fill,
            true,
        )?;
        Ok(section)
    }

    fn paren_expr(&mut self) -> PResult<Expr> {
        self.expect_punct(Mode::Expr, Punct::LParen)?;
        let expr = self.expr()?;
        self.expect_punct(Mode::Expr, Punct::RParen)?;
        Ok(expr)
    }

    /// `>region AT>lma_region :phdr ... =fill ,` after a closing brace.
    fn section_trailer(
        &mut self,
        region: &mut Option<Vec<u8>>,
        load_region: &mut Option<Vec<u8>>,
        phdrs: &mut Vec<Vec<u8>>,
        fill: &mut Option<Fill>,
        allow_regions: bool,
    ) -> PResult<()> {
        const S: Mode = Mode::Script;
        loop {
            let t = self.peek(S)?;
            match t.kind {
                TokenKind::Punct(Punct::Gt) if allow_regions && region.is_none() => {
                    self.next(S)?;
                    *region = Some(self.expect_name(S, "a memory region name")?);
                }
                TokenKind::Name
                    if allow_regions
                        && load_region.is_none()
                        && self.kw(&t) == Some(Kw::At)
                        && Self::is_punct(&self.peek2(S)?, Punct::Gt) =>
                {
                    self.next(S)?;
                    self.next(S)?;
                    *load_region = Some(self.expect_name(S, "a memory region name")?);
                }
                TokenKind::Punct(Punct::Colon) => {
                    self.next(S)?;
                    phdrs.push(self.expect_name(S, "a program header name")?);
                }
                TokenKind::Punct(Punct::Assign) if fill.is_none() => {
                    self.next(S)?;
                    *fill = Some(self.fill_expr()?);
                }
                TokenKind::Punct(Punct::Comma) => {
                    self.next(S)?;
                    return Ok(());
                }
                _ => return Ok(()),
            }
        }
    }

    /// A fill expression, remembering the digits of a bare hex literal.
    fn fill_expr(&mut self) -> PResult<Fill> {
        self.last_int = None;
        let expr = self.expr()?;
        let hex_digits = match (&expr, self.last_int) {
            (
                Expr::Number(_),
                Some(
                    token @ Token {
                        kind:
                            TokenKind::Int {
                                hex_digits: true, ..
                            },
                        ..
                    },
                ),
            ) => Some(
                self.lex
                    .slice(token.start.saturating_add(2), token.end)
                    .to_vec(),
            ),
            _ => None,
        };
        Ok(Fill { expr, hex_digits })
    }

    fn overlay(&mut self) -> PResult<Overlay> {
        const E: Mode = Mode::Expr;
        const S: Mode = Mode::Script;
        let address = if Self::is_punct(&self.peek(E)?, Punct::Colon) {
            None
        } else {
            Some(self.expr()?)
        };
        self.expect_punct(E, Punct::Colon)?;
        let mut overlay = Overlay {
            address,
            no_cross_refs: false,
            load_address: None,
            subalign: None,
            sections: Vec::new(),
            region: None,
            load_region: None,
            phdrs: Vec::new(),
            fill: None,
        };
        loop {
            let t = self.next(E)?;
            if Self::is_punct(&t, Punct::LBrace) {
                break;
            }
            match self.kw(&t) {
                Some(Kw::NoCrossRefs) => overlay.no_cross_refs = true,
                Some(Kw::At) => overlay.load_address = Some(self.paren_expr()?),
                Some(Kw::SubAlign) => overlay.subalign = Some(self.paren_expr()?),
                _ => return Err(self.unexpected(&t, "`{'")),
            }
        }
        loop {
            let t = self.next(S)?;
            match t.kind {
                TokenKind::Punct(Punct::RBrace) => break,
                TokenKind::Name | TokenKind::Quoted => {}
                _ => return Err(self.unexpected(&t, "an overlay section name or `}'")),
            }
            let mut section = OverlaySection {
                span: self.span(&t),
                name: self.owned_text(&t),
                commands: Vec::new(),
                phdrs: Vec::new(),
                fill: None,
            };
            self.expect_punct(Mode::Wild, Punct::LBrace)?;
            self.section_statements(&mut section.commands, false)?;
            self.expect_punct(Mode::Wild, Punct::RBrace)?;
            self.section_trailer(
                &mut None,
                &mut None,
                &mut section.phdrs,
                &mut section.fill,
                false,
            )?;
            overlay.sections.push(section);
        }
        self.section_trailer(
            &mut overlay.region,
            &mut overlay.load_region,
            &mut overlay.phdrs,
            &mut overlay.fill,
            true,
        )?;
        Ok(overlay)
    }

    // ----- output section statements -------------------------------------

    fn section_statements(
        &mut self,
        out: &mut Vec<OutputSectionCommand>,
        included: bool,
    ) -> PResult<()> {
        const W: Mode = Mode::Wild;
        const E: Mode = Mode::Expr;
        loop {
            let t = self.peek(W)?;
            let span = self.span(&t);
            let kind = match t.kind {
                TokenKind::Eof if included => return Ok(()),
                TokenKind::Punct(Punct::RBrace) if !included => return Ok(()),
                TokenKind::Punct(Punct::Semi) => {
                    self.next(W)?;
                    continue;
                }
                TokenKind::Punct(Punct::LBracket) => {
                    OutputSectionCommandKind::Input(self.input_section_spec(false)?)
                }
                TokenKind::Name | TokenKind::Quoted => {
                    let second = self.peek2(W)?;
                    let paren = Self::is_punct(&second, Punct::LParen);
                    match self.kw(&t) {
                        Some(Kw::Keep) if paren => {
                            self.next(W)?;
                            self.next(W)?;
                            let spec = self.input_section_spec(true)?;
                            self.expect_punct(W, Punct::RParen)?;
                            OutputSectionCommandKind::Input(spec)
                        }
                        Some(Kw::CreateObjectSymbols) => {
                            self.next(W)?;
                            OutputSectionCommandKind::CreateObjectSymbols
                        }
                        Some(Kw::Constructors) => {
                            self.next(W)?;
                            OutputSectionCommandKind::Constructors { sorted: false }
                        }
                        Some(Kw::Sort) if paren && self.sort_constructors_ahead()? => {
                            for _ in 0..4 {
                                self.next(W)?;
                            }
                            OutputSectionCommandKind::Constructors { sorted: true }
                        }
                        Some(kw @ (Kw::Byte | Kw::Short | Kw::Long | Kw::Quad | Kw::SQuad))
                            if paren =>
                        {
                            self.next(W)?;
                            self.next(W)?;
                            let expr = self.expr()?;
                            self.expect_punct(E, Punct::RParen)?;
                            let size = match kw {
                                Kw::Byte => DataSize::Byte,
                                Kw::Short => DataSize::Short,
                                Kw::Long => DataSize::Long,
                                Kw::Quad => DataSize::Quad,
                                _ => DataSize::SQuad,
                            };
                            OutputSectionCommandKind::Data { size, expr }
                        }
                        Some(Kw::Fill) if paren => {
                            self.next(W)?;
                            self.next(W)?;
                            let fill = self.fill_expr()?;
                            self.expect_punct(E, Punct::RParen)?;
                            OutputSectionCommandKind::Fill(fill)
                        }
                        Some(Kw::Asciz) => {
                            self.next(W)?;
                            OutputSectionCommandKind::Asciz(self.expect_name(W, "a string")?)
                        }
                        Some(Kw::LinkerVersion) => {
                            self.next(W)?;
                            OutputSectionCommandKind::LinkerVersion
                        }
                        Some(Kw::Assert) if paren => {
                            self.next(W)?;
                            let assert = self.assert_body()?;
                            let sep = self.next(E)?;
                            if !Self::is_punct(&sep, Punct::Semi)
                                && !Self::is_punct(&sep, Punct::Comma)
                            {
                                return Err(self.unexpected(&sep, "`;'"));
                            }
                            OutputSectionCommandKind::Assert(assert)
                        }
                        Some(Kw::Include) => {
                            self.next(W)?;
                            let name = self.next(W)?;
                            if !matches!(name.kind, TokenKind::Name | TokenKind::Quoted) {
                                return Err(self.unexpected(&name, "a file name"));
                            }
                            self.include(&name, |p| p.section_statements(out, true))?;
                            continue;
                        }
                        Some(Kw::Provide | Kw::ProvideHidden | Kw::Hidden) if paren => {
                            OutputSectionCommandKind::Assignment(self.assignment_statement(W)?)
                        }
                        _ if assign_op(second.kind).is_some() => {
                            OutputSectionCommandKind::Assignment(self.assignment_statement(W)?)
                        }
                        _ => OutputSectionCommandKind::Input(self.input_section_spec(false)?),
                    }
                }
                _ => return Err(self.unexpected(&t, "an input section description or `}'")),
            };
            out.push(OutputSectionCommand { span, kind });
        }
    }

    /// Whether the tokens ahead are `SORT ( CONSTRUCTORS )`.
    fn sort_constructors_ahead(&mut self) -> PResult<bool> {
        let saved = self.lex.save();
        let result = (|| {
            self.next(Mode::Wild)?;
            self.next(Mode::Wild)?;
            let inner = self.next(Mode::Wild)?;
            let close = self.next(Mode::Wild)?;
            Ok(self.kw(&inner) == Some(Kw::Constructors) && Self::is_punct(&close, Punct::RParen))
        })();
        self.lex.restore(saved);
        result
    }

    /// An input section description, without the `KEEP(` wrapper.
    fn input_section_spec(&mut self, keep: bool) -> PResult<InputSectionDescription> {
        const W: Mode = Mode::Wild;
        let mut flags = Vec::new();
        let t = self.peek(W)?;
        if self.kw(&t) == Some(Kw::InputSectionFlags)
            && Self::is_punct(&self.peek2(W)?, Punct::LParen)
        {
            self.next(W)?;
            self.next(W)?;
            loop {
                let name = self.expect_name(W, "a section flag")?;
                let (name, negated) = match name.split_first() {
                    Some((b'!', rest)) => (rest.to_vec(), true),
                    _ => (name, false),
                };
                flags.push(SectionFlag { name, negated });
                if !self.eat_punct(W, Punct::Amp)? {
                    break;
                }
            }
            self.expect_punct(W, Punct::RParen)?;
        }
        if self.eat_punct(W, Punct::LBracket)? {
            let sections = self.section_list(Punct::RBracket)?;
            self.expect_punct(W, Punct::RBracket)?;
            return Ok(InputSectionDescription {
                keep,
                flags,
                file: FileSpec {
                    pattern: Pattern::file(b"*"),
                    exclude: Vec::new(),
                    sort: SortMode::None,
                    reverse: false,
                },
                sections: Some(sections),
            });
        }
        let spec = self.wild_spec(0)?;
        let Some((sort, reverse)) = file_sort(&spec.chain) else {
            return Err(self.error_at(spec.start, "syntax error: invalid sort nesting for files"));
        };
        let file = FileSpec {
            pattern: Pattern::file(&spec.name),
            exclude: spec.exclude,
            sort,
            reverse,
        };
        let sections = if self.eat_punct(W, Punct::LParen)? {
            let list = self.section_list(Punct::RParen)?;
            self.expect_punct(W, Punct::RParen)?;
            Some(list)
        } else if spec.chain.is_empty() && file.exclude.is_empty() {
            None
        } else {
            let t = self.peek(W)?;
            return Err(self.unexpected(&t, "`('"));
        };
        Ok(InputSectionDescription {
            keep,
            flags,
            file,
            sections,
        })
    }

    /// Section patterns up to (not including) `close`.
    fn section_list(&mut self, close: Punct) -> PResult<Vec<SectionSpec>> {
        let mut out = Vec::new();
        loop {
            let t = self.peek(Mode::Wild)?;
            if Self::is_punct(&t, close) {
                return Ok(out);
            }
            let spec = self.wild_spec(0)?;
            let Some((sort, reverse)) = section_sort(&spec.chain) else {
                return Err(self.error_at(spec.start, "syntax error: invalid sort nesting"));
            };
            out.push(SectionSpec {
                pattern: Pattern::section(&spec.name),
                exclude_files: spec.exclude,
                sort,
                reverse,
            });
        }
    }

    /// `[SORT_*(]... [EXCLUDE_FILE(names)] name [)]...`.
    fn wild_spec(&mut self, depth: usize) -> PResult<WildSpec> {
        const W: Mode = Mode::Wild;
        let t = self.peek(W)?;
        let paren = Self::is_punct(&self.peek2(W)?, Punct::LParen);
        let sort = match self.kw(&t) {
            Some(Kw::Sort) => Some(SortKw::Name),
            Some(Kw::SortByAlignment) => Some(SortKw::Alignment),
            Some(Kw::SortByInitPriority) => Some(SortKw::InitPriority),
            Some(Kw::SortNone) => Some(SortKw::NoSort),
            Some(Kw::Reverse) => Some(SortKw::Reverse),
            _ => None,
        };
        if let Some(sort) = sort
            && paren
        {
            if depth >= 3 {
                return Err(self.error_at(t.start, "syntax error: sort keywords nested too deeply"));
            }
            self.next(W)?;
            self.next(W)?;
            let mut inner = self.wild_spec(depth.saturating_add(1))?;
            self.expect_punct(W, Punct::RParen)?;
            inner.chain.insert(0, sort);
            inner.start = t.start;
            return Ok(inner);
        }
        let mut exclude = Vec::new();
        if self.kw(&t) == Some(Kw::ExcludeFile) && paren {
            self.next(W)?;
            self.next(W)?;
            loop {
                let n = self.next(W)?;
                match n.kind {
                    TokenKind::Punct(Punct::RParen) if !exclude.is_empty() => break,
                    TokenKind::Name | TokenKind::Quoted => {
                        exclude.push(Pattern::file(self.text(&n)));
                    }
                    _ => return Err(self.unexpected(&n, "a file name pattern")),
                }
            }
        }
        let name = self.expect_name(W, "a file or section name pattern")?;
        Ok(WildSpec {
            chain: Vec::new(),
            exclude,
            name,
            start: t.start,
        })
    }

    // ----- VERSION ---------------------------------------------------------

    /// Version nodes until `}` or end of file (neither consumed).
    fn version_nodes(&mut self) -> PResult<Vec<VersionNode>> {
        const V: Mode = Mode::VersionScript;
        let mut nodes = Vec::new();
        loop {
            let t = self.peek(V)?;
            let span = self.span(&t);
            let name = match t.kind {
                TokenKind::Eof | TokenKind::Punct(Punct::RBrace) => return Ok(nodes),
                TokenKind::Punct(Punct::LBrace) => None,
                TokenKind::Name => {
                    self.next(V)?;
                    Some(self.owned_text(&t))
                }
                _ => return Err(self.unexpected(&t, "a version node")),
            };
            self.expect_punct(V, Punct::LBrace)?;
            let mut node = VersionNode {
                span,
                name,
                ..VersionNode::default()
            };
            self.version_body(&mut node, None, 0)?;
            self.expect_punct(Mode::VersionNode, Punct::RBrace)?;
            loop {
                let d = self.next(V)?;
                match d.kind {
                    TokenKind::Punct(Punct::Semi) => break,
                    TokenKind::Name if node.name.is_some() => {
                        node.dependencies.push(self.owned_text(&d));
                    }
                    _ => return Err(self.unexpected(&d, "`;'")),
                }
            }
            nodes.push(node);
        }
    }

    /// Patterns of a version node up to (not including) its `}`.
    fn version_body(
        &mut self,
        node: &mut VersionNode,
        language: Option<&[u8]>,
        depth: usize,
    ) -> PResult<()> {
        const N: Mode = Mode::VersionNode;
        let mut local = false;
        loop {
            let t = self.peek(N)?;
            match t.kind {
                TokenKind::Punct(Punct::RBrace) => return Ok(()),
                TokenKind::Punct(Punct::Semi) => {
                    self.next(N)?;
                }
                TokenKind::Name | TokenKind::Quoted => {
                    let word = self.owned_text(&t);
                    let second = self.peek2(N)?;
                    if t.kind == TokenKind::Name
                        && language.is_none()
                        && matches!(word.as_slice(), b"global" | b"local")
                        && Self::is_punct(&second, Punct::Colon)
                    {
                        self.next(N)?;
                        self.next(N)?;
                        local = word == b"local";
                        continue;
                    }
                    if t.kind == TokenKind::Name
                        && word == b"extern"
                        && matches!(second.kind, TokenKind::Name | TokenKind::Quoted)
                    {
                        let saved = self.lex.save();
                        self.next(N)?;
                        let lang_token = self.next(N)?;
                        if Self::is_punct(&self.peek(N)?, Punct::LBrace) {
                            if depth >= MAX_LIST_DEPTH {
                                return Err(
                                    self.error_at(t.start, "extern blocks nested too deeply")
                                );
                            }
                            self.next(N)?;
                            let lang = self.owned_text(&lang_token);
                            // Patterns in the block land in `inner.globals`
                            // (blocks have no labels); move them to the list
                            // the block itself is in.
                            let mut inner = VersionNode::default();
                            self.version_body(&mut inner, Some(&lang), depth.saturating_add(1))?;
                            self.expect_punct(N, Punct::RBrace)?;
                            let target = if local {
                                &mut node.locals
                            } else {
                                &mut node.globals
                            };
                            target.extend(inner.globals);
                            continue;
                        }
                        self.lex.restore(saved);
                    }
                    self.next(N)?;
                    let pattern = VersionPattern {
                        pattern: word,
                        language: language.map(<[u8]>::to_vec),
                        literal: t.kind == TokenKind::Quoted,
                    };
                    if local {
                        node.locals.push(pattern);
                    } else {
                        node.globals.push(pattern);
                    }
                }
                _ => return Err(self.unexpected(&t, "a symbol pattern or `}'")),
            }
        }
    }

    // ----- expressions ---------------------------------------------------

    fn expr(&mut self) -> PResult<Expr> {
        self.conditional(0).map(|(expr, _)| expr)
    }

    fn check_depth(&self, depth: usize) -> PResult<()> {
        if depth > MAX_EXPR_DEPTH {
            let offset = self.lex.save();
            Err(self.error_at(offset, "expression nested too deeply"))
        } else {
            Ok(())
        }
    }

    /// `a ? b : c` (right associative), lowest precedence. Returns the
    /// expression and the depth of its tree.
    fn conditional(&mut self, depth: usize) -> PResult<(Expr, usize)> {
        self.check_depth(depth)?;
        let (cond, cond_depth) = self.binary(1, depth)?;
        if !self.eat_punct(Mode::Expr, Punct::Question)? {
            return Ok((cond, cond_depth));
        }
        let inner = depth.saturating_add(1);
        let (then, then_depth) = self.conditional(inner)?;
        self.expect_punct(Mode::Expr, Punct::Colon)?;
        let (otherwise, else_depth) = self.conditional(inner)?;
        let tree = cond_depth.max(then_depth).max(else_depth).saturating_add(1);
        self.check_depth(tree)?;
        Ok((
            Expr::Conditional(Box::new(cond), Box::new(then), Box::new(otherwise)),
            tree,
        ))
    }

    /// Precedence climbing over the binary operators.
    fn binary(&mut self, min_prec: u8, depth: usize) -> PResult<(Expr, usize)> {
        self.check_depth(depth)?;
        let (mut lhs, mut tree) = self.unary(depth)?;
        loop {
            let t = self.peek(Mode::Expr)?;
            let Some((op, prec)) = binary_op(t.kind) else {
                return Ok((lhs, tree));
            };
            if prec < min_prec {
                return Ok((lhs, tree));
            }
            self.next(Mode::Expr)?;
            let (rhs, rhs_tree) = self.binary(prec.saturating_add(1), depth.saturating_add(1))?;
            tree = tree.max(rhs_tree).saturating_add(1);
            self.check_depth(tree)?;
            lhs = Expr::Binary(op, Box::new(lhs), Box::new(rhs));
        }
    }

    fn unary(&mut self, depth: usize) -> PResult<(Expr, usize)> {
        self.check_depth(depth)?;
        let t = self.peek(Mode::Expr)?;
        let op = match t.kind {
            TokenKind::Punct(Punct::Minus) => Some(UnaryOp::Neg),
            TokenKind::Punct(Punct::Bang) => Some(UnaryOp::Not),
            TokenKind::Punct(Punct::Tilde) => Some(UnaryOp::BitNot),
            TokenKind::Punct(Punct::Plus) => None,
            _ => return self.primary(depth),
        };
        self.next(Mode::Expr)?;
        let (operand, tree) = self.unary(depth.saturating_add(1))?;
        Ok(match op {
            Some(op) => (Expr::Unary(op, Box::new(operand)), tree.saturating_add(1)),
            None => (operand, tree),
        })
    }

    fn primary(&mut self, depth: usize) -> PResult<(Expr, usize)> {
        const E: Mode = Mode::Expr;
        let t = self.next(E)?;
        let inner = depth.saturating_add(1);
        match t.kind {
            TokenKind::Int { value, .. } => {
                self.last_int = Some(t);
                return Ok((Expr::Number(value), 1));
            }
            TokenKind::Punct(Punct::LParen) => {
                let result = self.conditional(inner)?;
                self.expect_punct(E, Punct::RParen)?;
                return Ok(result);
            }
            TokenKind::Quoted => {
                let name = self.owned_text(&t);
                return Ok((
                    if name == b"." {
                        Expr::Dot
                    } else {
                        Expr::Symbol(name)
                    },
                    1,
                ));
            }
            TokenKind::Name => {}
            _ => return Err(self.unexpected(&t, "an expression")),
        }
        if self.text(&t) == b"." {
            return Ok((Expr::Dot, 1));
        }
        let kw = self.kw(&t);
        if kw == Some(Kw::SizeOfHeaders) {
            return Ok((Expr::SizeOfHeaders, 1));
        }
        match kw {
            Some(kw) if Self::is_punct(&self.peek(E)?, Punct::LParen) => {
                self.builtin_call(kw, &t, inner)
            }
            _ => Ok((Expr::Symbol(self.owned_text(&t)), 1)),
        }
    }

    /// A built-in function call such as `ALIGN(...)`, with the keyword
    /// already read. Kept out of [`Parser::primary`] so the frames on the
    /// recursion path of parenthesized expressions stay small.
    fn builtin_call(&mut self, kw: Kw, t: &Token, inner: usize) -> PResult<(Expr, usize)> {
        const E: Mode = Mode::Expr;
        const S: Mode = Mode::Script;
        let kw = Some(kw);
        let boxed = |(expr, tree): (Expr, usize)| (Box::new(expr), tree);
        let (expr, tree) = match kw {
            Some(Kw::Defined) => (Expr::Defined(self.paren_name(E, "a symbol name")?), 1),
            Some(Kw::Constant) => (Expr::Constant(self.paren_name(E, "a constant name")?), 1),
            Some(Kw::AlignOf) => (Expr::AlignOf(self.paren_name(S, "a section name")?), 1),
            Some(Kw::SizeOf) => (Expr::SizeOf(self.paren_name(S, "a section name")?), 1),
            Some(Kw::Addr) => (Expr::Addr(self.paren_name(S, "a section name")?), 1),
            Some(Kw::LoadAddr) => (Expr::LoadAddr(self.paren_name(S, "a section name")?), 1),
            Some(Kw::Origin) => (Expr::Origin(self.paren_name(S, "a region name")?), 1),
            Some(Kw::Length) => (Expr::Length(self.paren_name(S, "a region name")?), 1),
            Some(Kw::SegmentStart) => {
                self.expect_punct(S, Punct::LParen)?;
                let name = self.expect_name(S, "a segment name")?;
                self.expect_punct(E, Punct::Comma)?;
                let (value, tree) = boxed(self.conditional(inner)?);
                self.expect_punct(E, Punct::RParen)?;
                (Expr::SegmentStart(name, value), tree)
            }
            Some(Kw::Assert) => {
                self.expect_punct(E, Punct::LParen)?;
                let (value, tree) = boxed(self.conditional(inner)?);
                self.expect_punct(E, Punct::Comma)?;
                let message = self.expect_name(E, "a message")?;
                self.expect_punct(E, Punct::RParen)?;
                (Expr::Assert(value, message), tree)
            }
            Some(
                kw @ (Kw::Absolute
                | Kw::Log2Ceil
                | Kw::DataSegmentEnd
                | Kw::Block
                | Kw::Next
                | Kw::Align
                | Kw::DataSegmentAlign
                | Kw::DataSegmentRelroEnd
                | Kw::Max
                | Kw::Min),
            ) => {
                self.expect_punct(E, Punct::LParen)?;
                let (a, a_tree) = boxed(self.conditional(inner)?);
                let two = matches!(
                    kw,
                    Kw::DataSegmentAlign | Kw::DataSegmentRelroEnd | Kw::Max | Kw::Min
                );
                let second =
                    if two || (kw == Kw::Align && Self::is_punct(&self.peek(E)?, Punct::Comma)) {
                        self.expect_punct(E, Punct::Comma)?;
                        Some(boxed(self.conditional(inner)?))
                    } else {
                        None
                    };
                self.expect_punct(E, Punct::RParen)?;
                let tree = a_tree.max(second.as_ref().map_or(0, |(_, t)| *t));
                let expr = match (kw, second) {
                    (Kw::Absolute, _) => Expr::Absolute(a),
                    (Kw::Log2Ceil, _) => Expr::Log2Ceil(a),
                    (Kw::DataSegmentEnd, _) => Expr::DataSegmentEnd(a),
                    (Kw::Block, _) => Expr::Block(a),
                    (Kw::Next, _) => Expr::Next(a),
                    (Kw::Align, None) => Expr::Align(a),
                    (Kw::Align, Some((b, _))) => Expr::AlignExpr(a, b),
                    (Kw::DataSegmentAlign, Some((b, _))) => Expr::DataSegmentAlign(a, b),
                    (Kw::DataSegmentRelroEnd, Some((b, _))) => Expr::DataSegmentRelroEnd(a, b),
                    (Kw::Max, Some((b, _))) => Expr::Max(a, b),
                    (_, Some((b, _))) => Expr::Min(a, b),
                    (_, None) => Expr::Absolute(a),
                };
                (expr, tree)
            }
            _ => return Ok((Expr::Symbol(self.owned_text(t)), 1)),
        };
        let tree = tree.saturating_add(1);
        self.check_depth(tree)?;
        Ok((expr, tree))
    }
}
