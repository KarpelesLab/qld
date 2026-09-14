//! GNU linker script lexer, parser and evaluator.
//!
//! **Workstream W3.** This module reads the full GNU ld script language:
//!
//! - Top-level commands: `ENTRY`, `INPUT`/`GROUP`/`LIB` with `AS_NEEDED`,
//!   `OUTPUT`, `SEARCH_DIR`, `STARTUP`, `OUTPUT_FORMAT`, `OUTPUT_ARCH`,
//!   `TARGET`, `INCLUDE`, `ASSERT`, `EXTERN`, `FORCE_COMMON_ALLOCATION`,
//!   `INHIBIT_COMMON_ALLOCATION`, `NOCROSSREFS`, `NOCROSSREFS_TO`,
//!   `REGION_ALIAS`, `INSERT AFTER/BEFORE`, `LD_FEATURE`, `MAP`, and symbol
//!   assignments with `PROVIDE`, `PROVIDE_HIDDEN` and `HIDDEN`.
//! - `SECTIONS` (and lld's `OVERWRITE_SECTIONS`): output section descriptions
//!   with address, type, `AT`, `ALIGN`, `SUBALIGN`, constraints, regions,
//!   program headers and fill; `OVERLAY`; input section descriptions with
//!   `KEEP`, `SORT_*`, `REVERSE`, `EXCLUDE_FILE`, `INPUT_SECTION_FLAGS` and
//!   `archive:member` patterns; data commands, `FILL`, `ASCIZ`,
//!   `LINKER_VERSION`, `CREATE_OBJECT_SYMBOLS` and `CONSTRUCTORS`.
//! - `MEMORY`, `PHDRS` and `VERSION`, plus standalone version scripts and
//!   `--defsym` expressions.
//!
//! Parsing produces a [`Script`] ([`parse_script`]). Expressions are
//! evaluated with [`eval`] against an [`EvalContext`] that layout implements,
//! and input section descriptions are matched against input sections with
//! [`InputSectionDescription::matches`].
//!
//! Scripts also appear as *inputs* (glibc's `libc.so` is a `GROUP(...)`
//! script), so nothing here depends on layout. Errors carry a file, line and
//! column, and malformed scripts never cause a panic. See `ROADMAP.md` M3.
//!
//! MRI scripts (`-c`) are not supported.

#![deny(clippy::arithmetic_side_effects)]

mod ast;
mod error;
mod lexer;
mod parser;
mod pattern;

pub use ast::{
    Assert, AssignKind, AssignOp, Assignment, BinaryOp, Command, CommandKind, DataSize, Expr,
    FileSpec, Fill, InputFile, InputName, InputSectionDescription, InsertPosition,
    MemoryAttributes, MemoryRegion, OutputSection, OutputSectionCommand, OutputSectionCommandKind,
    OutputSectionType, Overlay, OverlaySection, Phdr, Script, SectionConstraint, SectionFlag,
    SectionSpec, SectionsCommand, SectionsCommandKind, SortMode, Span, UnaryOp, VersionNode,
    VersionPattern,
};
pub use error::{EvalError, ScriptError};
pub use parser::{
    FsReader, NoIncludes, ScriptReader, parse_defsym, parse_expression, parse_script,
    parse_version_script,
};
pub use pattern::{Pattern, file_matches, init_priority};
