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

pub mod build_id;
mod chunks;
pub mod hash;
mod random;

pub use chunks::{ChunkRange, LayoutError, split_chunks, validate_layout, write_chunks};
