//! Linker-script-driven layout (roadmap M3).
//!
//! **Workstream W19.** This module runs layout from a linker script, the way
//! GNU ld does, while the rest of the ELF pipeline stays the same:
//!
//! 1. [`prepare`] ([`load`]) reads `-T` scripts, `--default-script`,
//!    implicit scripts named as inputs and `-b binary` inputs before the
//!    inputs are collected, applies script commands that are options
//!    (`ENTRY`, `OUTPUT_FORMAT`, `SEARCH_DIR`, ...), and builds the
//!    [`LayoutScript`] ([`plan`]). When `INSERT`, implicit scripts or layout
//!    options (`-Ttext`, `-N`, ...) need the default layout underneath, it is
//!    the built-in script of [`defaults`].
//! 2. [`place`] ([`matching`]) assigns input sections to output section
//!    statements and places orphans, instead of [`crate::elf::rules`].
//! 3. [`crate::elf::defined::register`] resolves the symbols scripts assign
//!    and read.
//! 4. [`layout`] ([`engine`]) assigns addresses and load addresses by
//!    walking the statements, repeating until stable, then builds program
//!    headers and file offsets ([`segments`]).
//!
//! Without scripts and layout options, the default rules and layout of
//! [`crate::elf::rules`] and [`crate::elf::layout`] are used unchanged.

#![deny(clippy::arithmetic_side_effects)]

pub mod defaults;
pub mod engine;
pub mod load;
pub mod matching;
pub mod plan;
pub mod segments;

pub use engine::{ScriptSymbol, layout};
pub use load::{Prepared, prepare};
pub use matching::{ResolvedSymbols, ScriptPlacement, SymbolDef, place};
pub use plan::LayoutScript;
