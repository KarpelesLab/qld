//! Hints for undefined and duplicate symbols ("intelligent library symbol
//! matching").
//!
//! **Workstream W13.** Given an undefined symbol and the link's search paths,
//! suggests the library that defines it (`did you forget -lm?`), explains
//! version mismatches (`memcpy@GLIBC_2.14` versus the versions available), and
//! proposes near-miss names (C/C++ linkage mismatches, leading underscores,
//! namespace or qualifier differences). The index of search-path libraries is
//! built lazily, only after a link has already failed. See
//! `docs/optimizations.md` ("Intelligent library symbol matching").
