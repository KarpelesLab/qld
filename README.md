# qld

**qld** is a fast, parallel linker (link editor) written in pure Rust. It is
designed as a drop-in replacement for GNU ld, gold, lld and mold: it accepts
their command lines, so compiler drivers (`gcc`, `clang`, `rustc`) and build
systems can use it with no other changes. It is also a Rust library, so tools
can link programs in-process.

> **Status: pre-alpha.** qld links **x86-64 Linux ELF**: static and dynamic
> executables, PIE and static PIE, shared libraries and relocatable (`-r`)
> output, with symbol versioning, RELRO, `DT_RELR`, `--gc-sections`, `--icf`,
> compressed debug sections and `--build-id`. The M1 and M2 feature lists are
> implemented; M2's exit criteria (coreutils, openssl, Python, rustc, LLVM)
> have not been run yet. So far it builds and passes the test suites of zlib,
> lua, bzip2, jansson, gzip, grep, expat and cmark, and qld's own test suite
> linked by qld. Other architectures and formats are not supported yet. Dynamic linking
> (PIE, shared libraries) is next (M2); everything else below describes where
> qld is going, not what it does today. See [ROADMAP.md](ROADMAP.md).

## Goals

- **Command-line compatibility.** Accepts the GNU ld / gold / lld argv,
  including positional state flags (`--whole-archive`, `--as-needed`,
  `-Bstatic`), response files, `-z` keywords, linker scripts and the LTO plugin
  interface. Mach-O links use an `ld64`-compatible flavor. PE links use the
  MinGW GNU ld flavor.
- **Speed.** Parallel at every stage: input parsing, symbol resolution, garbage
  collection, string merging, relocation and output writing. Inputs are
  memory-mapped and never copied. Output is written in place through a mapped
  file.
- **Deterministic output.** The output is byte-for-byte identical no matter how
  many threads are used or how they are scheduled.
- **Many output formats.**
  - ELF: executables, PIE, shared objects and relocatable (`-r`) output, on
    multiple architectures
  - PE32 / PE32+: EXE and DLL, MinGW style
  - Mach-O: executables, dylibs and bundles, including universal (fat) binaries
  - Raw binary: `--oformat binary` / `OUTPUT_FORMAT(binary)` for firmware and
    bare-metal work
  - DWARF debug information handled correctly in all of the above: relocation,
    compression, and index generation
- **Link-time optimizations.** Section garbage collection (tree shaking),
  identical code folding, string tail merging, relocation relaxation, compact
  relative relocations (`DT_RELR`), and LTO through the GCC/LLVM linker plugin
  API.
- **Intelligent library symbol matching.** Archive resolution does not depend
  on input order. When a symbol is undefined, qld suggests the library that
  defines it (`did you forget -lm?`), matches symbol versions, and names the
  closest mangled or unmangled candidates.
- **Usable as a crate.** A stable, embeddable API with no global state and no
  `process::exit`. Inputs and outputs can live in memory, and diagnostics are
  structured.
- **Pure Rust.** Building qld needs no C toolchain. The only foreign code is
  what the user's compiler supplies: LTO plugins, loaded at run time and only
  when requested.

## Documentation

| Document | Contents |
| --- | --- |
| [ROADMAP.md](ROADMAP.md) | Milestones, scope and exit criteria |
| [docs/workstreams.md](docs/workstreams.md) | Parallel work areas, file ownership, how to launch work |
| [docs/architecture.md](docs/architecture.md) | Link pipeline, module layout, data model, parallelism |
| [docs/compatibility.md](docs/compatibility.md) | Command-line flavors, option parsing rules, behavioral differences |
| [docs/formats.md](docs/formats.md) | Input/output formats and architectures, per-format scope |
| [docs/optimizations.md](docs/optimizations.md) | GC, ICF, merging, relaxation, LTO, symbol matching |
| [docs/library-api.md](docs/library-api.md) | Planned Rust crate API |
| [docs/testing.md](docs/testing.md) | Test strategy, differential testing, benchmarks |
| [docs/development.md](docs/development.md) | Toolchain, coding rules, dependency and `unsafe` policy |

## Usage

Today (x86-64 Linux ELF):

```sh
# gcc: -B points at a directory whose `ld` is a symlink to qld
gcc -B/opt/qld/bin hello.c -o hello

# clang
clang --ld-path=/opt/qld/bin/ld hello.c -o hello

# Rust (recent rustc uses its bundled rust-lld by default; turn that off)
RUSTFLAGS="-C linker=gcc -C linker-features=-lld -C link-arg=-B/opt/qld/bin" \
  cargo build --release
```

## Planned usage

```sh
# Directly, like GNU ld
qld -o hello crt1.o crti.o hello.o -lc crtn.o

# Through clang
clang -fuse-ld=qld hello.c            # finds `ld.qld` in PATH
clang --ld-path=/path/to/qld hello.c

# Through gcc: put a directory containing an `ld` symlink to qld first on the -B path
gcc -B/opt/qld/bin hello.c

# Through rustc / cargo (.cargo/config.toml). Recent rustc links with its
# bundled rust-lld by default on x86-64 Linux; turn that off first.
# [target.x86_64-unknown-linux-gnu]
# linker = "clang"
# rustflags = ["-C", "linker-features=-lld", "-C", "link-arg=--ld-path=/path/to/qld"]
```

## Requirements

- Rust **1.89** or newer (MSRV), edition 2024

## License

MIT. See [LICENSE](LICENSE).
