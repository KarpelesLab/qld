# Benchmarks (W24, roadmap M5)

qld against GNU ld, lld, mold and wild on real links, replayed from build
trees that already exist. The drivers are in `benches/`; this file records
the method, the corpus and the results.

## Standing, stated plainly

Measured on 2026-09-15 on a shared 32-core / 64-thread machine, at a load
average of 1–20 (per row in the tables below). "Default" is each linker's
own thread count (qld: one thread per 4 MiB of input, at most 16). Minimum
wall times:

- **wild is faster than qld on every benchmark it can link, at every
  thread count.** At default threads: clang 102 ms against qld's 213,
  libclang-cpp 77 against 140, qld's own debug binary 136 against 217,
  clang with debug info 660 against 836.
- **mold is faster than qld at 8 threads on every benchmark** (clang 149
  against 200 ms, clang-debug 598 against 921), and at default threads on
  clang (184 against 213) and libclang-cpp (125 against 140). qld is ahead
  of mold on clang-debug at default and 64 threads (836 against 1113, 896
  against 1288), and level with it on qld's debug binary (217 against 214
  at default, 236 against 238 at 64).
- **qld is faster than lld** at default and 8 threads on every benchmark
  except qld's debug binary at 8 threads (233 against 187 ms) and the small
  link (10 against 9 ms), including `vmlinux` (147 against 180 ms), which
  neither mold nor wild can link. On one thread, lld and mold are faster
  than qld on every large link (clang: lld 429, mold 504, qld 644 ms).
- **Peak RSS is below lld's on all six benchmarks** (clang-debug: qld
  3.8 GiB, lld 4.6, wild 4.4, mold 5.2).
- **Output is byte-identical across 1, 2, 8 and 64 threads** on all six
  benchmarks, and identical to the output of the tree W24 started from.

So the M5 exit criterion (wall time at or below mold's and wild's at 8 and
64 cores) is **not met**. The W24 optimizations took 12–25% off qld's
default-thread link times (clang 256 → 213 ms, clang-debug 1113 → 836 ms,
libclang-cpp 160 → 140 ms, `vmlinux` 193 → 147 ms, qld's debug binary
256 → 217 ms) and 14–24% off its single-threaded ones.

## Setup

| | |
| --- | --- |
| Machine | AMD Ryzen Threadripper 9970X, 32 cores / 64 threads, 125 GiB, Linux 6.18.41 |
| File system | btrfs (97% full); inputs in the page cache; outputs on the same btrfs |
| Load | shared machine; the load average is recorded with every run |
| GNU ld | 2.46.1 (Gentoo) |
| lld | 23.1.1, from the static LLVM tree (`llvm.sh`) |
| mold | 2.42.1, built from the release tarball with GCC 15.3, `-DCMAKE_BUILD_TYPE=Release` |
| wild | 0.10.0, `cargo install --locked wild-linker` |
| qld | release build of this branch (`cargo build --release`) |

## Corpus

| Benchmark | Link | Inputs | Output | Smoke check |
| --- | --- | --- | --- | --- |
| `clang` | `bin/clang-23` of the static LLVM 23.1.1 tree (`llvm.sh`), release; PIE, `--export-dynamic` | 5 objects and the C runtime's, 122 archive arguments (LLVM and clang libraries, some repeated), libstdc++ and libc | 131 MiB | `--version`, compile a C file |
| `libclang-cpp` | `lib/libclang-cpp.so.23.1` of the `BUILD_SHARED_LIBS` tree; `-shared`, `--gc-sections` | 1,014 objects, 57 shared libraries | 78 MiB | `dlopen` with `RTLD_NOW` |
| `clang-debug` | `bin/clang-23` of a `RelWithDebInfo` clang tree (X86 target, clang only, built with clang 22 and lld; see below) | as `clang`, with DWARF 5 | 1.2 GiB | as `clang` |
| `rust-qld-debug` | qld's own debug binary (`cargo build --bin qld`, rustc 1.98); PIE, `--gc-sections`, `--build-id` | 43 objects, 32 rlibs | 170 MiB | `--version` |
| `vmlinux` | `vmlinux.unstripped` of the kernel 7.2.5 tree (`kernel.sh`): `vmlinux.lds`, `--emit-relocs`, `--build-id=sha1` | `vmlinux.o` and 3 objects | 99 MiB | `start_kernel` defined, PT_LOAD headers |
| `small-count` | LLVM's `bin/count`; PIE, `--gc-sections` | 1 object and the C runtime's, 2 archives, libc | 15 KiB | counts a line |

