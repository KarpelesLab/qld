//! LTO through the GNU linker plugin API.
//!
//! Roadmap M6; not started. This is the only part of qld that uses FFI: it
//! `dlopen`s the plugin the compiler toolchain supplies (`liblto_plugin.so`
//! for GCC, `LLVMgold.so` for LLVM) and implements the linker half of
//! `plugin-api.h`. Building qld still requires no C toolchain.
//!
//! Gated by the `plugin` cargo feature, on by default for the binary.
//! See `docs/optimizations.md` ("LTO").
