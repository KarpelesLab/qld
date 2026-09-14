# Workstreams

qld is one crate, but the work splits into areas that barely touch each other.
Each workstream owns a directory under `src/`, so several agents (or people)
can work at the same time without colliding.

## Ground rules

1. **Stay inside your directory.** A workstream edits only the files it owns,
   plus its own tests and fixtures. Adding a file means adding a `mod` line to
   your own `mod.rs`, which you own.
2. **Shared files are frozen.** `Cargo.toml`, `src/lib.rs`, `src/error.rs`,
   `src/diag.rs`, `src/ids.rs`, `src/target.rs` and `src/main.rs` are edited by
   the integrator only. If your workstream needs a change there — a new
   dependency, a new `Error` variant, a new module — say so in your report and
   work around it meanwhile; do not edit the file.
3. **The dependency set is fixed:** `rayon`, `memmap2`, `hashbrown`,
   `foldhash`. Pure Rust, no `-sys` crates, no C build steps. Ask before
   adding anything.
4. **Follow `docs/development.md`**: MSRV 1.89, edition 2024, no panics on
   malformed input, checked arithmetic, deterministic results, `&[u8]` for
   symbol names, rustdoc on every `pub` item.
5. **`unsafe` is confined** to `src/input/` and `src/output/`, and needs a
   `// SAFETY:` comment. Everywhere else the crate-level `deny(unsafe_code)`
   applies.
6. **Before you finish**, all four must pass:
   ```sh
   cargo fmt --all
   cargo clippy --all-targets --all-features -- -D warnings
   cargo test --all-features
   cargo +1.89 check --all-targets --all-features
   ```
7. **Tests come with the code.** Unit tests go beside the code. A workstream
   may also add one integration test file, `tests/<area>.rs`, and data under
   `tests/data/<area>/`. The rest of `tests/` belongs to W9.
8. **Report** what you built, what you had to stub, and any shared-file change
   you need.

## Status

| ID | Area | Owns | Depends on | Ready |
| --- | --- | --- | --- | --- |
| W1 | Command-line parsing | `src/args/**` | — | merged |
| W2 | Input files and archives | `src/input/**` | — | merged |
| W3 | Linker scripts | `src/script/**` | — | merged |
| W4 | Symbol table and interning | `src/symbols/**` | — | merged |
| W5 | Output writer | `src/output/**` | — | merged |
| W6 | GC / ICF / merge passes | `src/passes/**` | — | merged |
| W7 | ELF reading | `src/elf/read/**` | — | merged |
| W8 | ELF layout, writing and link driver | `src/elf/**` (extends `read/` as needed) | W7 | merged (M1) |
| W9 | Test harness and fixtures | `tests/**` except other workstreams' `tests/<area>.rs` and `tests/data/<area>/` | — | merged |
| W10 | DWARF | `src/debug/**` | W7 | in progress |

W1–W7 and W9 can all run at once. They share no files.

---

## W1: Command-line parsing

**Goal:** parse GNU ld / gold / lld / mold command lines into `LinkOptions`.

**Owns:** `src/args/**`. `LinkOptions` (in `src/args/options.rs`) is yours to
extend — add fields as you implement the options that set them.

**Build:**

- An option table covering every option of GNU ld 2.4x, gold, lld and mold,
  each marked *implemented*, *accepted-ignored* or *unsupported*
  (`docs/compatibility.md` defines the policy). A data-driven table beats a
  hand-written `match` chain: tests will enumerate it.
- The parsing rules in `docs/compatibility.md`: one- or two-dash long options
  with the `-o` exception, joined and separate values, `-l:name`, `-z`
  keywords, `@response` files, `=`/`$SYSROOT` prefixes, `--` terminator.
- Positional state tracked per input: `--whole-archive`, `--as-needed`,
  `-Bstatic`/`-Bdynamic`, `--push-state`/`--pop-state`, groups.
- Flavor selection from `argv[0]` and `-flavor`; a stub `parse_darwin` is fine.
- `--help` covering the implemented options, and the exact `--version` line.

**Do not:** touch the filesystem. No path resolution, no `-l` searching, no
reading response files off disk — take a closure or trait for reading files so
tests stay hermetic.

**Done when:** the option corpus in `tests/` parses, unknown options error,
every table entry has a test, and `qld --help` lists what works.

---

## W2: Input files and archives

**Goal:** get bytes and identity for every input.

**Owns:** `src/input/**`.

**Build:**

- A file table that maps inputs with `memmap2`, holds them for the link, and
  hands out `&'a [u8]`. Fall back to reading for small or unmappable files.
  `unsafe` allowed here, with `SAFETY` comments.
- Format identification from magic: ELF, COFF, Mach-O, fat, `!<arch>`,
  `!<thin>`, LLVM bitcode, GCC LTO IR, text.
- A complete `ar` reader: GNU (`/`-terminated names, `//` long-name table),
  BSD (`#1/<len>` names), and thin archives; the `/` symbol index and the
  64-bit `/SYM64/` variant; member iteration that is lazy and parallel-safe.