`librustc_driver.so` is not in the corpus: no rustc tree was built on the
machine, and building one was out of the time budget.

The debug-info tree was configured with

```sh
cmake -G Ninja llvm-project-23.1.1.src/llvm -DCMAKE_BUILD_TYPE=RelWithDebInfo \
  -DCMAKE_C_COMPILER=clang -DCMAKE_CXX_COMPILER=clang++ -DLLVM_USE_LINKER=/path/to/ld.lld \
  -DLLVM_ENABLE_PROJECTS=clang -DLLVM_TARGETS_TO_BUILD=X86 -DLLVM_ENABLE_ASSERTIONS=OFF \
  -DLLVM_INCLUDE_BENCHMARKS=OFF -DLLVM_INCLUDE_TESTS=OFF -DCLANG_INCLUDE_TESTS=OFF
ninja clang
```

(about 6 minutes on this machine).

## Method

1. **Capture** (`benches/capture.py`, driven by `tests/projects/bench-capture.sh`):
   the last command of `ninja -t commands TARGET` runs with the linker
   replaced by a shim that records the working directory and the linker
   argv (response files expanded) into `SPECS/NAME/link.json`, and links
   nothing. For cargo, the shim is put in with `-C link-arg=-B`, every link
   also runs GNU ld so the build continues, and rustc's temporary inputs
   (codegen-unit objects, `symbols.o`) are copied next to the spec.
2. **Replay** (`benches/run.py SPECS`): each link runs in its original
   directory with only `-o` changed, with every linker at default threads
   and `--threads=1`, `8` and `64` (GNU ld: default only). Configurations are
   interleaved run by run (run 1 of every configuration, then run 2...), 5
   runs each (3 for clang-debug at one thread). Before each run the old
   output is deleted and the file system synced, outside the timing.
   Options that only affect diagnostics or checks are dropped for the
   linkers that reject them (`--color-diagnostics`; for lld, mold and wild
   also `--no-warn-rwx-segments`; for mold and wild `--discard-none` and
   `--orphan-handling=error`).
3. **Measures**: wall time (min and median), user+system CPU time and peak
   RSS from `wait4`, output size, and the 1-minute load average before each
   run. mold and wild fork by default and return once the output is written,
   leaving a child to clean up; their wall time is measured that way, but
   CPU and RSS come from an extra `--no-fork` run, since `wait4` does not
   see the child.
4. **Smoke check**: every configuration's output must pass the benchmark's
   check; one that fails counts as broken, not as a time.
5. **Determinism**: `run.py --determinism QLD --hashes FILE` links every
   benchmark with qld at 1, 2, 8 and 64 threads and compares the outputs
   with each other and with the hashes saved before the optimizations.
6. **Report**: `benches/report.py RESULTS.json...` prints the tables.

```sh
cargo build --release
tests/projects/bench-capture.sh ~/.cache/qld-bench/specs
benches/run.py ~/.cache/qld-bench/specs --json results.json
benches/run.py ~/.cache/qld-bench/specs --determinism target/release/qld --hashes hashes.json
benches/report.py results.json
```

mold and wild cannot link `vmlinux`: mold rejects a command of
`vmlinux.lds`, and wild does not support `--emit-relocs`.

## Results

"qld before" is the tree W24 started from (commit a85c629), "qld after"
this branch; both ran in the same interleaved runs as the other linkers.
Times are wall-clock; CPU is user+system.

### clang

