//! GNU linker script lexer, parser and evaluator.
//!
//! **Workstream W3.** Responsibilities:
//!
//! - Lex and parse the full language: `SECTIONS`, `MEMORY`, `PHDRS`, `ENTRY`,
//!   `INPUT`/`GROUP`/`AS_NEEDED`, `PROVIDE`/`PROVIDE_HIDDEN`, `ASSERT`,
//!   `INCLUDE`, `INSERT`, `/DISCARD/`, `KEEP`, `SORT_*`, `EXCLUDE_FILE`,
//!   `AT`/`AT>`, `FILL`, data commands, and `OUTPUT_FORMAT`/`OUTPUT_ARCH`.
//! - Evaluate expressions, including the location counter `.`, and the
//!   built-ins (`ALIGN`, `ADDR`, `LOADADDR`, `SIZEOF`, `DEFINED`, `ORIGIN`,
//!   `LENGTH`, `MAX`, `MIN`, `SEGMENT_START`, …).
//! - Match input section patterns (wildcards, archive:member syntax) against
//!   input sections.
//!
//! Scripts also appear as *inputs* (glibc's `libc.so` is a `GROUP(...)`
//! script), so parsing must work before any layout exists. Script errors are
//! diagnostics with a file and line, never panics. See `ROADMAP.md` M3.