- Path resolution for `-l`: `libfoo.so` then `libfoo.a` across search paths,
  `-l:exact`, sysroot handling.

**Done when:** archives produced by GNU `ar`, LLVM `llvm-ar` and macOS `libtool`
all parse, symbol indexes are read, malformed archives produce
`Error::Malformed` instead of panicking, and truncated or corrupted archives
never panic in randomized tests.

---

## W3: Linker scripts

**Goal:** a full GNU linker script front end.

**Owns:** `src/script/**`.

**Build:** lexer, parser and expression evaluator for the language listed in
`src/script/mod.rs`, plus input-section pattern matching. Errors carry file and
line.

**Start with** the input-script subset (`INPUT`, `GROUP`, `AS_NEEDED`,
`OUTPUT_FORMAT`), because M2 needs it to read glibc's `libc.so`. Then the
`SECTIONS`/`MEMORY`/`PHDRS` language for M3.

**Done when:** the scripts shipped by glibc and musl parse; a `SECTIONS`
script for a bare-metal target parses and evaluates to the same addresses GNU
ld computes (compare with `ld --verbose` output); corrupted scripts never
panic in randomized tests.

---

## W4: Symbol table and interning

**Goal:** the concurrent global symbol table.

**Owns:** `src/symbols/**`.

**Build:**

- Name interning over `&'a [u8]` with hashes precomputed during parsing
  (`foldhash`), and a sharded table (`hashbrown` + per-shard locks) keyed by
  name and optional version.
- Per-symbol state: current definition (file, section, value, kind) and atomic
  flag bits (needs GOT/PLT/copy/TLS, address-taken, exported).
- A `Resolver` trait for format-specific precedence, so ELF rules live in the
  ELF backend. Provide the ELF rules as a test implementation only.
- The archive-extraction loop shape: rounds of "insert definitions, collect
  undefined, extract members", to a fixpoint, with ties broken by input
  position.

**Done when:** a synthetic benchmark inserting millions of symbols from many
threads gives the same result as a single-threaded run, every time, and scales
with core count.

---

## W5: Output writer

**Goal:** write the output file fast and deterministically.

**Owns:** `src/output/**`.

**Build:**

- An `OutputFile` that creates the output next to its final path, sets its
  length, maps it writable, and replaces any old file on commit; plus an
  in-memory variant for library callers. *(Done: merged.)*
- Safe splitting into disjoint `&mut [u8]` chunks for parallel writers.
- Parallel build-id hashing (block hashes combined into one value), with
  `fast`, `md5`, `sha1`, `uuid` and explicit-hex modes. Pure Rust MD5 and SHA-1
  implementations belong here — they are small; do not add a dependency.
- Finishing touches: permissions (`0o755`), atomic replacement, and optional
  background deletion of a large previous output.

**Done when:** writing a 1 GiB output through N threads gives identical bytes
for every N, and benchmarks show the write stage is I/O bound rather than
CPU bound.

---

## W6: GC / ICF / merge passes

**Goal:** the format-neutral optimization engines.

**Owns:** `src/passes/**`.

**Build:**

- A graph interface the backends implement (sections, edges, roots) so the
  passes do not know about ELF.
- Parallel mark-and-sweep GC with atomic mark bits, plus `--print-gc-sections`
  and a `--why-live` reference chain.
- ICF: parallel hashing then iterative class refinement, `safe` and `all`
  modes, deterministic representative selection.
- Merge-section splitting and deduplication, with optional tail merging.

**Done when:** each pass has property tests on synthetic graphs (results
independent of thread count and of input permutation beyond the documented
tie-break), and micro-benchmarks on graphs of a million sections.

---

## W7: ELF reading

**Goal:** zero-copy parsing of ELF objects and shared libraries.

**Owns:** `src/elf/read/**` (create it), and `src/elf/mod.rs` until W8 starts.

**Build:**

- Header, section header, symbol table, relocation, note, group and
  `.eh_frame` readers, generic over class and endianness through a trait with
  `Elf64Le` as the first instantiation. No run-time branching in hot loops.
- Typed views that never assume alignment and never panic: every accessor
  bounds-checks and returns `Result`.
- Dynamic-object reading: `.dynsym`, `.dynamic`, `SONAME`, `NEEDED`, and the
  version tables.
- Constants for the ELF ABI and for x86-64 relocations, in their own module.

**Done when:** parsing every `.o` and `.so` on the test machine (a sweep of
`/usr/lib`) produces the same symbol and section inventory as `readelf`, with
no panics, and truncated or corrupted objects never panic in randomized tests.

---

## W8: ELF layout and writing (starts after W7)

**Goal:** turn resolved inputs into a static x86-64 executable (roadmap M1).

**Owns:** all of `src/elf/**`, including the driver in `src/elf/link.rs`
(which `crate::link` calls inside a `--threads`-sized pool). W7 is merged, so
W8 may extend `src/elf/read/` where layout needs more from the reader.

**Build:** output section assignment matching GNU ld's default script,
segments, linker-defined symbols, GOT/PLT synthesis, `.eh_frame_hdr`,
x86-64 relocation application and relaxation, and TLS local-exec.