| linker | threads | wall min | wall median | CPU | peak RSS | output | load |
| --- | --- | --- | --- | --- | --- | --- | --- |
| gnu | default | 2028 ms | 2100 ms | 2.10 s | 622 MiB | 131.2 MiB | 1–3 |
| lld | default | 317 ms | 354 ms | 2.32 s | 739 MiB | 131.3 MiB | 1–2 |
| lld | 1 | 429 ms | 434 ms | 0.43 s | 732 MiB | 131.3 MiB | 1–2 |
| lld | 8 | 291 ms | 340 ms | 1.22 s | 740 MiB | 131.3 MiB | 1–2 |
| lld | 64 | 371 ms | 377 ms | 5.07 s | 726 MiB | 131.3 MiB | 1–2 |
| mold | default | 184 ms | 191 ms | 5.38 s | 842 MiB | 131.8 MiB | 1–2 |
| mold | 1 | 504 ms | 505 ms | 0.50 s | 670 MiB | 131.8 MiB | 1–2 |
| mold | 8 | 149 ms | 170 ms | 0.94 s | 722 MiB | 131.8 MiB | 1–2 |
| mold | 64 | 210 ms | 222 ms | 11.40 s | 986 MiB | 131.8 MiB | 1–2 |
| wild | default | 102 ms | 104 ms | 1.65 s | 497 MiB | 131.0 MiB | 1–2 |
| wild | 1 | 465 ms | 467 ms | 0.48 s | 508 MiB | 131.0 MiB | 1–2 |
| wild | 8 | 118 ms | 126 ms | 0.55 s | 506 MiB | 131.0 MiB | 1–2 |
| wild | 64 | 100 ms | 103 ms | 1.64 s | 501 MiB | 131.0 MiB | 1–2 |
| qld before | default | 256 ms | 265 ms | 1.51 s | 618 MiB | 131.3 MiB | 1–2 |
| qld before | 1 | 755 ms | 759 ms | 0.76 s | 571 MiB | 131.3 MiB | 2–3 |
| qld before | 8 | 264 ms | 269 ms | 1.04 s | 600 MiB | 131.3 MiB | 1–3 |
| qld before | 64 | 299 ms | 304 ms | 4.66 s | 699 MiB | 131.3 MiB | 1–3 |
| qld after | default | 213 ms | 217 ms | 1.56 s | 625 MiB | 131.3 MiB | 1–3 |
| qld after | 1 | 644 ms | 650 ms | 0.65 s | 584 MiB | 131.3 MiB | 1–3 |
| qld after | 8 | 200 ms | 211 ms | 0.93 s | 602 MiB | 131.3 MiB | 1–3 |
| qld after | 64 | 247 ms | 254 ms | 4.78 s | 712 MiB | 131.3 MiB | 1–3 |

### libclang-cpp

| linker | threads | wall min | wall median | CPU | peak RSS | output | load |
| --- | --- | --- | --- | --- | --- | --- | --- |
| gnu | default | 1366 ms | 1387 ms | 1.38 s | 361 MiB | 77.5 MiB | 2–3 |
| lld | default | 223 ms | 232 ms | 1.34 s | 398 MiB | 77.4 MiB | 2–3 |
| lld | 1 | 290 ms | 302 ms | 0.30 s | 400 MiB | 77.4 MiB | 2–3 |
| lld | 8 | 188 ms | 231 ms | 0.80 s | 406 MiB | 77.4 MiB | 2–3 |
| lld | 64 | 242 ms | 247 ms | 2.49 s | 387 MiB | 77.4 MiB | 2–3 |
| mold | default | 125 ms | 129 ms | 3.65 s | 573 MiB | 78.0 MiB | 2–3 |
| mold | 1 | 324 ms | 326 ms | 0.32 s | 361 MiB | 78.0 MiB | 2–5 |
| mold | 8 | 102 ms | 115 ms | 0.57 s | 412 MiB | 78.0 MiB | 2–5 |
| mold | 64 | 161 ms | 165 ms | 8.39 s | 746 MiB | 78.0 MiB | 2–5 |
| wild | default | 77 ms | 78 ms | 1.36 s | 291 MiB | 77.3 MiB | 2–5 |
| wild | 1 | 286 ms | 289 ms | 0.30 s | 307 MiB | 77.3 MiB | 2–5 |
| wild | 8 | 73 ms | 78 ms | 0.37 s | 308 MiB | 77.3 MiB | 2–5 |
| wild | 64 | 76 ms | 102 ms | 1.33 s | 292 MiB | 77.3 MiB | 2–5 |
| qld before | default | 160 ms | 167 ms | 1.09 s | 341 MiB | 77.5 MiB | 2–5 |
| qld before | 1 | 494 ms | 498 ms | 0.50 s | 304 MiB | 77.5 MiB | 2–5 |
| qld before | 8 | 163 ms | 170 ms | 0.69 s | 322 MiB | 77.5 MiB | 2–5 |
| qld before | 64 | 219 ms | 228 ms | 3.03 s | 384 MiB | 77.5 MiB | 2–5 |
| qld after | default | 140 ms | 146 ms | 1.01 s | 348 MiB | 77.5 MiB | 2–5 |
| qld after | 1 | 425 ms | 429 ms | 0.43 s | 305 MiB | 77.5 MiB | 2–5 |
| qld after | 8 | 136 ms | 142 ms | 0.63 s | 330 MiB | 77.5 MiB | 2–5 |
| qld after | 64 | 207 ms | 223 ms | 3.24 s | 383 MiB | 77.5 MiB | 2–5 |

