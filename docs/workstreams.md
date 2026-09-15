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
| W10 | DWARF | `src/debug/**` | W7 | merged |
| W11 | Dynamic ELF on x86-64 (M2) | `src/elf/**` | W8 | merged (M2) |
| W12 | Symbol resolution follow-ups | `src/symbols/**` | W4 | merged |
| W13 | Symbol hints and demangling | `src/hints/**`, `src/demangle/**` | — | merged |
| W16 | M2 exit criteria: real-world builds | `src/elf/**`, `tests/projects/**` | W11 | merged (M2 met) |
| W14 | PE/COFF reading | `src/coff/**` | — | merged |
| W17 | LTO plugin host | `src/plugin/**` | — | merged |
| W18 | LTO in the ELF driver (M6) | `src/elf/{inputs,resolve,object,lto}*`, `tests/lto*` | W17 | merged (M6) |
| W19 | Linker-script layout and raw outputs (M3) | `src/elf/{rules,place,layout,write,defined,script_layout,rawout}*`, `tests/script_link*` | W3, W16 | merged (M3) |
| W20 | AArch64 ELF (M4) | `src/elf/arch/**`, `src/arch/**`, thunk/relaxation hooks in `src/elf/{layout,write,scan,synth}.rs`, `tests/aarch64*` | W16 | merged |
| W21 | PE/COFF linking (M7) | `src/coff/**` (beyond `read/`), `tests/coff_link*` | W14 | merged |
| W23 | Write-based (`pwrite`) output backend | `src/output/**`, writer call sites in `src/elf/` and `src/coff/` | W5 | merged (default) |
| W24 | Benchmarks and performance (M5) | `benches/**`, `tests/projects/bench*`, performance changes in `src/elf/**`, `src/symbols/**`, `src/passes/**`, `src/output/**`, `src/input/**`, `src/debug/**` | W23 | in progress |
| W25 | Mach-O linking (M8) | `src/macho/**`, the ld64 front end in `src/args/`, `tests/macho_link*` | W15 | merged |
| W22 | PE command-line options | `src/args/**`, `src/coff/options.rs` | W21 | in progress |
| W15 | Mach-O reading | `src/macho/**` | — | merged |

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

## W11: Dynamic ELF on x86-64 (M2)

**Goal:** roadmap M2 — PIE, shared object inputs and outputs, PLT/GOT,
`.dynamic`, symbol versioning, RELRO, `DT_RELR`, `-r`.

**Owns:** `src/elf/**` (taking over from W8), `tests/elf_link.rs`, new fixture
directories.

**Done when:** every M2 fixture in `tests/fixtures/` passes, the differential
runner agrees with GNU ld on dynamic symbols, `DT_*` tags and dynamic
relocations, and the PIE and shared-library outputs load under glibc `ld.so`.

---

## W12: Symbol resolution follow-ups

**Goal:** the two requests W8 made of `src/symbols/`: per-round work
proportional to the round's files (not all files), and a hook between loading
a round's files and interning their symbols so backends can claim COMDAT
groups before insertion.

**Owns:** `src/symbols/**`, `tests/symbols.rs`.

---

## W13: Symbol hints and demangling

**Goal:** "intelligent library symbol matching" diagnostics
(`docs/optimizations.md`) and a demangler for Itanium C++ and Rust names.

**Owns:** `src/hints/**`, `src/demangle/**`, `tests/hints.rs`,
`tests/demangle.rs`. Wiring into the ELF driver is a later integration step.

---

## W16: M2 exit criteria (real-world builds)

**Goal:** make ROADMAP M2's exit criteria pass: coreutils, curl, openssl,
zlib, Python, the Rust compiler and LLVM/clang build with qld as the system
linker and pass their test suites; qld-built shared objects load under glibc
and musl `ld.so`. Every bug found gets a minimal regression fixture.

**Owns:** `src/elf/**`, `tests/projects/**` (build scripts), new fixtures.

---

## W14: PE/COFF reading

