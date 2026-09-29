# qld

[![CI](https://github.com/KarpelesLab/qld/actions/workflows/ci.yml/badge.svg)](https://github.com/KarpelesLab/qld/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/qld.svg)](https://crates.io/crates/qld)
[![docs.rs](https://img.shields.io/docsrs/qld)](https://docs.rs/qld)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

**qld** is a fast, parallel linker (link editor) written in pure Rust. It is
designed as a drop-in replacement for GNU ld, gold, lld and mold: it accepts
their command lines, so compiler drivers (`gcc`, `clang`, `rustc`) and build
systems can use it with no other changes. It is also a Rust library, so tools
can link programs in-process.

> **Status: alpha.** Released as [0.1.0](https://crates.io/crates/qld).
> The feature surface below is broad and checked in CI on every push, but
> qld has had little use outside this repository, so expect rough edges and
> report them.
>
> **Formats:** ELF, PE/COFF (MinGW), Mach-O (`ld64` flavor), AmigaOS Hunk,
> and raw `binary`/`ihex`/`srec`.
>
> **ELF architectures:** x86-64, i386, x32, AArch64, 32-bit Arm, RISC-V 64
> and 32, PowerPC64 little-endian and big-endian (the latter static only),
> LoongArch64, s390x and m68k. Both word sizes and both byte orders.
>
> **Features:** static and dynamic executables, PIE and static PIE, shared
> libraries, relocatable (`-r`) and `--emit-relocs` output for every class,
> symbol versioning, RELRO, `DT_RELR`, `--gc-sections`, `--icf`,
> `--gdb-index`, `--debug-names`, `--separate-debug-file`, section ordering,
> compressed debug sections, `--build-id`, linker-script layout (`-T`,
> `MEMORY`, `PHDRS`), and LTO through the compiler's plugin (`gcc -flto`,
> `clang -flto`/`-flto=thin`) or `libLTO` on macOS.
>
> **Verified in CI on every push:** each architecture's output is compared
> with GNU ld, lld or vlink, and the programs are run natively or under
> qemu. A Linux kernel linked by qld boots in QEMU. Used as the system
> linker it builds and passes the test suites of coreutils, curl, OpenSSL,
> Python, LLVM/clang/lld and the Rust compiler; on macOS, of zlib, Lua,
> SQLite and {fmt}. Output is byte-identical whatever the thread count.
>
> Linking clang and `libclang-cpp.so` on a 32-core machine (minimum wall
> time of interleaved runs, each linker at its own default thread count):
>
> | Link | qld | GNU ld | lld | mold | wild |
> | --- | --- | --- | --- | --- | --- |
> | clang | 0.13 s | 2.17 s | 0.35 s | 0.19 s | 0.10 s |
> | libclang-cpp.so | 0.09 s | 1.43 s | 0.22 s | 0.13 s | 0.07 s |
>
> qld uses the least CPU of the five and less memory than lld or mold.
> [wild](https://github.com/davidlattimore/wild) is still ahead on wall
> time; the gap and what is left to close it are in
> [tests/projects/bench.md](tests/projects/bench.md).

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
# gcc: -B points at a directory whose `ld` is qld (packages install
# <prefix>/libexec/qld/ld; gcc has no -fuse-ld=qld)
gcc -B/usr/libexec/qld hello.c -o hello

# clang: finds ld.qld (ld64.qld on macOS) in PATH
clang -fuse-ld=qld hello.c -o hello

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
