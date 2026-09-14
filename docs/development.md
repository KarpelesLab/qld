# Development guide

## Toolchain

- **MSRV: Rust 1.89**, **edition 2024**. Every crate sets
  `rust-version = "1.89"` and `edition = "2024"` through workspace inheritance.
- Do not use language or standard library features stabilized after 1.89.
  Check with `cargo +1.89 check --workspace --all-targets`.
- Edition 2024 features that 1.89 supports and that we use freely: let chains
  (`if let Some(x) = a && cond`), `unsafe extern` blocks, and the new RPIT
  lifetime capture rules.
- Dependencies must also build on 1.89. CI checks this with a
  `Cargo.lock` resolved for MSRV (`resolver = "3"`, MSRV-aware resolution).

## Common commands

```sh
cargo build --release                         # release build of qld
cargo test --workspace                        # unit + integration tests
cargo +1.89 check --workspace --all-targets   # MSRV check
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
cargo fuzz run elf_object                     # fuzz a parser (nightly)
```

## Dependency policy

- **Pure Rust only.** No `build.rs` that compiles C, and no `-sys` crates.
  The `qld-plugin` crate's `dlopen` of compiler-provided LTO plugins is the
  one exception, and it is behind the `plugin` feature.
- Keep the dependency set small. Every new dependency needs a reason in the PR.
  Expected core dependencies:
  - `rayon` (parallelism)
  - `memmap2` (file mapping)
  - `hashbrown` + `foldhash` (hash tables)
  - `rustix` (pure-Rust syscalls: `fallocate`, `ftruncate`, file cloning)
  - `flate2` with the `miniz_oxide` backend, and a pure-Rust zstd (`ruzstd`), for compressed debug sections
  - `libloading` (plugin feature only)
- **Object parsing is written in-house**, not taken from the `object` crate. The
  hot paths need zero-copy, monomorphized, parallel-friendly access that fits
  qld's ID-based data model. `object`, `gimli` and similar crates may still be
  used as **dev-dependencies**, as independent oracles for checking our
  output in tests.
- Licenses must be compatible with MIT: MIT, Apache-2.0, BSD, Zlib, Unicode.
  This is enforced by `cargo deny`.

## `unsafe` policy

- `#![forbid(unsafe_code)]` in every crate except `qld-core` (mmap, output
  writer), `qld-plugin` (FFI), and the format crates' typed views, if
  benchmarks show the safe alternative costs too much.
- Every `unsafe` block has a `// SAFETY:` comment stating the invariant.
- Typed views over input bytes use `from_le_bytes`/`from_be_bytes` on slices,
  or `#[repr(C)]` structs with alignment-1 integer wrappers. Never cast a
  pointer to a possibly misaligned location.
- Mapped input files are treated as immutable. The risk that another process
  modifies them during a link is accepted, as every mmap-based linker accepts
  it, and documented.

## Coding rules

- **Input is untrusted.** Parsing code never panics on malformed input: no
  `unwrap`, no unchecked indexing, no arithmetic that can overflow on offsets
  or sizes. Use checked arithmetic and return `Error::Malformed { file, offset, what }`.
  The fuzz targets enforce this.
- **Determinism.** No `HashMap` iteration order in outputs, no
  dependence on the order parallel tasks finish, and no timestamps unless an
  option asks for them. If you collect results in parallel, sort them by input
  position before using them.
- **IDs over references** for cross-entity links (see
  [architecture.md](architecture.md#design-principles)).
- **Hot-path allocation.** No per-symbol or per-relocation heap allocation in
  parallel stages. Preallocate from counts gathered in an earlier pass.
- **Byte strings.** Symbol and section names are `&[u8]`. Convert them to
  strings only for display (`String::from_utf8_lossy` or a demangler).
- **Documentation.** Every `pub` item in the `qld` crate has rustdoc. Inner
  crates document module-level design. A change to the pipeline updates
  `docs/architecture.md` in the same PR.

## Commit and PR conventions

- Keep commits focused, with a message in the imperative mood (`elf: add GOTPCRELX relaxation`),
  prefixed with the crate or area.
- Tests come with the change: a fixture for any new behavior, and a regression
  fixture for any bug fix.
- A PR that intentionally diverges from GNU ld behavior updates the list of
  differences in [compatibility.md](compatibility.md).
- A PR that ticks off a roadmap item updates [ROADMAP.md](../ROADMAP.md).