**Goal:** the input side of M7, like W7 for ELF: zero-copy readers for COFF
objects (including bigobj), short and long import libraries, PE images
(exports of DLLs linked directly), `.drectve` directives and `.def` files.

**Owns:** `src/coff/**`, `tests/coff_read.rs`, `tests/data/coff_read/`.

---

## W15: Mach-O reading

**Goal:** the input side of M8: zero-copy readers for Mach-O objects and
dylibs (arm64, x86_64), fat slice selection, `.tbd` text stubs (v3–v5), and
the per-atom information dead stripping needs (`.subsections_via_symbols`).

**Owns:** `src/macho/**`, `tests/macho_read.rs`, `tests/data/macho_read/`.

---

## W17: LTO plugin host

**Goal:** the linker half of the GNU linker plugin API (`-plugin`,
`-plugin-opt`), loading GCC's `liblto_plugin.so` and LLVM's `LLVMgold.so`,
as a standalone API: claim IR inputs, report their symbols, take
resolutions, and collect the native objects the plugin produces. Wiring it
into the ELF driver's resolution rounds is a later integration step.

**Owns:** `src/plugin/**`, `tests/plugin.rs`, `tests/data/plugin/`.

---

## W18: LTO in the ELF driver (M6)

**Goal:** `gcc -flto` and `clang -flto`/`-flto=thin` links work end to end:
IR inputs (including archive members) are claimed through
`plugin::Session`, take part in resolution, and are replaced by the native
objects the plugin produces. Meets ROADMAP M6's exit criteria.

**Owns:** `src/elf/inputs.rs`, `src/elf/resolve.rs`, `src/elf/object.rs`, a
new `src/elf/lto.rs`, `tests/lto.rs`, new fixtures. Shares `src/elf/link.rs`
with W19 (keep edits there small).

---

## W19: Linker-script layout and raw outputs (M3)

**Goal:** `-T script` drives layout (`SECTIONS`, `MEMORY`, `PHDRS`, `AT>`,
`KEEP`, `/DISCARD/`, `PROVIDE`, `ASSERT`, `INSERT`), `-Ttext`/`--section-start`,
and `--oformat binary|ihex|srec` plus `-b binary` inputs. Meets ROADMAP M3's
exit criteria (Linux kernel boots; bare-metal addresses match GNU ld).

**Owns:** `src/elf/rules.rs`, `src/elf/place.rs`, `src/elf/layout.rs`,
`src/elf/write.rs`, `src/elf/defined.rs`, new `src/elf/script_layout*` and
`src/elf/rawout*` modules, `tests/script_link.rs`, new fixtures. Shares
`src/elf/link.rs` with W18 (keep edits there small).

---

## W20: AArch64 ELF (M4)

**Goal:** qld links AArch64 Linux ELF: relocations, range-extension thunks,
GOT/PLT, TLS (including TLSDESC), BTI/PAC properties, and the ADRP
relaxations. Exit criterion: the fixture suite passes for
`aarch64-unknown-linux-gnu`, compared against that target's GNU ld, and a
CI job on an arm64 runner runs the binaries.

**Owns:** `src/elf/arch/**`, `src/arch/**`, the architecture hooks in
`src/elf/{layout,write,scan,synth,values}.rs`, `tests/aarch64.rs`, new
`aarch64-*` fixtures.

---

## W21: PE/COFF linking (M7)

**Goal:** qld produces Windows PE32+ executables and DLLs from COFF objects,
MinGW flavor: symbol resolution, layout, imports and exports, base
relocations, SEH, and `--out-implib`. The readers are merged (W14).

**Owns:** `src/coff/**` outside `read/`, `tests/coff_link.rs`, new fixtures.

---

## W24: Benchmarks and performance (M5)

**Goal:** a reproducible benchmark suite comparing qld with GNU ld, lld, mold
and wild on real links (clang, rustc's `librustc_driver`, the Linux kernel,
large debug-info binaries), then profiling-driven work toward M5's exit
criterion: wall time at or below mold and wild at 8 and 64 cores, peak RSS no
higher than lld's, identical output across thread counts.

