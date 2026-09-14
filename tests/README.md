# qld integration tests

Everything here runs under `cargo test`. The suites that need a host
toolchain skip themselves, with a `SKIPPED: <reason>` line, when a tool is
missing or when qld reports "not implemented yet". Skips never fail the build,
so CI stays green while the pipeline is being built.

| Path | What |
| --- | --- |
| `fixtures.rs` | Fixture runner: compile, link with qld, run, check output and `readelf` |
| `fixtures/<name>/` | One fixture: sources plus `test.toml` |
| `differential.rs` | Links every fixture with GNU ld and qld, compares normalized properties |
| `corpus.rs`, `corpus/*.txt` | Real linker argv from gcc/clang/rustc/CMake/Meson, fed to `qld::args::parse_gnu` |
| `common/` | Shared helpers: tool discovery, process runner, TOML subset parser, `readelf` parsing, determinism harness |
| `<area>.rs`, `data/<area>/` | Per-workstream integration tests (owned by that workstream) |

Scratch directories live under `target/tmp/qld-tests/<suite>/<fixture>/`,
are recreated on every run and are kept afterwards for inspection. A failure
message names the directory and lists every command that ran, with its output.

## Running

```sh
cargo test --all-features                          # everything
cargo test --test fixtures -- --nocapture          # fixtures, with per-fixture lines
QLD_FIXTURE=tls,pie cargo test --test fixtures     # only fixtures whose name contains "tls" or "pie"
cargo test --test fixtures -- --ignored            # validate the fixtures against GNU ld
QLD_FIXTURE_LINKER=lld cargo test --test fixtures  # run the fixtures with another linker
cargo test --test differential -- --ignored        # differential self-check: GNU ld vs GNU ld
```

`--nocapture` shows the `PASS`/`SKIPPED`/`FAIL` line of every fixture and
the summary; without it, output is shown only when the suite fails.

## Environment variables

| Variable | Effect |
| --- | --- |
| `QLD_FIXTURE` | Comma-separated substrings; only fixtures whose name contains one of them run (fixtures and differential) |
| `QLD_FIXTURE_LINKER` | Linker for `fixtures`: `qld` (default), `ld`/`bfd`, `lld`, `mold`, `gold`, or a path |
| `QLD_DIFF_CANDIDATE` | Linker compared against GNU ld by `differential` (default `qld`) |
| `QLD_REQUIRE_TOOLS` | When set to a non-empty value other than `0`, a missing tool fails instead of skipping (for CI jobs that install the toolchain) |
| `QLD_REQUIRE_COFF_TOOLS` | Same, for the PE/COFF reader tests (MinGW, `llvm-dlltool`, `llvm-readobj`); separate because CI jobs with a C toolchain may lack MinGW |
| `QLD_COFF_SWEEP_DIRS`, `QLD_COFF_READOBJ_ARCHIVES`, `QLD_COFF_FUZZ_ROUNDS` | Extra directories for the ignored COFF sweep, archives to compare against `llvm-readobj`, and corruption rounds; COFF fixtures are regenerated with `tests/data/coff_read/generate.sh` |
| `QLD_TEST_JOBS` | Parallel fixture jobs (default: available parallelism) |
| `QLD_TEST_CC`, `QLD_TEST_CXX`, `QLD_TEST_AR` | Compiler driver and archiver (default `cc`/`gcc`/`clang`, `c++`/`g++`/`clang++`, `ar`/`llvm-ar`) |
| `QLD_TEST_READELF` | `readelf` to use (default `readelf`, then `llvm-readelf`) |
| `QLD_TEST_GNU_LD`, `QLD_TEST_LLD`, `QLD_TEST_MOLD`, `QLD_TEST_GOLD` | Reference linker binaries |

An empty value for a `QLD_TEST_*` tool variable means "treat it as not
installed", which is a quick way to check the skip paths.

## Skip semantics

A fixture job is reported as skipped, not failed, when:

- qld's output on a failed link contains `not implemented yet` (the text of
  `qld::Error::Unimplemented`), during the main link or a determinism relink;
- a required tool is missing: `sh`, the compiler, `ar`, `readelf`, the
  reference linker, or for a non-host target the cross compiler and
  `qemu-<arch>` (`QLD_REQUIRE_TOOLS` turns these into failures);
- the host is not a Linux target (the fixtures produce ELF);
- the fixture sets `skip = "reason"` (or `diff.skip` for the differential runner).

Everything else is a failure: a compile command failing, a link failing for
any other reason, a wrong exit code or stdout, a `readelf` pattern mismatch,
or non-deterministic output.

The corpus test skips each command line while `parse_gnu` returns
`Error::Unimplemented`, and fails on any other error, including an unknown
option.

## Adding a fixture