### small-count

| linker | threads | wall min | wall median | CPU | peak RSS | output | load |
| --- | --- | --- | --- | --- | --- | --- | --- |
| gnu | default | 27 ms | 30 ms | 0.03 s | 21 MiB | 0.0 MiB | 6–6 |
| lld | default | 9 ms | 10 ms | 0.02 s | 36 MiB | 0.0 MiB | 6–6 |
| lld | 1 | 9 ms | 10 ms | 0.01 s | 39 MiB | 0.0 MiB | 6–6 |
| lld | 8 | 8 ms | 9 ms | 0.01 s | 37 MiB | 0.0 MiB | 6–6 |
| lld | 64 | 10 ms | 10 ms | 0.03 s | 38 MiB | 0.0 MiB | 6–6 |
| mold | default | 12 ms | 12 ms | 0.26 s | 105 MiB | 0.0 MiB | 6–6 |
| mold | 1 | 10 ms | 11 ms | 0.01 s | 37 MiB | 0.0 MiB | 6–6 |
| mold | 8 | 10 ms | 11 ms | 0.05 s | 52 MiB | 0.0 MiB | 6–6 |
| mold | 64 | 15 ms | 15 ms | 0.43 s | 148 MiB | 0.0 MiB | 6–6 |
| wild | default | 9 ms | 9 ms | 0.20 s | 21 MiB | 0.0 MiB | 6–6 |
| wild | 1 | 5 ms | 5 ms | 0.01 s | 21 MiB | 0.0 MiB | 6–6 |
| wild | 8 | 5 ms | 5 ms | 0.02 s | 21 MiB | 0.0 MiB | 6–6 |
| wild | 64 | 8 ms | 9 ms | 0.20 s | 21 MiB | 0.0 MiB | 6–6 |
| qld before | default | 10 ms | 10 ms | 0.03 s | 21 MiB | 0.0 MiB | 6–6 |
| qld before | 1 | 8 ms | 9 ms | 0.01 s | 23 MiB | 0.0 MiB | 6–6 |
| qld before | 8 | 9 ms | 9 ms | 0.03 s | 22 MiB | 0.0 MiB | 6–6 |
| qld before | 64 | 16 ms | 17 ms | 0.50 s | 21 MiB | 0.0 MiB | 6–6 |
| qld after | default | 10 ms | 10 ms | 0.03 s | 21 MiB | 0.0 MiB | 6–6 |
| qld after | 1 | 8 ms | 9 ms | 0.01 s | 24 MiB | 0.0 MiB | 6–6 |
| qld after | 8 | 8 ms | 9 ms | 0.03 s | 21 MiB | 0.0 MiB | 6–6 |
| qld after | 64 | 16 ms | 16 ms | 0.50 s | 21 MiB | 0.0 MiB | 6–6 |

### rust-qld-debug

