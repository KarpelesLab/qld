# Testing and benchmarking

## Test tiers

| Tier | What | Where | Runs |
| --- | --- | --- | --- |
| Unit | Parsers, expression evaluator, relocation math, hash tables | `#[cfg(test)]` beside the code | every commit |
| Option corpus | Real argv captured from gcc/clang/rustc/cargo/meson/cmake builds, parsed and checked against the expected `LinkOptions` | `tests/corpus/` | every commit |
| Fixtures | Small C/C++/asm/Rust programs compiled with the host toolchain, linked by qld, **executed**, and their output checked | `tests/fixtures/` | every commit |
| Differential | The same link done by qld and GNU ld (and lld where available); normalized `readelf`/`objdump` output compared | `tests/differential.rs` | every commit |
| Cross-arch | Fixtures cross-compiled and run under `qemu-user` | `tests/fixtures/` + CI matrix | every commit (x86-64, aarch64, riscv64); nightly (others) |
| Real projects | Build and test suites of external projects with qld as the linker, with `.dynsym`/`DT_*` comparison against GNU ld relinks | `tests/projects/*.sh` (see `tests/projects/README.md`) | manual / nightly |
| Corruption | Every committed fixture mutated by a seeded generator and fed back to the readers and the link drivers; nothing may panic, hang or grow without bound | `tests/corrupt.rs` | every commit (short), nightly (long) |
| Fuzzing | `cargo fuzz` targets for every parser and the linker script evaluator | `fuzz/` | deferred: needs a second crate (see below) |
| Determinism | Each fixture and benchmark linked with 1, 2 and N threads; outputs must be byte-identical | harness option | every commit |
| Diagnostics | The rendered shape of an error, a warning and a note, compared with GNU ld and lld | `tests/diag.rs`, `src/diag.rs` unit tests | every commit |

### Fixture format

Each fixture is a directory holding source files and a `test.toml`:

```toml
# tests/fixtures/tls-gd-to-le/test.toml
compile = ["cc -O2 -fPIC -c tls.c", "cc -O2 -c main.c"]
driver  = "cc"                          # link through the compiler driver,
link    = "-static -o out main.o tls.o" # which adds crt files and libc
run     = "./out"
expect.stdout = "42\n"
expect.readelf = ["PT_TLS"]                       # must / must-not ("!") appear
expect.readelf_arch.x86_64 = ["!R_X86_64_TPOFF64"] # only when linking for x86-64
targets = ["x86_64-linux-gnu", "aarch64-linux-gnu"]
```

Without `driver`, `link` is passed to the linker as raw arguments, which only
suits programs that need no libc. With `driver = "cc"`, the compiler driver is
pointed at the linker under test (`-B` for gcc, `--ld-path=` for clang). The
complete key list, environment variables (`QLD_FIXTURE`,
`QLD_FIXTURE_LINKER`, `QLD_TEST_*`) and skip rules are in
[`tests/README.md`](../tests/README.md).

Every fixture is validated against GNU ld:
`cargo test --test fixtures -- --ignored` runs them all with it. A fixture
where qld intentionally differs from GNU ld is marked `gnu_ld = "fail"`.

Checks are substring matches today. FileCheck-style patterns against
`readelf`/`llvm-readobj` output are planned, so that test ideas can be adapted
from existing linker test suites.

### Corruption testing instead of `cargo fuzz`

`cargo fuzz` needs a second crate, and qld is one crate
([CLAUDE.md](../CLAUDE.md)), so `tests/corrupt.rs` stands in for it. It reads
every binary fixture under `tests/data/` — ELF objects and shared objects,
`ar` archives, PE/COFF objects, import libraries and a DLL, Mach-O objects,
dylibs and fat files, `.tbd` stubs, linker and version scripts — classifies
each by its magic, mutates it with a seeded xorshift generator and runs the
result back through the readers and the link drivers, in memory, writing
nothing.

