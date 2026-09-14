//! ELF backend: parsing, resolution rules, layout and relocations.
//!
//! **Workstreams W7 (reading) and W8 (layout and writing).** Responsibilities:
//!
//! - Zero-copy parsing of relocatable objects and shared objects, monomorphized
//!   over ELF class and endianness so hot loops never branch on format.
//! - ELF symbol precedence: strong, weak, common, lazy and shared definitions;
//!   COMDAT group deduplication; visibility.
//! - Synthetic sections: GOT, PLT, `.dynamic`, `.dynsym`/`.dynstr`, hash
//!   tables, `.rela.dyn`/`.relr.dyn`, version sections, `.eh_frame_hdr`,
//!   notes.
//! - Layout: output section assignment matching GNU ld's default script,
//!   segments, and linker-defined symbols.
//! - Per-architecture relocation application, relaxation and thunks, under
//!   `arch/`, starting with x86-64.
//!
//! See `ROADMAP.md` M1 and M2, and `docs/formats.md`.