| linker | threads | wall min | wall median | CPU | peak RSS | output | load |
| --- | --- | --- | --- | --- | --- | --- | --- |
| gnu | default | 1390 ms | 1391 ms | 1.39 s | 468 MiB | 162.3 MiB | 4–6 |
| lld | default | 240 ms | 257 ms | 2.32 s | 613 MiB | 170.6 MiB | 4–6 |
| lld | 1 | 386 ms | 396 ms | 0.40 s | 616 MiB | 170.6 MiB | 4–6 |
| lld | 8 | 187 ms | 208 ms | 0.91 s | 614 MiB | 170.6 MiB | 4–6 |
| lld | 64 | 276 ms | 282 ms | 7.05 s | 617 MiB | 170.6 MiB | 4–6 |
| mold | default | 214 ms | 217 ms | 5.83 s | 793 MiB | 189.7 MiB | 4–6 |
| mold | 1 | 395 ms | 409 ms | 0.41 s | 622 MiB | 189.7 MiB | 5–7 |
| mold | 8 | 125 ms | 183 ms | 1.30 s | 685 MiB | 189.7 MiB | 5–7 |
| mold | 64 | 238 ms | 251 ms | 12.59 s | 917 MiB | 189.7 MiB | 5–7 |
| wild | default | 136 ms | 139 ms | 1.94 s | 581 MiB | 169.6 MiB | 5–7 |
| wild | 1 | 395 ms | 405 ms | 0.42 s | 596 MiB | 169.6 MiB | 5–7 |
| wild | 8 | 135 ms | 137 ms | 0.54 s | 595 MiB | 169.6 MiB | 5–7 |
| wild | 64 | 133 ms | 138 ms | 1.96 s | 583 MiB | 169.6 MiB | 5–7 |
| qld before | default | 256 ms | 261 ms | 1.68 s | 584 MiB | 169.6 MiB | 5–7 |
| qld before | 1 | 982 ms | 1026 ms | 1.02 s | 524 MiB | 169.6 MiB | 5–6 |
| qld before | 8 | 273 ms | 282 ms | 1.36 s | 574 MiB | 169.6 MiB | 5–6 |
| qld before | 64 | 261 ms | 266 ms | 4.21 s | 621 MiB | 169.6 MiB | 5–6 |
| qld after | default | 217 ms | 221 ms | 1.44 s | 583 MiB | 169.6 MiB | 5–6 |
| qld after | 1 | 838 ms | 851 ms | 0.85 s | 521 MiB | 169.6 MiB | 5–6 |
| qld after | 8 | 233 ms | 238 ms | 1.12 s | 567 MiB | 169.6 MiB | 4–6 |
| qld after | 64 | 236 ms | 239 ms | 3.69 s | 617 MiB | 169.6 MiB | 4–6 |

### vmlinux

| linker | threads | wall min | wall median | CPU | peak RSS | output | load |
| --- | --- | --- | --- | --- | --- | --- | --- |
| gnu | default | 613 ms | 631 ms | 0.63 s | 197 MiB | 96.9 MiB | 4–5 |
| lld | default | 180 ms | 205 ms | 0.76 s | 326 MiB | 98.6 MiB | 4–5 |
| lld | 1 | 230 ms | 232 ms | 0.23 s | 331 MiB | 98.6 MiB | 4–5 |
| lld | 8 | 176 ms | 194 ms | 0.54 s | 328 MiB | 98.6 MiB | 4–5 |
| lld | 64 | 191 ms | 200 ms | 1.36 s | 321 MiB | 98.6 MiB | 4–5 |
| mold | default | fails: link failed:                                               ^ unknown l | | | | | |
| mold | 1 | fails: link failed:                                               ^ unknown l | | | | | |
| mold | 8 | fails: link failed:                                               ^ unknown l | | | | | |
| mold | 64 | fails: link failed:                                               ^ unknown l | | | | | |
| wild | default | fails: link failed: wild: error: unrecognized option(s): --emit-relocs | | | | | |
| wild | 1 | fails: link failed: wild: error: unrecognized option(s): --emit-relocs | | | | | |
| wild | 8 | fails: link failed: wild: error: unrecognized option(s): --emit-relocs | | | | | |
| wild | 64 | fails: link failed: wild: error: unrecognized option(s): --emit-relocs | | | | | |
| qld before | default | 193 ms | 202 ms | 1.01 s | 220 MiB | 98.6 MiB | 4–5 |
| qld before | 1 | 586 ms | 587 ms | 0.59 s | 145 MiB | 98.6 MiB | 4–5 |
| qld before | 8 | 197 ms | 204 ms | 0.74 s | 207 MiB | 98.6 MiB | 4–5 |
| qld before | 64 | 206 ms | 214 ms | 2.30 s | 218 MiB | 98.6 MiB | 4–5 |
| qld after | default | 147 ms | 148 ms | 0.72 s | 216 MiB | 98.6 MiB | 4–5 |
| qld after | 1 | 440 ms | 446 ms | 0.44 s | 147 MiB | 98.6 MiB | 4–5 |
| qld after | 8 | 150 ms | 152 ms | 0.56 s | 206 MiB | 98.6 MiB | 4–5 |
| qld after | 64 | 163 ms | 165 ms | 1.48 s | 223 MiB | 98.6 MiB | 3–5 |

