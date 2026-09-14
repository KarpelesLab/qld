//! Instruction-level helpers shared across formats.
//!
//! Relocation *types* belong to a format, but the instruction encoding behind
//! them does not: an AArch64 range-extension thunk is the same code whether it
//! is emitted into an ELF or a Mach-O output. Branch ranges, thunk encodings,
//! ADRP/ADD immediate packing and similar helpers live here.
//!
//! Anything that names a relocation constant belongs in a backend instead.
