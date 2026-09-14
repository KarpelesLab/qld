//! Shared helpers for qld's integration tests (workstream W9).
//!
//! Each `tests/*.rs` crate that needs them declares `mod common;`. Not every
//! crate uses every helper, hence the `dead_code` allowance.
//!
//! - [`tools`]: finding compilers, `readelf` and reference linkers
//! - [`process`]: running commands with timeouts, word splitting, parallelism
//! - [`toml`]: the `test.toml` subset parser
//! - [`fixture`]: the fixture format and compile/link/run/check steps
//! - [`readelf`]: `readelf -W` parsing and normalized ELF properties
//! - [`determinism`]: relinking with different thread counts
//! - [`textdiff`]: readable diffs for failure messages

#![allow(dead_code)]

pub mod determinism;
pub mod fixture;
pub mod process;
pub mod readelf;
pub mod textdiff;
pub mod toml;
pub mod tools;