### clang-debug

| linker | threads | wall min | wall median | CPU | peak RSS | output | load |
| --- | --- | --- | --- | --- | --- | --- | --- |
| gnu | default | 5953 ms | 6077 ms | 6.07 s | 1505 MiB | 1212.1 MiB | 3–19 |
| lld | default | 1320 ms | 1331 ms | 15.77 s | 4621 MiB | 1213.7 MiB | 3–17 |
| lld | 1 | 2658 ms | 2799 ms | 2.79 s | 4611 MiB | 1213.7 MiB | 5–9 |
| lld | 8 | 1061 ms | 1150 ms | 6.52 s | 4619 MiB | 1213.7 MiB | 4–16 |
| lld | 64 | 1617 ms | 1662 ms | 75.65 s | 4650 MiB | 1213.7 MiB | 4–16 |
| mold | default | 1113 ms | 1172 ms | 35.43 s | 5165 MiB | 1214.2 MiB | 9–17 |
| mold | 1 | 2143 ms | 2163 ms | 2.21 s | 4688 MiB | 1214.2 MiB | 5–8 |
| mold | 8 | 598 ms | 620 ms | 4.40 s | 4792 MiB | 1214.2 MiB | 9–17 |
| mold | 64 | 1288 ms | 1336 ms | 73.14 s | 5592 MiB | 1214.2 MiB | 9–16 |
| wild | default | 660 ms | 677 ms | 11.22 s | 4398 MiB | 1213.4 MiB | 9–18 |
| wild | 1 | 2499 ms | 2574 ms | 2.89 s | 4366 MiB | 1213.4 MiB | 5–7 |
| wild | 8 | 817 ms | 823 ms | 3.77 s | 4385 MiB | 1213.4 MiB | 8–18 |
| wild | 64 | 668 ms | 679 ms | 10.49 s | 4411 MiB | 1213.4 MiB | 8–18 |
| qld before | default | 1113 ms | 1153 ms | 11.16 s | 3659 MiB | 1213.7 MiB | 8–17 |
| qld before | 1 | 4427 ms | 4570 ms | 4.56 s | 3328 MiB | 1213.7 MiB | 4–7 |
| qld before | 8 | 1206 ms | 1290 ms | 7.33 s | 3455 MiB | 1213.7 MiB | 8–17 |
| qld before | 64 | 1151 ms | 1188 ms | 31.00 s | 3768 MiB | 1213.7 MiB | 8–17 |
| qld after | default | 836 ms | 867 ms | 9.27 s | 3756 MiB | 1213.7 MiB | 11–20 |
| qld after | 1 | 3829 ms | 3915 ms | 3.91 s | 3325 MiB | 1213.7 MiB | 4–7 |
| qld after | 8 | 921 ms | 991 ms | 6.07 s | 3454 MiB | 1213.7 MiB | 11–20 |
| qld after | 64 | 896 ms | 927 ms | 28.82 s | 3895 MiB | 1213.7 MiB | 11–20 |

## Determinism

Every benchmark, with qld at 1, 2, 8 and 64 threads, after the
optimizations:

| benchmark | 1 / 2 / 8 / 64 threads | same bytes as before W24 |
| --- | --- | --- |
| clang | identical | yes |
| clang-debug | identical | yes |
| libclang-cpp | identical | yes |
| rust-qld-debug | identical | yes |
| small-count | identical | yes |
| vmlinux | identical | yes |

Also identical across thread counts, and to the old tree: clang, clang-debug
and `vmlinux` with `-O2` (string tail merging), and W23's
`synthetic:320 --build-id=sha1` link. Every optimization was checked the
same way before it was committed.

## Optimizations

Each is one commit, with its measurements in the commit message (A/B runs
of the previous and the new binary, interleaved).

