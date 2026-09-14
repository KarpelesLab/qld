//! Output file writing and post-write steps.
//!
//! **Workstream W5.** Responsibilities:
//!
//! - Create the output: unlink any existing file, set the length, and map it
//!   writable, with an in-memory fallback for unmappable destinations and for
//!   library callers who want bytes back.
//! - Hand out disjoint `&mut [u8]` chunks so sections can be copied and
//!   relocated in parallel.
//! - Post-write steps: build-id (hash blocks in parallel, then combine),
//!   Mach-O code signature, fat header.
//! - Set the executable bit, and replace the old file atomically.
//!
//! This module and [`crate::input`] are the only places allowed to use
//! `unsafe`, for mapping. Every block needs a `SAFETY` comment.
//! See `docs/architecture.md` ("Output writing").
//!
//! # Overview
//!
//! | Item | Role |
//! | --- | --- |
//! | [`OutputFile`] | Creates, maps, fills and commits the output; its docs cover how an old file is replaced |
//! | [`split_chunks`], [`write_chunks`] | Disjoint `&mut [u8]` per layout chunk, for rayon |
//! | [`build_id`] | Parallel tree-hashed build-ids: `fast`, `md5`, `sha1`, `uuid`, hex |
//! | [`hash`] | In-crate MD5, SHA-1 and xxHash64 |
//! | [`WriteStats`] | Bytes written and time per [`WritePhase`] |
//!
//! Mapping lives in a private submodule that is the only code here allowed to
//! use `unsafe`.
//!
//! Not implemented yet: the Mach-O code signature and the fat header
//! (roadmap M8).

pub mod build_id;
mod chunks;
mod file;
pub mod hash;
mod mmap;
mod random;
mod stats;

pub use chunks::{ChunkRange, LayoutError, split_chunks, validate_layout, write_chunks};
pub use file::{FileMode, Finished, OutputFile, OutputOptions, ReplaceStrategy};
pub use stats::{Backing, WritePhase, WriteStats};
