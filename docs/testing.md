# Testing and benchmarking

## Test tiers

| Tier | What | Where | Runs |
| --- | --- | --- | --- |
| Unit | Parsers, expression evaluator, relocation math, hash tables | `#[cfg(test)]` beside the code | every commit |
| Option corpus | Real argv captured from gcc/clang/rustc/cargo/meson/cmake builds, parsed and checked against the expected `LinkOptions` | `tests/corpus/` | every commit |
| Fixtures | Small C/C++/asm/Rust programs compiled with the host toolchain, linked by qld, **executed**, and their output checked | `tests/fixtures/` | every commit |
| Differential | The same link done by qld and GNU ld (and lld where available); normalized `readelf`/`objdump` output compared | `tests/differential.rs` | every commit |
| Cross-arch | Fixtures cross-compiled and run under `qemu-user` | `tests/fixtures/` + CI matrix | every commit (x86-64, aarch64, riscv64); nightly (others) |
| Real projects | Build and test suites of external projects with qld as the linker | `tests/projects/` scripts | nightly |
| Fuzzing | `cargo fuzz` targets for every parser and the linker script evaluator | `fuzz/` | continuous / nightly |
| Determinism | Each fixture and benchmark linked with 1, 2 and N threads; outputs must be byte-identical | harness option | every commit |

### Fixture format

Each fixture is a directory holding source files and a `test.toml`:

```toml
# tests/fixtures/tls-gd-to-le/test.toml
compile = ["cc -O2 -fPIC -c tls.c", "cc -O2 -c main.c"]
driver  = "cc"                          # link through the compiler driver,
link    = "-static -o out main.o tls.o" # which adds crt files and libc
run     = "./out"
expect.stdout = "42\n"
expect.readelf = ["PT_TLS", "!R_X86_64_TPOFF64"]  # must / must-not appear
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

`benches/` contains drivers that capture a link once and replay it:

1. Build the project with `-Wl,--reproduce=repro.tar` (lld and mold support
   this; qld will too) to capture every input and the command line.
2. Replay the link with each linker: hyperfine for wall time, `/usr/bin/time`
   for peak RSS, `perf stat` for CPU time.

**Standard corpus:** clang (release and debug), chromium (debug), rustc,
Linux kernel `vmlinux`, Firefox `libxul.so`, and a large debug-info Rust
binary (for example `cargo` built in debug mode).

**Compared against:** GNU ld, gold, lld, mold, wild — each at its latest release.

The results are published per commit, so a regression shows up in the commit
that introduced it.