| Commit | Change | Effect (min wall or stage lap) |
| --- | --- | --- |
| f1310bc | Lock-free merge-section deduplication (bucket pieces by shard, one task per shard); parallel offset assignment for groups without tail merging | clang-debug merge 425 → 195 ms; link 1212 → 946 ms |
| d479fa1 | Relocation targets: section index without building an error `Result` | clang one thread 753 → 686 ms |
| 70743b2 | Archive symbol indexes read in parallel after the input walk | clang inputs 27 → 14 ms |
| 6e963fa | Layout: members found and sized in parallel, bucketed ordering, early exit of the entry size scan | clang layout 20.5 → 9.4 ms |
| 7dadc4e | `.dynstr` names added in bulk (parallel hash, dedup and copy); parallel GNU hash ordering and output emptiness | clang dynamic 23 → 16 ms |
| 25d55ca | Output section flags folded per file in parallel | clang placement 17.7 → 11.4 ms |
| 881a1f3 | Default thread count from uncompressed input size | W23's synthetic 320 MiB `--build-id` link 905 → 169 ms |
| 99c944a | Faster portable SHA-1 (one loop per round function) | `vmlinux` one thread write 381 → 324 ms |
| 31f03fa | Relocation lookups: `Target` always inlined (no store-forwarding stall), section kinds in a dense vector | clang one thread 677 → 624 ms |
| 79778ea | Per-thread hint for the last merge piece found | clang-debug one thread 4322 → 3795 ms |

Tried and not kept (no gain within noise): a lock-free symbol interning
pass 1 (faster at 16 threads on clang's first round, slower on one
thread), numbering new symbols by job order instead of sorting, building
the write chunk list in parallel, a single writer thread fed by the
rendering workers, reusing region buffers per thread, 1,024 merge shards
instead of 256, and the mapped output backing at one thread.

## Where qld's time goes

Stage laps (`QLD_TIMING=1`, approximate: single runs at low load) at
default threads, after the optimizations:

| stage | clang | clang-debug |
| --- | --- | --- |
| inputs | 14 ms | 17 ms |
| resolution | 52 ms | 120 ms |
| placement | 12 ms | 14 ms |
| scan | 8 ms | 10 ms |
| merge | 7 ms | 195 ms |
| dynamic | 15 ms | 11 ms |
| layout | 10 ms | 10 ms |
| write | 55 ms | 370 ms |
| after the write (freeing, unmapping inputs, exit) | 25–30 ms | 120–130 ms |

- **Process teardown**: unmapping the inputs (2.5 GiB of mapped pages for
  clang-debug) takes about as long whether qld unmaps them or the kernel
  does at exit. mold and wild avoid it by forking: the parent exits as soon
  as the output is complete. qld could do the same by running the link in a
  child process started from `main` (without `unsafe`: `std::process` to
  start it, a Unix socket for the child to report that the output is
  complete and with which status), which is a change to the frozen
  `src/main.rs`; the gain is about the last row above.
- **Resolution** (clang: 52 ms against wild's ~15 ms): interning 298,000
  archive index names in the first round (20 ms: hash probes, then sorting
  new names by first occurrence), loading and COMDAT claims in later rounds.
  Deterministic dense symbol IDs by first occurrence are the costly part.
- **Write**: rendering is parallel, but buffered writes to one file are
  serialized by the kernel; on clang-debug the 1.2 GiB copy into the page
  cache is most of the 370 ms (wild spends 485 ms in its single flush).
- **Merge** (clang-debug): 18.1 million `.debug_str` pieces; the shard
  tasks' hash-table work is 132 ms at 16 threads, dominated by cache misses
  on piece bytes and tables.
- **64 threads**: qld's CPU time triples from 16 to 64 threads for the same
  work (idle rayon workers spinning between parallel stages, and
  page-fault and allocator contention: 5 s of system time on clang-debug at
  16 threads, 13 s at 64), so wall time gets worse beyond 16 threads on
  this machine; the default stays capped at 16.

## W23's open items

- *Single-threaded links 5–12% slower with the write backing*: not
  reproduced on this corpus. At `--threads=1`, `QLD_OUTPUT_BACKING=mmap`
  was not faster than `write` (clang: write stage 267 against 246 ms;
  rust-qld-debug and clang-debug within noise), so the default is unchanged.
- *A large `--build-id` link spending ~600 ms in a stage that does not
  scale*: the synthetic link's objects have compressed debug sections, so
  the default thread count (sized from input file bytes) was 1. Fixed in
  881a1f3; SHA-1 itself also got 30% faster (99c944a).
