//! Symbol name demangling for diagnostics.
//!
//! **Workstream W13.** Itanium C++ ABI names (`_ZN3foo3barEv`), Rust legacy
//! (`_ZN...17h<hash>E`) and Rust v0 (`_R...`) mangling, rendered the way
//! `c++filt` and `rustfilt` show them. Used only when printing diagnostics,
//! map files and `--print-*` output; never on hot paths. Malformed names are
//! returned unchanged, never a panic.