1. Create `tests/fixtures/<name>/` with the sources and a `test.toml`.
2. Validate it against GNU ld, which must pass before qld can be expected to:
   `QLD_FIXTURE=<name> cargo test --test fixtures -- --ignored --nocapture`.
3. Optionally check that the differential runner's normalization is stable:
   `QLD_FIXTURE=<name> cargo test --test differential -- --ignored`.

Name symbols that `readelf` patterns look for distinctively (`dead_function_qld`
rather than `dead`) so that a pattern cannot match something in libc.

### `test.toml` reference

```toml
description = "one line saying what is tested"

# Shell commands (sh -c), run in order in the scratch directory, which starts
# as a copy of the fixture directory. The first word `cc`, `gcc`, `c++`, `g++`
# or `ar` is replaced by the discovered tool (or the cross tool for a non-host
# target); $CC, $CXX and $AR are also exported. Put one command per entry.
compile = ["cc -O2 -c main.c", "ar rcs libfoo.a foo.o"]

# Without `driver`, each `link` entry is the linker's own argv (a raw
# `ld` command line), split with shell quoting rules but not expanded.
# With `driver`, it is the argv of that compiler driver ("cc" or "c++"), which
# is pointed at the linker under test through `-B<shim>/` (gcc) or
# `--ld-path=` (clang). A string or an array of links run in order.
driver = "cc"
link = ["-shared -o libfoo.so foo.o", "-o out main.o -L. -lfoo -Wl,-rpath,$ORIGIN"]

output = "out"               # file for expect.readelf; default: last link's -o
run = "./out"                # shell command; under qemu-<arch> for cross targets
expect.stdout = "42\n"       # exact match; """multi-line strings""" are handy
expect.exit = 0              # default 0
expect.readelf = ["PT_TLS", "!R_X86_64_TPOFF64"]  # substrings; "!" = must not appear
expect.readelf_files."libfoo.so" = ["Library soname: [libfoo.so]"]
expect.readelf_args = "-a"   # arguments after `readelf -W` (default -a)

targets = ["x86_64-linux-gnu"]  # default: the host, if it is Linux
determinism = true           # relink with --threads=1, 2, N; outputs must be identical
gnu_ld = "fail"              # GNU ld rejects this link on purpose (docs/compatibility.md)
timeout = 60                 # seconds per command
skip = "reason"              # disable the fixture everywhere

diff.ignore = ["dynamic: DEBUG"]  # differential: drop property lines containing these
diff.skip = "reason"              # differential: skip this fixture
```

Keys outside this list are errors, so typos do not silently disable checks.
The TOML parser (`common/toml.rs`) supports strings (basic, literal,
multi-line), integers, booleans, arrays, dotted and quoted keys, `[tables]`
and comments; not floats, dates or inline tables.

`gnu_ld = "fail"` means: when the fixture runs with GNU ld (the `--ignored`
validation), the link must fail, which confirms that the difference is real;
with qld, the fixture must link and pass its checks as usual.

### Determinism

With `determinism = true`, the compiled inputs are snapshotted before the
first link, then relinked in fresh directories with `--threads=1`, `2` and N
(N = available parallelism, at least 4); every `-o` output must be
byte-identical to the first link's. Linkers without `--threads` (GNU ld,
gold) are relinked once without it, which still validates the fixture.

## Differential runner

For each fixture (host target only), the compiled inputs are linked by the
candidate (qld) and by GNU ld in separate directories. For every `-o` output
it compares, as sorted multisets of text lines:

- `dynsym:` dynamic symbols with version suffix, type, binding, visibility,
  and `UND`/`ABS`/`COM`/`DEF`
- `dynamic:` `DT_*` tags; values only for `NEEDED`, `SONAME`, `RPATH`,
  `RUNPATH`, `FLAGS`, `FLAGS_1` and similar non-address tags
- `reloc:` relocation section, type and symbol, with a count
- `section:` name, type and flags (not addresses, sizes or order)
- `run:` exit status and stdout lines, when the fixture has `run`

Differences are printed as `-` (GNU ld) and `+` (qld) lines. Lines that
legitimately differ can be filtered with `diff.ignore` in the fixture.

## Argv corpus

Each `corpus/*.txt` holds one captured command line: `#` comments, `#! key =
value` expectations, then one argument per line (argv[0] excluded).
Expectation keys: `output`, `kind` (an `OutputKind` variant name), `soname`,
`entry`, `dynamic_linker`, `gc_sections`, `export_dynamic`, `bind_now`,
`inputs` (count).

The files were captured with `corpus/capture.sh`, which builds small
programs with a logging `ld` shim (reached through gcc's `-B`, clang's
`--ld-path` and a rustc linker wrapper) so that the argv is exactly what the
linker receives, then `corpus/normalize.py` rewrites machine-specific paths.
To add a command line from another build, log the linker argv the same way,
normalize paths, and add the expectations you are sure of.