**Owns:** `benches/**`, benchmark scripts under `tests/projects/`, and
performance changes in the link pipeline modules.

---

## W25: Mach-O linking (M8)

**Goal:** qld links arm64 and x86_64 macOS executables and dylibs through an
ld64-compatible command line, including chained fixups, `__unwind_info`, the
ad-hoc code signature and universal binaries; the macOS CI runner executes
the results. The readers are merged (W15).

**Owns:** `src/macho/**` outside `read/` (extending `read/` as needed), the
ld64 flavor parser in `src/args/`, `tests/macho_link.rs`, new fixtures.

---

## Integration follow-ups

- **W25:** `link()` dispatch for Mach-O and the `macho-macos` CI job are done.
  Open: move `src/macho/sha256.rs` into `src/output/hash` (after W24 merges);
  `-r`, `-bundle_loader`, `-init`, `-alias`, `-flat_namespace`, arm64e and
  LTO are rejected by name; cstring deduplication and ObjC relative method
  lists are missing; legacy (pre-chained-fixups) output binds everything at
  load time; `-v` with inputs prints a note instead of the version on stdout.

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
| W3 | Layout-side script semantics: `DATA_SEGMENT_*` relro adjustment, `PROVIDE` only-if-referenced, `NEXT_SECTION`, section-relative symbols from `.` | done (W19) |
| W1 | Let `ParseOutcome` grow print-and-exit variants (`--print-sysroot`, `--print-output-format`); `main.rs` must handle them | open |
| W1 | `target.rs`: more operating systems (FreeBSD, …) and architectures (MIPS, PowerPC32, …) for their `-m` emulations | open, when needed |
| W6 | Split `merge_sections` into a parse-time split step and a post-GC dedup/offset step, so the relocation scan can map references to pieces (see architecture stage 4 and 8) | done: `split_section` + `merge_split_sections` |
| W8 | `Location` label so duplicate symbols print `>>> defined at` | done: `Diagnostic::detail` |
| W8 | Let the ELF driver size the pool when `--threads` is absent | done (`lib.rs`) |
| W8 | `resolve_symbols` runs parallel iterators over all files every round, including inactive archive members: 17 ms at 64 threads vs 2.5 ms on one for static hello. Iterate only the round's files, or `with_min_len` | done (W12) |
| W8 | `ResolveFile` hook between loading and interning, so COMDAT groups can be claimed before insertion | done (W12) |
| W8 | Switch the local debug tombstone helper in `elf/write.rs` to W10's rules, handle compressed inputs and `--compress-debug-sections` via `crate::debug`, and pass `LinkOptions::dead_reloc_in_nonalloc` | done (W11) |
| W10 | `-z dead-reloc-in-nonalloc=` parsing in `src/args` | done |
| W13 | Call `hints::Hinter` from the ELF driver's undefined-symbol report (integration code in W13's report: collect `LinkedLibrary` with `--as-needed`/`-Bstatic` state, then `hints::attach`), and demangle symbol names in diagnostics with `hints::display_symbol` | done (W11) |
| W13 | Duplicate-definition explanations (which reference extracted which member) need an extraction trace from `symbols` | open |
| CI | Merge x86 `GNU_PROPERTY_X86_ISA_1_USED` / `FEATURE_2_USED` properties (OR-AND semantics: kept only if every input has them) instead of dropping them; GNU `as` emits them and GNU ld keeps them | done (W11) |
| W17 | Integrate `plugin::Session` into the ELF driver: claim IR inputs in `RoundHook::after_load` (deterministic order), feed claimed symbols to resolution (arena-owned names), settle `LDPR_*` resolutions after the fixpoint, re-resolve with `LtoOutput::files`/`libraries`, `finish` after writing (sketch in W17's report) | done (W18) |
| W17 | `Error::Plugin(String)` variant; a "referenced from a non-IR file" symbol flag for `PREVAILING_DEF` vs `_IRONLY` | `Error::Plugin` done; the symbol flag is open |
| W18 | `-r` with a shared-library input writes `R_X86_64_NONE` for references bound to it (GNU ld's `-r` implies `-Bstatic`) — `src/elf/relocatable.rs` | open |
| W20 | Run the AArch64 fixtures natively | done (`arm64-linux` job) |
| W20 | `-z force-bti` needs an option (the backend has the landing-pad PLT form); `--help` still says `supported emulations: elf_x86_64`; `--oformat elf64-littleaarch64` is rejected | open, for W22 (`src/args`) |
| W20 | Script layout uses a 4 KiB page size instead of the architecture default (64 KiB on AArch64), so `-T` links need `-z max-page-size=0x10000` | open |
| W20 | `--emit-relocs` maps relaxed relocations back to `R_X86_64_*` names; `-b binary` inputs are stamped `EM_X86_64`; `--icf=safe` treats every non-`PLT32` relocation as address-taking | open |
| W20 | The fixture harness skips a non-host target when `qemu-<arch>` is missing even for fixtures with no `run` line | open |
| W23 | Single-threaded links are 5–12% slower with the write backing (one extra copy); build-id with ~1 MiB sections reads blocks back; a 320 MiB synthetic link with `--build-id` spends ~600 ms in a stage that does not scale with threads under every backing | open |
| Integrator | Regression from W20, found by W23: GD/TLSDESC→IE relaxations looked up the wrong GOT slot (clang-23 failed to link). Fixed, with fixture `tls-gd-to-ie-shared`; nothing in CI covered it before | done |
| W21 | Wire `coff::link` into `crate::link` | done |
| W21 | Run qld-built PE images on a Windows CI runner | done (`pe-windows` job) |
| W21 | i386 (PE32) and ARM64 PE output; i386 SafeSEH; delay-load imports; `-r`, `--gc-sections`, `--icf`, `--wrap`, `--defsym` and linker scripts for PE | open |
| W21 | Local symbols in the PE output symbol table; auto-import of PC-relative references; `--enable-stdcall-fixup`; `--add-stdcall-alias` | open |
| W19 | `-r` with `-T`, and `--defsym` expressions beyond `symbol+offset` | open |
| W19 | `-r` output of the kernel's `vmlinux.o` makes objtool fail ("can't find starting instruction") — `src/elf/relocatable.rs` | open |
| W19 | GLOBAL HIDDEN inputs emitted LOCAL in static links; no `FILE` symbols; `sym = othersym` script assignments do not copy type/size | open |
| W19 | vdso64 differs from GNU ld in `.hash` alignment/size, `.dynstr`, `.dynamic` and an empty `.got.plt` | open |
| W19 | A hook in `src/elf/inputs.rs` for script-provided input lists and `-b binary`, to drop the workaround in `script_layout/load.rs` | open |
| W18 | clang's `.eh_frame` keeps `SHT_X86_64_UNWIND` where GNU ld writes `PROGBITS` — `src/elf/write.rs`/layout | open, for W19 |
| W12 | Adopt `RoundHook` + `GroupClaims` for COMDAT in `src/elf/` (claim before insertion; delete `redirect_discarded`) | done (W11) |
| W10 | Fill `Location::source` for undefined-symbol diagnostics with `debug::dwarf::LineLookup` | done (W11) |
| W8 | `ifunc-static` fixture prints "same address: no" when built with clang, under GNU ld too: fixture/toolchain issue | open |
| W7 | `elf_read::basic_object_matches_readelf` failed once under the full parallel test run, passed on reruns: possible flake | done: two tests raced on the shared `basic.o`; it is now built once |
| W5 | Pre-allocate output with `fallocate` | dropped: measured on btrfs, `posix_fallocate` changes a 1 GiB mapped write by 0–15%; the cost is page-fault contention, which grows with threads (400 ms at 1 thread, ~860 ms at 16), while `pwrite` stays at ~260–370 ms. No crate would be needed anyway (a hand-declared `posix_fallocate`, like the plugin host's `dlopen`). Replaced by W23 |

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