The mutations are bit flips, single bytes, absurd little- and big-endian
counts, lengths and offsets, truncation, zeroed ranges, and one range copied
over another (which produces overlapping and cyclic structures). Every
mutation must come back as `Ok` or an [`qld::Error`]; a panic fails the test
with the seed, the fixture and the list of mutations, so it reproduces
exactly. A watchdog thread aborts the process, naming the case, if one case
takes longer than two minutes or the process outgrows 4 GiB, because a test
harness cannot kill a thread that is stuck.

| Variable | Meaning |
| --- | --- |
| `QLD_CORRUPT_SEEDS` | Comma-separated seeds (decimal or `0x…`), replacing the fixed set |
| `QLD_CORRUPT_ITERS` | Mutations per fixture per seed (default 24) |
| `QLD_CORRUPT_LIST` | Print the corpus and how each file was classified |

A long local run:

```sh
QLD_CORRUPT_ITERS=4000 QLD_CORRUPT_SEEDS=101,202,303,404,505,606,707,808 \
  cargo test --release --all-features --test corrupt -- --test-threads=2
```

Each format also has its own corruption test beside its reader
(`tests/elf_read.rs`, `tests/coff_read.rs`, `tests/macho_read.rs`,
`tests/script.rs`, `tests/debug.rs`); `tests/corrupt.rs` is the cross-cutting
one that also covers the link drivers.

### Diagnostics

`src/diag.rs`'s unit tests pin the rendering — the `>>>` lines, the aligned
source position, the colours, the error limit — and `tests/diag.rs` runs the
`qld` binary and checks the shape of an undefined symbol, a duplicate symbol,
a missing library, an unknown option and a malformed input against what GNU
ld 2.46 and lld 23 print. `docs/compatibility.md` records the differences
that are on purpose.

Because `Stderr` sorts by `Diagnostic::order`, it renders nothing until it is
flushed, so a test that reads qld's standard error sees everything at once,
after the link map and anything else written to standard output.

### Borrowing upstream test suites

- **mold** (MIT): its shell test scripts cover most GNU ld behaviors and
  translate directly into fixtures.
- **lld** (Apache-2.0 WITH LLVM-exception): `lld/test/ELF` and friends are
  lit/FileCheck tests. They can be run against qld with a small lit shim,
  keeping their license headers.
- **binutils `ld-*` testsuite** (GPL): used only as an **external** test
  runner. Its files are not copied into this repository.

### Differential testing normalization

Addresses, section order and padding legitimately differ between linkers, so
the comparison looks at:

- the set of dynamic symbols, their binding, visibility and version
- `DT_*` entries (values compared symbolically where they are addresses)
- the multiset of dynamic relocation types against their symbols
- program behavior: exit code and stdout of the executed binary
- section flags and presence, but not addresses

## Host platforms in CI

| Host | Purpose |
| --- | --- |
| Linux x86-64 | Primary: all tiers |
| Linux aarch64 | Native AArch64 ELF fixtures |
| macOS arm64 / x86-64 | Mach-O fixtures (M8), host build of qld |
| Windows x86-64 | PE fixtures (M7), host build of qld |

MSRV (Rust 1.89) CI builds the crate and runs the unit tests.
Integration tiers run on current stable.

## Benchmarks

`benches/` contains drivers that capture a link once and replay it (the full
method, corpus and latest results are in
[tests/projects/bench.md](../tests/projects/bench.md)):

1. `benches/capture.py` records a link's command line and working directory
   from a ninja target, a cargo build or a raw argv
   (`tests/projects/bench-capture.sh` recaptures the whole corpus).
2. `benches/run.py SPECS` replays each link with every linker at several
   thread counts, interleaving runs so load changes affect all linkers
   alike. It records wall time, CPU time and peak RSS (`wait4`), output size
   and load average, and smoke-checks every output.
3. `benches/run.py SPECS --determinism QLD` checks that qld's output is
   identical across thread counts; `benches/report.py` renders tables.

**Standard corpus:** clang (release and debug), chromium (debug), rustc,
Linux kernel `vmlinux`, Firefox `libxul.so`, and a large debug-info Rust
binary (for example `cargo` built in debug mode).

**Compared against:** GNU ld, gold, lld, mold, wild — each at its latest release.

The results are published per commit, so a regression shows up in the commit
that introduced it.
