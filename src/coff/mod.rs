//! PE/COFF backend (Windows).
//!
//! Roadmap M7. Scope is in `docs/formats.md`: COFF objects, import
//! libraries (short and long), `.def` files, EXE and DLL output, base
//! relocations, SEH, MinGW auto-import, and resources.
//!
//! Implemented so far:
//!
//! - [`read`]: zero-copy readers for COFF objects (including `/bigobj`),
//!   `.drectve` directives, short and long import libraries, PE images
//!   (exports of DLLs linked directly) and module-definition files
//!   (workstream W14).
//!
//! Not started: resolution, layout and PE writing.

pub mod read;
