# Rust compiler bootstrap

- **Version:** rustc 1.98.1 source tarball
  (`https://static.rust-lang.org/dist/rustc-1.98.1-src.tar.xz`); stage 0 is
  the downloaded 1.97.1 beta toolchain
- **Script:** `tests/projects/rust.sh QLD SCRATCH [test paths...]`
  (`RUST_CLEAN=1` rebuilds stage 1 from scratch)
- **LLVM:** the system LLVM 22.1.8 (`llvm-config`, `link-shared = true`),
  to avoid building LLVM a second time

`bootstrap.toml`:

```toml
change-id = "ignore"
[build]
jobs = 64
extended = false
docs = false
[rust]
lld = false
bootstrap-override-lld = false
debuginfo-level = 0
channel = "stable"
[llvm]
link-shared = true
[target.x86_64-unknown-linux-gnu]
llvm-config = "/usr/lib/llvm/22/bin/llvm-config"
cc = "clang"
cxx = "clang++"
linker = "$SCRATCH/bin/qclang"   # clang --ld-path=<qld wrapper>
default-linker-linux-override = "off"
```

`qclang` passes `--ld-path`, which wins over the `-fuse-ld=lld` rustc adds
for its self-contained linker, so both the stage 0 compiler (build scripts,
stage 1 compiler and std) and the stage 1 compiler (UI tests) link with qld.

## Commands and result

| Step | Result | Time |
| --- | --- | --- |
| `python3 x.py build --stage 1` | builds | 107 s |
| `python3 x.py test --stage 1 --no-fail-fast tests/ui library/std` | pass | 88 s |
| `tests/ui` (compiletest, 21552 tests) | 21288 passed, 0 failed, 264 ignored | 46 s |
| `library/std` unit, integration and doc-less tests | all pass (563 + 337 + 1405 + smaller suites) | |

About 7100 links went through qld during the build and tests. The
`linked-by-qld` check lists 17 ELF files without `Linker: qld`, none of which
qld was asked to link: the LLVM tools bootstrap copies from the system LLVM
into the sysroot (`llvm-ar`, `llc`, `opt`, …), `rust-objcopy` (copied from
stage 0), and two `flag_check` probes the `cc` crate links with the compiler
default.

## Dynamic symbol tables against GNU ld

With `QLD_LINK_COMPARE`, every output over 1 MiB (91, including
`librustc_driver.so`, `libstd.so`, `rustdoc`, proc-macro crates and UI test
binaries) was linked again with GNU ld; all identical after the fix below.

## Bugs found

1. **`rust_metadata_*` symbols became `SHN_ABS` in `.dynsym`** (fixed,
   commit `elf: dynamic symbols in non-allocated sections keep their
   section`). Every Rust dylib exports `rust_metadata_<crate>_<hash>`
   defined in the non-allocated `.rustc` section; qld derived the section
   index from the address, found none, and wrote `ABS`. GNU ld keeps the
   `.rustc` section index. Test:
   `dynamic_symbols_in_non_allocated_sections_keep_their_section`.

`librustc_driver.so` links in 0.38 s with qld and 3.03 s with GNU ld
(143914408 and 141226872 bytes), measured during the build.
