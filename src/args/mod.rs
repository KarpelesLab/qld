//! Command-line option model and the argv front ends that fill it in.
//!
//! [`LinkOptions`] is plain data: it performs no I/O, resolves no paths and
//! opens no files. Library users can build one directly instead of going
//! through argv. Path resolution happens later, in the driver.
//!
//! Parsing rules, flavors and the option table policy are specified in
//! `docs/compatibility.md`.

pub mod options;
pub mod parse;

pub use options::{
    BuildId, Flavor, HashStyle, InputAttrs, InputKind, InputSpec, LinkOptions, OutputKind,
    StripMode,
};
pub use parse::{ParseOutcome, parse_gnu, usage};
