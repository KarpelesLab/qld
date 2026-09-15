# Real-project builds

Build scripts and notes for the M2, M3 and M6 exit criteria (`ROADMAP.md`):
large projects built and tested with qld as the system linker. They download
release tarballs, need a host toolchain and network access, and take
minutes to an hour, so they are for manual and nightly runs only;
`cargo test` never runs them.

Every script takes the qld binary and a scratch directory:

```sh
cargo build --release
cp target/release/qld ~/.cache/qld-projects/qld   # a copy a rebuild cannot change mid-build
tests/projects/coreutils.sh ~/.cache/qld-projects/qld ~/.cache/qld-projects
```

| Script | Project | Notes |
| --- | --- | --- |
| `coreutils.sh` | GNU coreutils | [coreutils.md](coreutils.md) |
| `curl.sh` | curl, shared libcurl | [curl.md](curl.md) |
| `openssl.sh` | OpenSSL 3.x | [openssl.md](openssl.md) |
| `python.sh` | CPython, `--enable-shared` and default | [python.md](python.md) |
| `llvm.sh` | LLVM, clang, lld; static and `BUILD_SHARED_LIBS` | [llvm.md](llvm.md) |
| `rust.sh` | rustc stage 1 bootstrap, `tests/ui`, `library/std` | [rust.md](rust.md) |
| `musl.sh` | musl `ld.so` loading qld's shared objects | [musl.md](musl.md) |
| `lto.sh` | M6: zlib, lua, curl, OpenSSL, coreutils or Python with `gcc -flto`, `clang -flto` or `-flto=thin` | [lto.md](lto.md) |
| `rust-lto.sh` | M6: `-C linker-plugin-lto` (Rust and C, and qld's unit tests) through LLVMgold | [lto.md](lto.md) |
| `kernel.sh` | M3: the Linux kernel, built with a shim, compared with GNU ld and booted in QEMU | [kernel.md](kernel.md) |
| `bench-capture.sh` | M5: captures the benchmark corpus (clang, clang with debug info, `libclang-cpp.so`, `vmlinux`, qld's debug binary) for `benches/run.py` | [bench.md](bench.md) |
| `baremetal.sh` | M3: a firmware image from a linker script, compared with GNU ld down to the raw bytes | [baremetal.md](baremetal.md) |

`coreutils.sh`, `curl.sh`, `openssl.sh` and `python.sh` take a third
argument, `gnu`, to build the same tree with the system GNU ld for
comparison. `kernel.sh` and `baremetal.sh` compare with GNU ld themselves,
and need it installed (`GNU_LD=` to name it); `kernel.sh` also wants
`qemu-system-x86_64` and `cpio` for the boot, and skips it without them.

## How qld is put in the build

`common.sh` writes wrappers into `$SCRATCH/bin`:

- `ld` runs qld; `ld.bfd`, `ld.gold`, `ld.lld`, `ld.mold` and `ld.qld` are
  links to it, so neither gcc's `-B` lookup nor clang's `-fuse-ld=` can fall
  back to another linker;
- `qcc`, `qc++` (`gcc -B$SCRATCH/bin`) and `qclang`, `qclang++`
  (`clang --ld-path=$SCRATCH/bin/ld`) are the compilers the scripts pass to
  configure, CMake and `x.py`;
- every script checks `readelf -p .comment` for `Linker: qld` on the
  executables and shared objects it built (`check_linked_by_qld`).

`kernel.sh` is the exception: the kernel is built with `LD=` pointing at its
own shim (`$SCRATCH/bin/ld-kernel`), which routes the 32-bit links, the
version probes and `ld -r` to GNU ld and everything else to qld, and logs
which linker ran each call. See [kernel.md](kernel.md).

The `ld` wrapper also:

- appends each link's working directory and argv to `$QLD_LINK_LOG`,
  copying response files, so a failing link can be replayed;
- with `QLD_LINK_COMPARE=DIR`, links each output again with GNU ld from the
  same inputs (`ldcompare.py`) and records both times, both sizes and the
  `elfdiff.py` differences in `DIR`;
- works around qld's `--help` not printing a `supported targets:` line,
  without which libtool silently builds static libraries only (see
  [curl.md](curl.md)).

## Comparison tools

| Tool | What it does |
| --- | --- |
| `elfdiff.py REF CAND [--relocs] [--sections]` | Dynamic symbols (name, version, type, binding, visibility, defined or not) and `DT_*` entries of two ELF files, normalized as in `tests/differential.rs` |
| `elfdiff-tree.sh GNU_TREE QLD_TREE` | `elfdiff.py` over every executable and shared object two build trees share |
| `linktime.py LOG OUTPUT QLD [--keep DIR]` | Replays a logged link with qld and GNU ld: best-of-N wall time and output size |
| `output-backing.py QLD --outdir DIR WORKLOAD...` | Times each output backing (`QLD_OUTPUT_BACKING`) on logged links or a synthetic debug-info link, per file system and thread count, and checks the outputs are identical; results in [output-backing.md](output-backing.md) |
| `ldcompare.py` | The wrapper's `QLD_LINK_COMPARE` mode, for inputs that do not outlive the build |
| `gcdiff.py LOG OUTPUT QLD` | Sections only one of the linkers removed with `--gc-sections` |
| `relocdiff.py REF CAND [TYPE]` | Dynamic relocations named by the symbol they patch |

## Results

See each project's notes. Summary of the last full run (qld at the head of
`W16`):

| Project | Version | Result |
| --- | --- | --- |
| coreutils | 9.11 | `make check`: same results as with GNU ld (1 environment failure in both) |
| curl | 8.22.0 | `make test`: 1629/1629 OK |
| OpenSSL | 3.6.4 | `make test`: PASS (4555 tests in 355 files) |
| Python | 3.14.7 | `python -m test`: 467 OK, 3 environment failures, same as GNU ld; shared and static |
| LLVM | 23.1.1 | `check-llvm`, `check-clang`, `check-lld`: all pass (static); 1 path-name failure (shared) |
| Rust | 1.98.1 | stage 1 builds; `tests/ui` 21288 passed, `library/std` passed |
| musl | 1.2.5 | shared libraries, PIE, non-PIE and zlib's tests run under musl's `ld.so` |
| LTO (M6, W18) | as above | zlib, lua, curl, OpenSSL: tests pass with gcc, clang and ThinLTO; coreutils and Python: same results as GNU ld; see [lto.md](lto.md) |
| Linux kernel (M3) | 7.2.5 | defconfig `bzImage` links with qld and boots in QEMU; 43 allocated sections and all 238897 symbols have GNU ld's addresses; see [kernel.md](kernel.md) |
| Bare metal (M3) | — | MEMORY/`AT>`/PHDRS images match GNU ld's addresses, and the binary, ihex and srec files are byte-identical; see [baremetal.md](baremetal.md) |

## Performance

Link time against GNU ld 2.46.1 (`ld.bfd`), replaying the logged links with
`linktime.py` (best of 5; qld without `--threads`; 64-core host shared with
other jobs; outputs on btrfs). Sizes in bytes.

| Output | qld | GNU ld | Speedup | qld size | GNU ld size |
| --- | --- | --- | --- | --- | --- |
| clang (static LLVM libs) | 0.26 s | 2.00 s | 7.7x | 137677264 | 137544040 |
| lld | 0.16 s | 1.08 s | 6.9x | 75497536 | 75492960 |
| opt | 0.16 s | 0.97 s | 6.2x | 67297832 | 67242816 |
| libclang-cpp.so | 0.39 s | 2.20 s | 5.6x | 134891496 | 134746368 |
| libLTO.so | 0.16 s | 0.95 s | 5.9x | 57830080 | 57790832 |
| libclangSema.so (shared build) | 0.06 s | 0.28 s | 4.5x | 17148576 | 17148560 |
| libLLVMCodeGen.so (shared build) | 0.04 s | 0.18 s | 4.6x | 9046128 | 9052008 |
| librustc_driver.so (timed during the build) | 0.38 s | 3.03 s | 8.0x | 143914408 | 141226872 |
| libpython3.14.so | 0.07 s | 0.22 s | 3.2x | 34423480 | 34366440 |
| python (static libpython) | 0.07 s | 0.20 s | 2.8x | 35723832 | 35662720 |
| libcrypto.so.3 | 0.03 s | 0.10 s | 3.0x | 7471008 | 7425264 |
| libssl.so.3 | 0.01 s | 0.03 s | 2.0x | 1317744 | 1311328 |

qld's outputs are up to 2% larger. libLLVM was not built as one shared
library (`LLVM_BUILD_LLVM_DYLIB` is off in both configurations).

`libcrypto.so.3` (849 objects) first took 82 ms, barely faster than GNU ld:
the first parallel stage started rayon's global pool of 64 threads, and
mapping 849 files on 64 threads took 50 ms (`QLD_TIMING=1`). Inputs are now
mapped in a pool of at most 16 threads, and the rest of the link gets one
thread per 4 MiB of input, at most 16 (commit `elf: map inputs in a small
pool and size link pools from 4 MiB per thread`).
