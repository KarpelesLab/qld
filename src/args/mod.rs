//! Command-line option model and the argv front ends that fill it in.
//!
//! **Workstream W1** owns this module.
//!
//! [`LinkOptions`] is plain data: it performs no I/O, resolves no paths and
//! opens no files. Library users can build one directly instead of going
//! through argv. Path resolution happens later, in the driver.
//!
//! - [`options`]: the option model.
//! - [`parse`]: flavor selection and the GNU parser.
//! - [`darwin`]: the Apple ld64 option table and parser.
//! - [`table`]: every recognized option and `-z` keyword, with its status.
//! - [`response`]: `@file` expansion, through a [`FileReader`] so tests stay
//!   hermetic.
//! - [`emulation`]: `-m` names and the targets they select.
//!
//! Parsing rules, flavors and the option table policy are specified in
//! `docs/compatibility.md`.

pub mod darwin;
pub mod emulation;
pub mod options;
pub mod parse;
pub mod response;
pub mod table;

pub use darwin::DarwinArgs;
pub use options::{
    BuildId, ColorChoice, DiscardMode, DynamicFlags, ExecStack, Flavor, HashStyle, InputAttrs,
    InputFormat, InputKind, InputSpec, LinkOptions, MagicMode, OutputCompleteHook, OutputKind,
    PeArgs, ReportLevel, SeparateCode, StripMode, SymbolicMode, UnresolvedSymbols, X86Features,
};
pub use parse::{
    ParseOutcome, parse_darwin, parse_darwin_with, parse_gnu, parse_gnu_with, select_flavor, usage,
};
pub use response::{FileReader, FsReader, NoFiles, Quoting};