**Done when:** a static "hello world" links, runs, and passes `readelf -a`
inspection.

---

## W9: Test harness and fixtures

**Goal:** the infrastructure every other workstream tests against.

**Owns:** `tests/**`, except the per-area files other workstreams add.

**Build:**

- The `test.toml` fixture runner described in `docs/testing.md`: compile with
  the host toolchain, link with qld, run, compare stdout and `readelf`
  patterns. It must skip cleanly (not fail) while linking is unimplemented.
  No new dependencies: parse the small TOML subset fixtures use by hand.
- A differential runner that links the same inputs with GNU ld and compares
  the normalized properties listed in `docs/testing.md`.
- The captured-argv corpus for W1: real command lines from gcc, clang, rustc,
  meson and cmake builds, with the expected parse results.
- Fuzzing is deferred: `cargo fuzz` needs a separate crate, and qld is kept
  to one crate. Until that is decided, parsers get randomized
  malformed-input tests (truncation and byte-flipping of valid inputs) in
  their unit tests.
- A determinism harness: link with 1, 2 and N threads, compare bytes.

**Done when:** `cargo test` runs the fixture and differential suites, and the
skip behavior keeps CI green until M1 lands.

---

## W10: DWARF (starts after W7)

**Goal:** debug information handling.

**Owns:** `src/debug/**`.

**Build:** tombstone values for relocations into dead sections, `SHF_COMPRESSED`
and `.zdebug_*` decompression, zlib/zstd output compression, and the lazy
line-table lookup that turns a section offset into `file:line` for diagnostics.

---

## Integration follow-ups

Changes requested by merged workstreams that need a frozen shared file, or
that cross workstream boundaries. The integrator does these between merges.

| From | Change | Status |
| --- | --- | --- |
| W2 | `Error` variant for "library not found" (`cannot find -lfoo`) | done: `Error::NotFound` |
| W2 | `Error` variant for "too many input files" | done: `Error::Limit` |
| W5 | `Error` variant for internal/layout errors | done: `Error::Internal` |
| W6 | `Error::Internal` so backend-bug `InputError`s convert into `crate::Error` | done |
| W4 | `const fn` ID accessors and `Default` on IDs | done |
| W1 | Emit `LinkOptions::warnings` to the diagnostic sink, honoring `--no-warnings` / `--fatal-warnings` | done (`main.rs`) |
| W1 | Re-export `parse_gnu_with` from the crate root | done |
| W3 | `Error` variant for script errors with line and column | done: `Error::Script` |
| W3 | Layout-side script semantics: `DATA_SEGMENT_*` relro adjustment, `PROVIDE` only-if-referenced, `NEXT_SECTION`, section-relative symbols from `.` | open, for W8 / M3 |
| W1 | Let `ParseOutcome` grow print-and-exit variants (`--print-sysroot`, `--print-output-format`); `main.rs` must handle them | open |
| W1 | `target.rs`: more operating systems (FreeBSD, …) and architectures (MIPS, PowerPC32, …) for their `-m` emulations | open, when needed |
| W6 | Split `merge_sections` into a parse-time split step and a post-GC dedup/offset step, so the relocation scan can map references to pieces (see architecture stage 4 and 8) | done: `split_section` + `merge_split_sections` |
| W8 | `Location` label so duplicate symbols print `>>> defined at` | done: `Diagnostic::detail` |
| W8 | Let the ELF driver size the pool when `--threads` is absent | done (`lib.rs`) |
| W8 | `resolve_symbols` runs parallel iterators over all files every round, including inactive archive members: 17 ms at 64 threads vs 2.5 ms on one for static hello. Iterate only the round's files, or `with_min_len` | open |
| W8 | `ResolveFile` hook between loading and interning, so COMDAT groups can be claimed before insertion | open |
| W8 | Switch the local debug tombstone helper in `elf/write.rs` to W10's rules, and handle compressed inputs via W10 | open, after W10 |
| W8 | `ifunc-static` fixture prints "same address: no" when built with clang, under GNU ld too: fixture/toolchain issue | open |
| W7 | `elf_read::basic_object_matches_readelf` failed once under the full parallel test run, passed on reruns: possible flake | open, investigate |
| W5 | Pre-allocate output with `fallocate`: filling a fresh 1 GiB mapped file costs ~900 ms of page-fault block allocation on btrfs. Needs a syscall crate (`rustix` is pure Rust) — dependency decision | open |

## Launching an agent

Give the agent this brief:

> You are implementing workstream **W\<n\>** in the qld repository at
> `/home/magicaltux/projects/qld`. Read `CLAUDE.md`, `docs/workstreams.md`
> (your section), `docs/architecture.md`, `docs/development.md` and the
> module doc comment in the directory you own. Implement the workstream.
> Touch only the files your workstream owns. Run the four checks listed in
> the ground rules before reporting. Report what you built, what is stubbed,
> and any change you need in a frozen shared file.

Run each agent on its own branch (`ws/1-args`, `ws/2-input`, …) or in its own
worktree, and merge through review. Because ownership does not overlap, merges
should be clean; the only conflicts to expect are in shared files, which
agents are not allowed to edit.
