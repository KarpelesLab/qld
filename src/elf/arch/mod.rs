//! Per-architecture relocation handling.
//!
//! Each architecture module classifies relocation types, rewrites relaxable
//! instruction sequences and writes linker-generated stubs. Only x86-64 is
//! implemented (roadmap M1); the others arrive with M4.

pub mod x86_64;
