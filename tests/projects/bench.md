# Benchmarks (W24, W26; roadmap M5)

qld against GNU ld, lld, mold and wild on real links, replayed from build
trees that already exist. The drivers are in `benches/`; this file records
the method, the corpus and the results. W26's results come first; W24's
(the first round, with GNU ld) follow from [Standing after W24](#standing-after-w24-stated-plainly).

## Standing after W26, stated plainly

Measured on 2026-09-18 on the same shared 32-core / 64-thread machine as
W24, at a load average of 9–25 (other work ran on it; per row in the full
tables below; runs are interleaved, so load changes hit every linker alike).
"Default" is each linker's own thread count (qld: one thread per 4 MiB of
input, at most 16). qld now forks by default like mold and wild (see
[Method](#method)). Minimum wall times; "qld before" is the tree W26 started
from (master 0bebc9d, the end of W24), "qld after" this branch:

| benchmark | threads | lld | mold | wild | qld before | qld after |
| --- | --- | --- | --- | --- | --- | --- |
| clang | default | 336 ms | 186 ms | 100 ms | 190 ms | 140 ms |
| clang | 1 | 405 ms | 510 ms | 447 ms | 600 ms | 525 ms |
| clang | 8 | 286 ms | 134 ms | 111 ms | 200 ms | 157 ms |
| clang | 64 | 367 ms | 222 ms | 98 ms | 238 ms | 155 ms |
| clang-debug | default | 1386 ms | 1221 ms | 647 ms | 784 ms | 755 ms |
| clang-debug | 1 | 2493 ms | 1888 ms | 2556 ms | 3635 ms | 3363 ms |
| clang-debug | 8 | 1006 ms | 651 ms | 804 ms | 856 ms | 809 ms |
| clang-debug | 64 | 1556 ms | 1286 ms | 690 ms | 901 ms | 744 ms |
| libclang-cpp | default | 227 ms | 124 ms | 74 ms | 130 ms | 110 ms |
| libclang-cpp | 1 | 297 ms | 322 ms | 281 ms | 404 ms | 383 ms |
| libclang-cpp | 8 | 203 ms | 101 ms | 75 ms | 139 ms | 122 ms |
| libclang-cpp | 64 | 242 ms | 161 ms | 71 ms | 203 ms | 118 ms |
| rust-qld-debug | default | 235 ms | 204 ms | 135 ms | 201 ms | 172 ms |
| rust-qld-debug | 1 | 360 ms | 382 ms | 403 ms | 722 ms | 710 ms |
| rust-qld-debug | 8 | 203 ms | 166 ms | 132 ms | 207 ms | 182 ms |
| rust-qld-debug | 64 | 268 ms | 234 ms | 131 ms | 218 ms | 166 ms |
| small-count | default | 10 ms | 17 ms | 8 ms | 10 ms | 9 ms |
| small-count | 1 | 10 ms | 12 ms | 5 ms | 9 ms | 9 ms |
| small-count | 8 | 9 ms | 10 ms | 4 ms | 9 ms | 7 ms |
| small-count | 64 | 11 ms | 19 ms | 9 ms | 16 ms | 11 ms |
| vmlinux | default | 180 ms | fails | fails | 142 ms | 124 ms |
| vmlinux | 1 | 224 ms | fails | fails | 422 ms | 382 ms |
| vmlinux | 8 | 175 ms | fails | fails | 146 ms | 127 ms |
| vmlinux | 64 | 177 ms | fails | fails | 157 ms | 124 ms |

- **wild is still faster than qld on every benchmark it links, at every
  thread count**: clang 100 against 140 ms at default threads,
  `libclang-cpp` 74 against 110, qld's debug binary 135 against 172. The gap
  is smallest for clang with debug information (647 against 755 ms at
  default threads, 804 against 809 at 8), where qld used to pay for process
  exit, and largest on links dominated by symbol resolution (see below).
- **qld is now faster than mold at default and 64 threads on every
  benchmark** (clang 140 against 186 ms, 155 against 222; `libclang-cpp` 110
  against 124, 118 against 161; clang with debug information 755 against
  1221, 744 against 1286). mold is still faster at 8 threads on the large
  links (clang 134 against 157, `libclang-cpp` 101 against 122, clang with
  debug information 651 against 809, qld's debug binary 166 against 182)
  and on one thread.
- **64 threads is now about as fast as the default 16** (it was 17-56%
  slower): clang 155 ms against 238 before W26, `libclang-cpp` 118 against
  203, clang with debug information 744 against 901; the CPU time at 64
  threads fell by two thirds (clang 5.1 s to 1.7 s). See
  [Scaling past 16 threads](#scaling-past-16-threads).
- **qld is faster than lld** at default, 8 and 64 threads on every benchmark
  (small-count at 64 threads: a tie), including `vmlinux`. On one thread lld, mold and wild are still faster on
  every large link (clang: lld 405, wild 447, mold 510, qld 525 ms); qld's
  own debug binary is the outlier (710 against 360-403 ms), because its
  `--build-id` is SHA-1 over 170 MiB on that one thread (see below).
- **Output is byte-identical** across 1, 2, 8 and 64 threads on all six
  benchmarks, and identical to the output of the tree W26 started from
  (and so to W24's). Peak RSS is below lld's and mold's on every large
  link, and below wild's on the two debug links.

So the M5 exit criterion (wall time at or below mold's and wild's at 8 and
64 cores) is **met against mold at 64 threads, not at 8, and not against
wild**. The W26 changes took 4-26% off qld's wall times at default threads
(clang 190 → 140 ms, `libclang-cpp` 130 → 110, qld's debug binary 201 →
172, `vmlinux` 142 → 124, clang with debug information 784 → 755),
17-42% at 64 threads, and up to 13% on one thread.

### W26 changes

Each is one commit, with its measurements in the commit message (A/B runs
of the previous and the new binary, interleaved; stage laps from
`QLD_TIMING=1`).

| Commit | Change | Effect (min wall or stage lap) |
| --- | --- | --- |
| 0638def | `--fork` (default on Unix): the link runs in a child process and `qld` returns once the output is complete, leaving the child to free memory and unmap the inputs (see [Method](#method)) | clang 192 → 163 ms, clang-debug 745 → 659, libclang-cpp 122 → 111 |
| 227b30b | `reloc::decide` always inlined (a store-forwarding stall on every relocation) | clang, one thread: 577 → 565 ms (scan lap 77 → 64) |
| 60143bf | Archive members found in parallel, one task per archive, before the (serial) input walk | clang inputs 13.6 → 8.1 ms |
| 8160790 | Symbol versions (`@`) found a word at a time | clang resolution, one thread: 158 → 152 ms |
| 786ee3a | COMDAT claims look up again only the offers that held their key | clang resolution, one thread: 150 → 145 ms |
| 2d8b69c | Interning of later resolution batches in parallel from 16,384 names (was 65,536) | rust-qld-debug resolution 58 → 48 ms; clang 61 → 56 ms at 64 threads |
| 41d4cab | In a pool over 16 threads, every stage but the relocation scan and section merging runs in a pool of 16 | 64 threads: clang 203 → 155 ms, clang-debug 771 → 670, libclang-cpp 189 → 121 |
| 6c82625 | Links with at least 4 Mi merge pieces merge them on every core | clang-debug 687 → 664 ms (merge lap 184 → 169) |
| 6bf6161, ba07b13 | Code padding written with the input section before it: 147,000 fewer chunks for clang, found by a forward walk | clang write lap 51 → 48 ms; one thread 219 → 193 ms |
| a8193a8 | Dynamic symbols' import versions looked up once, in parallel | dynamic lap: clang 14.8 → 13.4 ms, libclang-cpp 10.1 → 8.8 |
| 0da64af | `--fork` stays on for paths that merely contain `dev` (only `/dev/…` and `/proc/…` keep the link in process) | correctness of the fork decision |
| 6f1215d | Section merging runs side by side with the relocation scan | clang scan + merge laps 10.9 → 9.5 ms; 8 threads 14.5 → 12.1 |
| 6ef8e0b | On one thread, symbols are interned in order whatever the batch size | resolution, one thread: clang 154 → 128 ms, rust-qld-debug 110 → 99 |

### Scaling past 16 threads

Before W26, qld at 64 threads was slower than at 16 (clang 238 against 190
ms) and burnt three times the CPU time. Two causes, found with `perf`
(user space only on this machine) and `strace -c`:

- **Idle workers.** The pipeline is dozens of short parallel steps separated
  by short serial ones. Between steps, rayon's idle workers spin: each round
  of looking for work tries to steal from every other worker's deque, then
  calls `sched_yield`, 32 rounds before sleeping; waking them for the next
  step is a chain of futex wake-ups. At 64 threads, half of the samples of
  a clang link were in `Stealer::steal`, the crossbeam epoch and rayon's
  sleep code; even at 16 threads the link made 31,700 `sched_yield` and
  19,600 `futex` calls. On 32 cores the spinning workers also share cores
  with the busy ones.
- **The kernel.** Mapping 1,014 objects took 5 ms on 16 threads and 28-53 ms
  on 64 (many threads mapping files into one address space); the write of a 1.2 GiB output took 353 against 451 ms
  (buffered writes to one file are serialized by the inode lock, and more
  writers only wait longer). glibc's per-thread malloc arenas grow 4 KiB at
  a time, each growth an `mprotect` that takes the process's `mmap_lock` for
  writing: 13,800 `mprotect` calls in a clang link, 10,700 of them during
  resolution.

Stage laps at 4-24 threads (clang, after W26, load 11) show where it
flattens:

| stage | 4 | 8 | 12 | 16 | 24 |
| --- | --- | --- | --- | --- | --- |
| inputs | 9.1 ms | 7.0 | 7.8 | 7.6 | 7.6 |
| resolution | 52.7 | 43.8 | 42.0 | 40.7 | 42.9 |
| placement | 18.1 | 14.1 | 13.5 | 11.0 | 11.6 |
| scan | 18.0 | 10.8 | 9.6 | 7.2 | 7.4 |
| merge | 4.3 | 3.7 | 4.0 | 3.9 | 4.6 |
| dynamic | 14.2 | 13.3 | 13.3 | 12.6 | 14.4 |
| layout | 10.9 | 9.9 | 9.6 | 9.5 | 9.2 |
| write | 67.0 | 47.1 | 47.9 | 46.7 | 46.8 |

Past 8 threads only the scan and placement still gain; merging still gains
with 18 million pieces (clang-debug: 182 ms on 16 threads, 153 on 64). So
(41d4cab) when the pool is larger than 16 threads, as with `--threads=64`,
the ELF driver runs every stage but the scan and merging in a second pool
of 16 threads (`Narrow` in `src/elf/link.rs`); the large pool's workers
then sleep instead of spinning. The default stays capped at 16 threads:
clang took 166-174 ms from 8 to 32 threads and 186-201 at 48-64 before this
change, and a default above 16 gains nothing now that the extra threads
would only serve the scan (a large debug link's merge gets its own pool of
one thread per core, 6c82625).

What remains at 64 threads: rayon still spins between the scan's and the
merge's steps, and `mprotect` from malloc arena growth. glibc's tunables
would help (`glibc.malloc.top_pad=16777216` took clang's resolution from
47 to 39 ms and cut `mprotect` calls from 13,800 to 1,200;
`glibc.malloc.hugetlb=1` to 174), but they can only be set in the
environment, results on the other benchmarks were mixed (clang-debug was
slower with both), and a child's environment is inherited by the LTO
plugin's subprocesses: not kept.

### Where qld's time goes after W26

Stage laps (`QLD_TIMING=1`, `--no-fork` to include the exit), min of 7 runs
at load 6 (clang-debug: 5 runs at load 15), before and after W26:

| stage | clang before | clang after | clang-debug before | clang-debug after |
| --- | --- | --- | --- | --- |
| inputs | 13.9 ms | 7.2 ms | 15.0 ms | 8.4 ms |
| resolution | 40.3 | 34.6 | 92.4 | 90.6 |
| placement | 11.2 | 12.5 | 11.9 | 13.2 |
| scan (after: and merge) | 8.4 | 9.5 | 8.5 | 178.0 |
| merge | 4.2 | 0.2 | 197.3 | 0.4 |
| dynamic | 14.9 | 13.5 | 11.4 | 13.5 |
| layout | 10.7 | 9.9 | 9.6 | 11.1 |
| write | 54.2 | 51.0 | 406.9 | 389.8 |
| after the write (hidden by `--fork`) | 31.4 | 33.3 | 86.8 | 100.5 |

- **Resolution** is the largest gap to wild (clang: 35-45 ms against about
  15 for wild's loading and resolution). It is ten rounds of load, COMDAT
  claims, interning and insertion, each ending in a barrier, and the first
  round interns 298,000 archive index names (150,000 distinct) into an
  empty table: 10-17 ms at 16 threads, of which 6-14 ms in the lookup pass
  and 5 ms numbering the new names by first occurrence. Symbol IDs follow
  first occurrence and decide output order (`.dynsym` within a hash
  bucket), so a design that interns every archive member's symbols up
  front, as wild does, would change the output; closing this gap needs a
  new resolution design that keeps the IDs.
- **Write**: rendering is parallel, but buffered writes to one file are
  serialized by the kernel (131 MiB take 30 ms with `dd` on this btrfs; 1.2
  GiB 310 ms from one thread or from eight, with or without `fallocate`).
  clang's write lap is 46-51 ms from 8 threads up.
- **One thread**: clang 525 ms against wild's 447 and lld's 405. The write
  (193 ms, relocation processing and copying) and resolution (128 ms, half
  of it parsing archive members) dominate. qld's own debug binary spends
  265 ms of its 473 ms write in SHA-1 for `--build-id` (170 MiB at 640
  MB/s, portable code: SHA-NI would need `unsafe` outside the modules that
  may use it). lld reads `--build-id` without a value as its fast hash,
  where qld follows GNU ld's SHA-1.

### Tried in W26 and not kept

Each was measured with interleaved A/B runs and did not help beyond noise,
or was slower somewhere:

- Parsing every archive member ahead of time, in parallel with the first
  resolution round (a `ResolveFile::prepare` hook): resolution 49 → 55-74 ms
  at 16 threads, 171 → 215 ms on one thread (13% more members parsed, and
  the parsed data evicted from cache before use).
- Interning partitioned by shard (a counting sort of each batch's names by
  shard, then one lock-free task per shard) instead of taking each shard's
  lock per name: the lock's atomic operations take 90% of the lookup's
  samples, but they only absorb the cache misses; resolution unchanged at
  16 and 64 threads (median 47 ms both ways), 164 → 175 ms on one thread.
- Sizing the symbol table from the archive index names up front: the first
  round's lookup pass got faster (12-14 → 7-10 ms), resolution did not.
- An index of the default section rules by the name's first four bytes:
  placement unchanged (52 → 51 ms on one thread); the cost is reading the
  section names, not the patterns.
- `ElfFile::relocation_section` always inlined, and a smaller `relocations`
  accessor for the scan and the write (the profile blamed a store-forward
  stall after the call): no change.
- Skipping the atomic OR of a symbol's flags in the scan when the flags
  are already set: no change.
- Grouping layout's members per output section in parallel (a stable sort
  per file, then a gather per output): slower on one thread, unchanged on
  16.
- Running the dynamic and regular symbol table plans side by side: not
  correct, as the dynamic plan sets reference flags on strong aliases of
  weak imports (`environ`) that the symbol table reads.
- glibc malloc tunables in the child's environment (see above).
- The symbol table's shard locks, COMDAT claim locks and definition locks
  each alone in a 128-byte cache line pair (the first interning pass was
  no faster on 16 threads than on 8; false sharing was a suspect): no
  change in the pass or in resolution.
- Presizing each shard's table and pending list from a count of the first
  batch's names per shard: no change (the lookup pass took 10-11 ms at 16
  threads either way).
- Reusing the input stage's pool of 16 threads for the rest of the link
  instead of starting a second one: no change.

Not attempted, for later: freeing the inputs' mappings in parallel
(`madvise(MADV_DONTNEED)`, as mold does) for `--no-fork` and library links;
interning each repeated archive's index names once (clang names 22 archives
twice: 84,500 of the first round's 298,000 names); overlapping the COMDAT
claims of a resolution round with its interning; splitting the reading of
a large archive's symbol index (the inputs stage reads each archive's in
one task, 4 ms for clang).

### Full results (W26)

Wall time, CPU (user+system, from a `--no-fork` run for the forking
linkers), peak RSS and output size, from the run of the table above
(`benches/run.py SPECS --linker qld-base=... --only lld,mold,wild,qld-base,qld
--json`; `benches/report.py --compact` prints the summary table).

#### clang

| linker | threads | wall min | wall median | CPU | peak RSS | output | load |
| --- | --- | --- | --- | --- | --- | --- | --- |
| lld | default | 336 ms | 360 ms | 2.20 s | 737 MiB | 131.3 MiB | 9–15 |
| lld | 1 | 405 ms | 426 ms | 0.42 s | 732 MiB | 131.3 MiB | 9–15 |
| lld | 8 | 286 ms | 328 ms | 1.15 s | 736 MiB | 131.3 MiB | 9–15 |
| lld | 64 | 367 ms | 384 ms | 4.87 s | 730 MiB | 131.3 MiB | 9–15 |
| mold | default | 186 ms | 199 ms | 5.34 s | 840 MiB | 131.8 MiB | 9–15 |
| mold | 1 | 510 ms | 601 ms | 0.52 s | 669 MiB | 131.8 MiB | 9–19 |
| mold | 8 | 134 ms | 206 ms | 1.12 s | 717 MiB | 131.8 MiB | 9–19 |
| mold | 64 | 222 ms | 239 ms | 12.64 s | 1001 MiB | 131.8 MiB | 9–19 |
| wild | default | 100 ms | 105 ms | 1.56 s | 499 MiB | 131.0 MiB | 9–19 |
| wild | 1 | 447 ms | 494 ms | 0.49 s | 504 MiB | 131.0 MiB | 9–19 |
| wild | 8 | 111 ms | 119 ms | 0.59 s | 503 MiB | 131.0 MiB | 9–19 |
| wild | 64 | 98 ms | 100 ms | 1.50 s | 506 MiB | 131.0 MiB | 11–19 |
| qld before | default | 190 ms | 205 ms | 1.53 s | 628 MiB | 131.3 MiB | 11–19 |
| qld before | 1 | 600 ms | 659 ms | 0.62 s | 586 MiB | 131.3 MiB | 11–19 |
| qld before | 8 | 200 ms | 210 ms | 1.06 s | 605 MiB | 131.3 MiB | 11–19 |
| qld before | 64 | 238 ms | 254 ms | 5.11 s | 704 MiB | 131.3 MiB | 11–19 |
| qld after | default | 140 ms | 155 ms | 1.44 s | 612 MiB | 131.3 MiB | 11–18 |
| qld after | 1 | 525 ms | 563 ms | 0.61 s | 574 MiB | 131.3 MiB | 12–18 |
| qld after | 8 | 157 ms | 165 ms | 1.00 s | 581 MiB | 131.3 MiB | 12–18 |
| qld after | 64 | 155 ms | 165 ms | 1.65 s | 614 MiB | 131.3 MiB | 12–18 |

#### clang-debug

| linker | threads | wall min | wall median | CPU | peak RSS | output | load |
| --- | --- | --- | --- | --- | --- | --- | --- |
| lld | default | 1386 ms | 1619 ms | 20.81 s | 4617 MiB | 1213.7 MiB | 13–19 |
| lld | 1 | 2493 ms | 2689 ms | 2.68 s | 4612 MiB | 1213.7 MiB | 12–19 |
| lld | 8 | 1006 ms | 1153 ms | 7.17 s | 4613 MiB | 1213.7 MiB | 11–20 |
| lld | 64 | 1556 ms | 1590 ms | 71.52 s | 4650 MiB | 1213.7 MiB | 13–20 |
| mold | default | 1221 ms | 1279 ms | 40.65 s | 5143 MiB | 1214.2 MiB | 13–20 |
| mold | 1 | 1888 ms | 1980 ms | 2.08 s | 4688 MiB | 1214.2 MiB | 14–20 |
| mold | 8 | 651 ms | 864 ms | 4.84 s | 4809 MiB | 1214.2 MiB | 14–19 |
| mold | 64 | 1286 ms | 1319 ms | 69.65 s | 5562 MiB | 1214.2 MiB | 14–19 |
| wild | default | 647 ms | 685 ms | 10.60 s | 4407 MiB | 1213.4 MiB | 15–25 |
| wild | 1 | 2556 ms | 2635 ms | 2.73 s | 4363 MiB | 1213.4 MiB | 15–25 |
| wild | 8 | 804 ms | 823 ms | 3.29 s | 4379 MiB | 1213.4 MiB | 14–23 |
| wild | 64 | 690 ms | 769 ms | 10.67 s | 4403 MiB | 1213.4 MiB | 14–23 |
| qld before | default | 784 ms | 832 ms | 9.36 s | 3775 MiB | 1213.7 MiB | 14–22 |
| qld before | 1 | 3635 ms | 4135 ms | 3.97 s | 3335 MiB | 1213.7 MiB | 14–22 |
| qld before | 8 | 856 ms | 926 ms | 5.77 s | 3466 MiB | 1213.7 MiB | 14–22 |
| qld before | 64 | 901 ms | 942 ms | 30.20 s | 3882 MiB | 1213.7 MiB | 14–21 |
| qld after | default | 755 ms | 790 ms | 12.77 s | 3696 MiB | 1213.7 MiB | 14–21 |
| qld after | 1 | 3363 ms | 3474 ms | 3.52 s | 3335 MiB | 1213.7 MiB | 14–21 |
| qld after | 8 | 809 ms | 868 ms | 5.40 s | 3439 MiB | 1213.7 MiB | 13–20 |
| qld after | 64 | 744 ms | 873 ms | 13.69 s | 3956 MiB | 1213.7 MiB | 13–19 |

#### libclang-cpp

| linker | threads | wall min | wall median | CPU | peak RSS | output | load |
| --- | --- | --- | --- | --- | --- | --- | --- |
| lld | default | 227 ms | 231 ms | 1.34 s | 398 MiB | 77.4 MiB | 22–25 |
| lld | 1 | 297 ms | 312 ms | 0.31 s | 401 MiB | 77.4 MiB | 22–25 |
| lld | 8 | 203 ms | 233 ms | 0.81 s | 402 MiB | 77.4 MiB | 22–25 |
| lld | 64 | 242 ms | 252 ms | 2.27 s | 387 MiB | 77.4 MiB | 22–25 |
| mold | default | 124 ms | 141 ms | 3.79 s | 573 MiB | 78.0 MiB | 22–25 |
| mold | 1 | 322 ms | 335 ms | 0.33 s | 361 MiB | 78.0 MiB | 22–25 |
| mold | 8 | 101 ms | 128 ms | 0.82 s | 402 MiB | 78.0 MiB | 22–25 |
| mold | 64 | 161 ms | 167 ms | 8.75 s | 744 MiB | 78.0 MiB | 21–25 |
| wild | default | 74 ms | 75 ms | 1.37 s | 297 MiB | 77.3 MiB | 21–25 |
| wild | 1 | 281 ms | 287 ms | 0.31 s | 307 MiB | 77.3 MiB | 21–24 |
| wild | 8 | 75 ms | 78 ms | 0.36 s | 308 MiB | 77.3 MiB | 21–24 |
| wild | 64 | 71 ms | 90 ms | 1.12 s | 299 MiB | 77.3 MiB | 21–24 |
| qld before | default | 130 ms | 138 ms | 0.90 s | 334 MiB | 77.5 MiB | 21–24 |
| qld before | 1 | 404 ms | 436 ms | 0.40 s | 307 MiB | 77.5 MiB | 21–25 |
| qld before | 8 | 139 ms | 143 ms | 0.67 s | 336 MiB | 77.5 MiB | 21–25 |
| qld before | 64 | 203 ms | 227 ms | 3.42 s | 387 MiB | 77.5 MiB | 21–25 |
| qld after | default | 110 ms | 117 ms | 1.05 s | 325 MiB | 77.5 MiB | 21–25 |
| qld after | 1 | 383 ms | 401 ms | 0.37 s | 300 MiB | 77.5 MiB | 21–25 |
| qld after | 8 | 122 ms | 129 ms | 0.64 s | 320 MiB | 77.5 MiB | 21–25 |
| qld after | 64 | 118 ms | 120 ms | 1.11 s | 327 MiB | 77.5 MiB | 21–25 |

#### rust-qld-debug

| linker | threads | wall min | wall median | CPU | peak RSS | output | load |
| --- | --- | --- | --- | --- | --- | --- | --- |
| lld | default | 235 ms | 248 ms | 2.25 s | 613 MiB | 170.6 MiB | 16–21 |
| lld | 1 | 360 ms | 398 ms | 0.40 s | 616 MiB | 170.6 MiB | 16–20 |
| lld | 8 | 203 ms | 237 ms | 1.19 s | 613 MiB | 170.6 MiB | 16–20 |
| lld | 64 | 268 ms | 296 ms | 7.50 s | 613 MiB | 170.6 MiB | 16–20 |
| mold | default | 204 ms | 216 ms | 6.28 s | 798 MiB | 189.7 MiB | 16–20 |
| mold | 1 | 382 ms | 412 ms | 0.40 s | 622 MiB | 189.7 MiB | 16–20 |
| mold | 8 | 166 ms | 178 ms | 1.26 s | 689 MiB | 189.7 MiB | 16–20 |
| mold | 64 | 234 ms | 251 ms | 12.66 s | 911 MiB | 189.7 MiB | 16–20 |
| wild | default | 135 ms | 137 ms | 1.95 s | 587 MiB | 169.6 MiB | 16–20 |
| wild | 1 | 403 ms | 412 ms | 0.42 s | 595 MiB | 169.6 MiB | 15–20 |
| wild | 8 | 132 ms | 145 ms | 0.55 s | 592 MiB | 169.6 MiB | 15–20 |
| wild | 64 | 131 ms | 135 ms | 1.60 s | 596 MiB | 169.6 MiB | 15–20 |
| qld before | default | 201 ms | 205 ms | 1.52 s | 568 MiB | 169.6 MiB | 15–20 |
| qld before | 1 | 722 ms | 781 ms | 0.83 s | 521 MiB | 169.6 MiB | 15–20 |
| qld before | 8 | 207 ms | 225 ms | 1.10 s | 556 MiB | 169.6 MiB | 15–18 |
| qld before | 64 | 218 ms | 222 ms | 3.42 s | 616 MiB | 169.6 MiB | 15–18 |
| qld after | default | 172 ms | 182 ms | 1.41 s | 577 MiB | 169.6 MiB | 15–18 |
| qld after | 1 | 710 ms | 780 ms | 0.80 s | 517 MiB | 169.6 MiB | 15–18 |
| qld after | 8 | 182 ms | 191 ms | 1.10 s | 563 MiB | 169.6 MiB | 15–18 |
| qld after | 64 | 166 ms | 184 ms | 1.90 s | 595 MiB | 169.6 MiB | 15–18 |

#### small-count

| linker | threads | wall min | wall median | CPU | peak RSS | output | load |
| --- | --- | --- | --- | --- | --- | --- | --- |
| lld | default | 10 ms | 10 ms | 0.02 s | 37 MiB | 0.0 MiB | 16–16 |
| lld | 1 | 10 ms | 11 ms | 0.01 s | 38 MiB | 0.0 MiB | 16–16 |
| lld | 8 | 9 ms | 10 ms | 0.01 s | 37 MiB | 0.0 MiB | 16–16 |
| lld | 64 | 11 ms | 12 ms | 0.02 s | 37 MiB | 0.0 MiB | 16–16 |
| mold | default | 17 ms | 17 ms | 0.29 s | 106 MiB | 0.0 MiB | 16–16 |
| mold | 1 | 12 ms | 12 ms | 0.01 s | 37 MiB | 0.0 MiB | 16–16 |
| mold | 8 | 10 ms | 11 ms | 0.05 s | 54 MiB | 0.0 MiB | 16–16 |
| mold | 64 | 19 ms | 20 ms | 0.39 s | 120 MiB | 0.0 MiB | 16–16 |
| wild | default | 8 ms | 10 ms | 0.13 s | 20 MiB | 0.0 MiB | 16–16 |
| wild | 1 | 5 ms | 5 ms | 0.01 s | 20 MiB | 0.0 MiB | 16–16 |
| wild | 8 | 4 ms | 5 ms | 0.02 s | 20 MiB | 0.0 MiB | 16–16 |
| wild | 64 | 9 ms | 10 ms | 0.14 s | 20 MiB | 0.0 MiB | 16–16 |
| qld before | default | 10 ms | 13 ms | 0.03 s | 21 MiB | 0.0 MiB | 16–16 |
| qld before | 1 | 9 ms | 9 ms | 0.01 s | 24 MiB | 0.0 MiB | 16–16 |
| qld before | 8 | 9 ms | 10 ms | 0.03 s | 22 MiB | 0.0 MiB | 16–16 |
| qld before | 64 | 16 ms | 20 ms | 0.44 s | 22 MiB | 0.0 MiB | 16–16 |
| qld after | default | 9 ms | 10 ms | 0.03 s | 20 MiB | 0.0 MiB | 16–16 |
| qld after | 1 | 9 ms | 10 ms | 0.01 s | 24 MiB | 0.0 MiB | 16–16 |
| qld after | 8 | 7 ms | 8 ms | 0.02 s | 22 MiB | 0.0 MiB | 16–16 |
| qld after | 64 | 11 ms | 12 ms | 0.11 s | 20 MiB | 0.0 MiB | 16–16 |

#### vmlinux

| linker | threads | wall min | wall median | CPU | peak RSS | output | load |
| --- | --- | --- | --- | --- | --- | --- | --- |
| lld | default | 180 ms | 193 ms | 0.78 s | 326 MiB | 98.6 MiB | 15–17 |
| lld | 1 | 224 ms | 234 ms | 0.23 s | 331 MiB | 98.6 MiB | 15–17 |
| lld | 8 | 175 ms | 184 ms | 0.49 s | 327 MiB | 98.6 MiB | 15–17 |
| lld | 64 | 177 ms | 195 ms | 1.15 s | 329 MiB | 98.6 MiB | 15–17 |
| mold | default | fails: link failed:                                               ^ unknown l | | | | | |
| mold | 1 | fails: link failed:                                               ^ unknown l | | | | | |
| mold | 8 | fails: link failed:                                               ^ unknown l | | | | | |
| mold | 64 | fails: link failed:                                               ^ unknown l | | | | | |
| wild | default | fails: link failed: wild: error: unrecognized option(s): --emit-relocs | | | | | |
| wild | 1 | fails: link failed: wild: error: unrecognized option(s): --emit-relocs | | | | | |
| wild | 8 | fails: link failed: wild: error: unrecognized option(s): --emit-relocs | | | | | |
| wild | 64 | fails: link failed: wild: error: unrecognized option(s): --emit-relocs | | | | | |
| qld before | default | 142 ms | 146 ms | 0.67 s | 210 MiB | 98.6 MiB | 15–17 |
| qld before | 1 | 422 ms | 457 ms | 0.41 s | 146 MiB | 98.6 MiB | 15–17 |
| qld before | 8 | 146 ms | 153 ms | 0.53 s | 199 MiB | 98.6 MiB | 15–17 |
| qld before | 64 | 157 ms | 167 ms | 1.49 s | 221 MiB | 98.6 MiB | 15–17 |
| qld after | default | 124 ms | 132 ms | 0.68 s | 215 MiB | 98.6 MiB | 15–17 |
| qld after | 1 | 382 ms | 418 ms | 0.43 s | 144 MiB | 98.6 MiB | 15–17 |
| qld after | 8 | 127 ms | 139 ms | 0.54 s | 204 MiB | 98.6 MiB | 15–17 |
| qld after | 64 | 124 ms | 131 ms | 0.77 s | 216 MiB | 98.6 MiB | 15–17 |

## Standing after W24, stated plainly

Measured on 2026-09-15 on a shared 32-core / 64-thread machine, at a load
average of 2–17 (per row in the tables below). "Default" is each linker's
own thread count (qld: one thread per 4 MiB of input, at most 16). Minimum
wall times:

- **wild is faster than qld on every benchmark it can link, at every
  thread count.** At default threads: clang 101 ms against qld's 197,
  libclang-cpp 76 against 134, qld's own debug binary 129 against 202,
  clang with debug info 670 against 837.
- **mold is faster than qld at 8 threads on every benchmark** (clang 114
  against 198 ms, clang-debug 543 against 946), and at default and 64
  threads on clang (177 against 197, 205 against 232) and libclang-cpp
  (125 against 134, 157 against 198). qld is faster than mold at default
  and 64 threads on clang-debug (837 against 1121, 895 against 1250) and
  qld's debug binary (202 against 208, 219 against 244).
- **qld is faster than lld** at default, 8 and 64 threads on every
  benchmark except qld's debug binary at 8 threads (217 against 199 ms) and
  the small link (a tie), including `vmlinux` (148 against 153 ms at
  default), which neither mold nor wild can link. On one thread, lld, mold
  and wild are all faster than qld on every large link (clang: lld 418,
  wild 460, mold 498, qld 627 ms).
- **Peak RSS is below lld's on all six benchmarks** (clang-debug: qld
  3.8 GiB, lld 4.6, wild 4.4, mold 5.2).
- **Output is byte-identical across 1, 2, 8 and 64 threads** on all six
  benchmarks, and identical to the output of the tree W24 started from.

So the M5 exit criterion (wall time at or below mold's and wild's at 8 and
64 cores) is **not met**. The W24 optimizations took 16–22% off qld's
default-thread link times (clang 253 → 197 ms, clang-debug 1080 → 837 ms,
libclang-cpp 160 → 134 ms, qld's debug binary 240 → 202 ms, `vmlinux`
190 → 148 ms) and 16–25% off its single-threaded ones.

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
   run. mold, wild and (since W26) qld fork by default and return once the
   output is written, leaving a child to clean up; their wall time is
   measured that way, but CPU and RSS come from an extra `--no-fork` run,
   since `wait4` does not see the child. The wall time ends when the
   linker's process has exited and its stderr pipe is closed: wild and qld
   close theirs when they return (qld relays the child's pipes, see
   `src/main.rs`), mold's child keeps them until it exits, so mold's times
   include its exit.
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

## Results (W24)

"qld before" is the tree W24 started from (commit a85c629), "qld after"
this branch; both ran in the same interleaved runs as the other linkers.
Times are wall-clock; CPU is user+system.

### clang

| linker | threads | wall min | wall median | CPU | peak RSS | output | load |
| --- | --- | --- | --- | --- | --- | --- | --- |
| gnu | default | 2046 ms | 2065 ms | 2.06 s | 622 MiB | 131.2 MiB | 2–4 |
| lld | default | 328 ms | 339 ms | 2.07 s | 739 MiB | 131.3 MiB | 2–4 |
| lld | 1 | 418 ms | 429 ms | 0.43 s | 732 MiB | 131.3 MiB | 2–5 |
| lld | 8 | 263 ms | 305 ms | 1.00 s | 740 MiB | 131.3 MiB | 2–5 |
| lld | 64 | 370 ms | 376 ms | 5.08 s | 724 MiB | 131.3 MiB | 2–5 |
| mold | default | 177 ms | 185 ms | 5.77 s | 854 MiB | 131.8 MiB | 2–5 |
| mold | 1 | 498 ms | 506 ms | 0.50 s | 671 MiB | 131.8 MiB | 2–5 |
| mold | 8 | 114 ms | 161 ms | 1.23 s | 725 MiB | 131.8 MiB | 2–5 |
| mold | 64 | 205 ms | 219 ms | 12.04 s | 992 MiB | 131.8 MiB | 2–5 |
| wild | default | 101 ms | 104 ms | 1.62 s | 498 MiB | 131.0 MiB | 2–5 |
| wild | 1 | 460 ms | 463 ms | 0.48 s | 506 MiB | 131.0 MiB | 2–5 |
| wild | 8 | 117 ms | 119 ms | 0.58 s | 508 MiB | 131.0 MiB | 2–5 |
| wild | 64 | 100 ms | 102 ms | 1.75 s | 496 MiB | 131.0 MiB | 2–5 |
| qld before | default | 253 ms | 254 ms | 1.55 s | 620 MiB | 131.3 MiB | 2–5 |
| qld before | 1 | 744 ms | 758 ms | 0.76 s | 572 MiB | 131.3 MiB | 2–5 |
| qld before | 8 | 248 ms | 259 ms | 1.02 s | 601 MiB | 131.3 MiB | 2–5 |
| qld before | 64 | 282 ms | 289 ms | 4.52 s | 702 MiB | 131.3 MiB | 2–5 |
| qld after | default | 197 ms | 199 ms | 1.58 s | 625 MiB | 131.3 MiB | 2–5 |
| qld after | 1 | 627 ms | 631 ms | 0.63 s | 588 MiB | 131.3 MiB | 2–5 |
| qld after | 8 | 198 ms | 202 ms | 0.91 s | 611 MiB | 131.3 MiB | 2–5 |
| qld after | 64 | 232 ms | 240 ms | 5.47 s | 715 MiB | 131.3 MiB | 2–5 |

### libclang-cpp

| linker | threads | wall min | wall median | CPU | peak RSS | output | load |
| --- | --- | --- | --- | --- | --- | --- | --- |
| gnu | default | 1354 ms | 1377 ms | 1.37 s | 361 MiB | 77.5 MiB | 7–10 |
| lld | default | 221 ms | 239 ms | 1.42 s | 401 MiB | 77.4 MiB | 6–10 |
| lld | 1 | 293 ms | 307 ms | 0.31 s | 399 MiB | 77.4 MiB | 6–10 |
| lld | 8 | 197 ms | 236 ms | 0.83 s | 402 MiB | 77.4 MiB | 6–10 |
| lld | 64 | 239 ms | 242 ms | 2.26 s | 386 MiB | 77.4 MiB | 6–10 |
| mold | default | 125 ms | 132 ms | 3.48 s | 574 MiB | 78.0 MiB | 6–10 |
| mold | 1 | 321 ms | 324 ms | 0.33 s | 361 MiB | 78.0 MiB | 6–10 |
| mold | 8 | 91 ms | 106 ms | 0.56 s | 414 MiB | 78.0 MiB | 6–10 |
| mold | 64 | 157 ms | 164 ms | 8.25 s | 728 MiB | 78.0 MiB | 6–10 |
| wild | default | 76 ms | 77 ms | 1.45 s | 292 MiB | 77.3 MiB | 6–10 |
| wild | 1 | 289 ms | 290 ms | 0.30 s | 307 MiB | 77.3 MiB | 6–9 |
| wild | 8 | 78 ms | 80 ms | 0.39 s | 308 MiB | 77.3 MiB | 6–9 |
| wild | 64 | 77 ms | 103 ms | 1.33 s | 295 MiB | 77.3 MiB | 6–9 |
| qld before | default | 160 ms | 166 ms | 1.05 s | 347 MiB | 77.5 MiB | 6–9 |
| qld before | 1 | 501 ms | 504 ms | 0.50 s | 305 MiB | 77.5 MiB | 6–9 |
| qld before | 8 | 160 ms | 179 ms | 0.72 s | 331 MiB | 77.5 MiB | 6–9 |
| qld before | 64 | 228 ms | 241 ms | 3.06 s | 389 MiB | 77.5 MiB | 7–9 |
| qld after | default | 134 ms | 137 ms | 1.00 s | 348 MiB | 77.5 MiB | 7–9 |
| qld after | 1 | 422 ms | 428 ms | 0.43 s | 307 MiB | 77.5 MiB | 7–9 |
| qld after | 8 | 132 ms | 147 ms | 0.68 s | 333 MiB | 77.5 MiB | 7–9 |
| qld after | 64 | 198 ms | 207 ms | 3.21 s | 389 MiB | 77.5 MiB | 7–9 |

### small-count

| linker | threads | wall min | wall median | CPU | peak RSS | output | load |
| --- | --- | --- | --- | --- | --- | --- | --- |
| gnu | default | 28 ms | 28 ms | 0.03 s | 21 MiB | 0.0 MiB | 6–7 |
| lld | default | 9 ms | 10 ms | 0.02 s | 37 MiB | 0.0 MiB | 6–7 |
| lld | 1 | 9 ms | 9 ms | 0.01 s | 38 MiB | 0.0 MiB | 6–7 |
| lld | 8 | 8 ms | 8 ms | 0.01 s | 37 MiB | 0.0 MiB | 6–7 |
| lld | 64 | 9 ms | 10 ms | 0.03 s | 37 MiB | 0.0 MiB | 6–7 |
| mold | default | 12 ms | 13 ms | 0.18 s | 104 MiB | 0.0 MiB | 6–7 |
| mold | 1 | 10 ms | 11 ms | 0.01 s | 37 MiB | 0.0 MiB | 6–7 |
| mold | 8 | 10 ms | 11 ms | 0.04 s | 52 MiB | 0.0 MiB | 6–7 |
| mold | 64 | 15 ms | 17 ms | 0.45 s | 140 MiB | 0.0 MiB | 6–7 |
| wild | default | 9 ms | 10 ms | 0.20 s | 21 MiB | 0.0 MiB | 6–7 |
| wild | 1 | 5 ms | 5 ms | 0.00 s | 21 MiB | 0.0 MiB | 6–7 |
| wild | 8 | 5 ms | 5 ms | 0.02 s | 21 MiB | 0.0 MiB | 6–7 |
| wild | 64 | 9 ms | 9 ms | 0.19 s | 21 MiB | 0.0 MiB | 6–7 |
| qld before | default | 9 ms | 10 ms | 0.03 s | 22 MiB | 0.0 MiB | 6–7 |
| qld before | 1 | 8 ms | 8 ms | 0.01 s | 23 MiB | 0.0 MiB | 6–7 |
| qld before | 8 | 8 ms | 9 ms | 0.03 s | 21 MiB | 0.0 MiB | 6–7 |
| qld before | 64 | 17 ms | 17 ms | 0.51 s | 21 MiB | 0.0 MiB | 6–7 |
| qld after | default | 9 ms | 10 ms | 0.03 s | 21 MiB | 0.0 MiB | 6–7 |
| qld after | 1 | 8 ms | 9 ms | 0.01 s | 24 MiB | 0.0 MiB | 6–7 |
| qld after | 8 | 9 ms | 10 ms | 0.03 s | 21 MiB | 0.0 MiB | 6–7 |
| qld after | 64 | 16 ms | 17 ms | 0.51 s | 21 MiB | 0.0 MiB | 6–7 |

### rust-qld-debug

| linker | threads | wall min | wall median | CPU | peak RSS | output | load |
| --- | --- | --- | --- | --- | --- | --- | --- |
| gnu | default | 1347 ms | 1381 ms | 1.38 s | 469 MiB | 162.3 MiB | 5–10 |
| lld | default | 231 ms | 253 ms | 2.32 s | 616 MiB | 170.6 MiB | 5–10 |
| lld | 1 | 386 ms | 393 ms | 0.39 s | 616 MiB | 170.6 MiB | 5–10 |
| lld | 8 | 199 ms | 226 ms | 1.07 s | 614 MiB | 170.6 MiB | 5–10 |
| lld | 64 | 267 ms | 282 ms | 7.04 s | 616 MiB | 170.6 MiB | 5–10 |
| mold | default | 208 ms | 215 ms | 5.98 s | 792 MiB | 189.7 MiB | 5–10 |
| mold | 1 | 395 ms | 410 ms | 0.39 s | 622 MiB | 189.7 MiB | 5–10 |
| mold | 8 | 111 ms | 192 ms | 1.19 s | 688 MiB | 189.7 MiB | 5–10 |
| mold | 64 | 244 ms | 255 ms | 12.77 s | 924 MiB | 189.7 MiB | 5–10 |
| wild | default | 129 ms | 133 ms | 1.93 s | 587 MiB | 169.6 MiB | 5–10 |
| wild | 1 | 391 ms | 397 ms | 0.41 s | 597 MiB | 169.6 MiB | 5–10 |
| wild | 8 | 132 ms | 134 ms | 0.55 s | 593 MiB | 169.6 MiB | 5–9 |
| wild | 64 | 126 ms | 132 ms | 1.96 s | 584 MiB | 169.6 MiB | 5–9 |
| qld before | default | 240 ms | 252 ms | 1.68 s | 583 MiB | 169.6 MiB | 5–9 |
| qld before | 1 | 1016 ms | 1027 ms | 1.03 s | 523 MiB | 169.6 MiB | 5–9 |
| qld before | 8 | 262 ms | 284 ms | 1.28 s | 573 MiB | 169.6 MiB | 5–9 |
| qld before | 64 | 261 ms | 264 ms | 4.01 s | 623 MiB | 169.6 MiB | 5–9 |
| qld after | default | 202 ms | 204 ms | 1.44 s | 577 MiB | 169.6 MiB | 6–10 |
| qld after | 1 | 810 ms | 811 ms | 0.81 s | 521 MiB | 169.6 MiB | 6–10 |
| qld after | 8 | 217 ms | 221 ms | 1.06 s | 568 MiB | 169.6 MiB | 5–10 |
| qld after | 64 | 219 ms | 222 ms | 3.66 s | 622 MiB | 169.6 MiB | 5–10 |

### vmlinux

| linker | threads | wall min | wall median | CPU | peak RSS | output | load |
| --- | --- | --- | --- | --- | --- | --- | --- |
| gnu | default | 602 ms | 617 ms | 0.62 s | 197 MiB | 96.9 MiB | 5–7 |
| lld | default | 153 ms | 200 ms | 0.71 s | 325 MiB | 98.6 MiB | 5–7 |
| lld | 1 | 230 ms | 239 ms | 0.24 s | 331 MiB | 98.6 MiB | 5–7 |
| lld | 8 | 176 ms | 186 ms | 0.48 s | 328 MiB | 98.6 MiB | 5–7 |
| lld | 64 | 191 ms | 198 ms | 1.33 s | 323 MiB | 98.6 MiB | 5–7 |
| mold | default | fails: link failed:                                               ^ unknown l | | | | | |
| mold | 1 | fails: link failed:                                               ^ unknown l | | | | | |
| mold | 8 | fails: link failed:                                               ^ unknown l | | | | | |
| mold | 64 | fails: link failed:                                               ^ unknown l | | | | | |
| wild | default | fails: link failed: wild: error: unrecognized option(s): --emit-relocs | | | | | |
| wild | 1 | fails: link failed: wild: error: unrecognized option(s): --emit-relocs | | | | | |
| wild | 8 | fails: link failed: wild: error: unrecognized option(s): --emit-relocs | | | | | |
| wild | 64 | fails: link failed: wild: error: unrecognized option(s): --emit-relocs | | | | | |
| qld before | default | 190 ms | 199 ms | 1.03 s | 217 MiB | 98.6 MiB | 5–7 |
| qld before | 1 | 586 ms | 588 ms | 0.59 s | 145 MiB | 98.6 MiB | 5–7 |
| qld before | 8 | 201 ms | 204 ms | 0.76 s | 207 MiB | 98.6 MiB | 5–7 |
| qld before | 64 | 210 ms | 212 ms | 2.26 s | 219 MiB | 98.6 MiB | 5–7 |
| qld after | default | 148 ms | 149 ms | 0.70 s | 220 MiB | 98.6 MiB | 5–7 |
| qld after | 1 | 440 ms | 444 ms | 0.44 s | 146 MiB | 98.6 MiB | 5–7 |
| qld after | 8 | 152 ms | 157 ms | 0.57 s | 207 MiB | 98.6 MiB | 5–7 |
| qld after | 64 | 159 ms | 161 ms | 1.49 s | 221 MiB | 98.6 MiB | 5–7 |

### clang-debug

| linker | threads | wall min | wall median | CPU | peak RSS | output | load |
| --- | --- | --- | --- | --- | --- | --- | --- |
| gnu | default | 5981 ms | 6225 ms | 6.22 s | 1505 MiB | 1212.1 MiB | 5–14 |
| lld | default | 1325 ms | 1366 ms | 16.32 s | 4616 MiB | 1213.7 MiB | 4–13 |
| lld | 1 | 2484 ms | 2628 ms | 2.62 s | 4612 MiB | 1213.7 MiB | 5–8 |
| lld | 8 | 1034 ms | 1064 ms | 6.02 s | 4612 MiB | 1213.7 MiB | 5–13 |
| lld | 64 | 1668 ms | 1692 ms | 76.53 s | 4653 MiB | 1213.7 MiB | 5–13 |
| mold | default | 1121 ms | 1161 ms | 33.81 s | 5180 MiB | 1214.2 MiB | 5–13 |
| mold | 1 | 2080 ms | 2170 ms | 2.17 s | 4687 MiB | 1214.2 MiB | 5–7 |
| mold | 8 | 543 ms | 578 ms | 3.80 s | 4816 MiB | 1214.2 MiB | 7–13 |
| mold | 64 | 1250 ms | 1264 ms | 71.31 s | 5563 MiB | 1214.2 MiB | 7–13 |
| wild | default | 670 ms | 672 ms | 10.78 s | 4398 MiB | 1213.4 MiB | 7–17 |
| wild | 1 | 2526 ms | 2644 ms | 2.75 s | 4365 MiB | 1213.4 MiB | 4–7 |
| wild | 8 | 774 ms | 836 ms | 3.51 s | 4378 MiB | 1213.4 MiB | 7–17 |
| wild | 64 | 664 ms | 669 ms | 10.87 s | 4403 MiB | 1213.4 MiB | 7–17 |
| qld before | default | 1080 ms | 1113 ms | 10.69 s | 3664 MiB | 1213.7 MiB | 7–16 |
| qld before | 1 | 4417 ms | 4536 ms | 4.53 s | 3328 MiB | 1213.7 MiB | 4–7 |
| qld before | 8 | 1212 ms | 1294 ms | 7.17 s | 3448 MiB | 1213.7 MiB | 7–16 |
| qld before | 64 | 1144 ms | 1198 ms | 34.45 s | 3768 MiB | 1213.7 MiB | 7–15 |
| qld after | default | 837 ms | 854 ms | 9.08 s | 3791 MiB | 1213.7 MiB | 7–15 |
| qld after | 1 | 3532 ms | 3685 ms | 3.68 s | 3336 MiB | 1213.7 MiB | 4–6 |
| qld after | 8 | 946 ms | 964 ms | 5.97 s | 3451 MiB | 1213.7 MiB | 7–15 |
| qld after | 64 | 895 ms | 910 ms | 31.56 s | 3896 MiB | 1213.7 MiB | 7–14 |

## Determinism

After W26, every benchmark links to the same bytes with qld at 1, 2, 8 and
64 threads, and to the same bytes as the tree W26 started from (checked
after every W26 commit with `run.py --determinism` and the hashes saved
before W24's changes).

After W24, every benchmark, with qld at 1, 2, 8 and 64 threads:

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

## Optimizations (W24)

Each is one commit, with its measurements in the commit message (A/B runs
of the previous and the new binary, interleaved; stage laps from
`QLD_TIMING=1`).

| Commit | Change | Effect (min wall or stage lap) |
| --- | --- | --- |
| f1310bc | Lock-free merge-section deduplication (pieces bucketed by shard, one task per shard); parallel offset assignment for groups without tail merging | clang-debug merge 425 → 195 ms; link 1212 → 946 ms |
| d479fa1 | Relocation targets: section index without building an error `Result` | clang, one thread: 753 → 686 ms |
| 70743b2 | Archive symbol indexes read in parallel after the input walk | clang inputs 27 → 14 ms |
| 6e963fa | Layout: members found and sized in parallel, bucketed ordering, early exit of the entry-size scan | clang layout 20.5 → 9.4 ms |
| 7dadc4e | `.dynstr` names added in bulk (parallel hash, dedup and copy); parallel GNU hash ordering and output emptiness | clang dynamic 23 → 16 ms |
| 25d55ca | Output section flags folded per file in parallel | clang placement 17.7 → 11.4 ms |
| 881a1f3 | Default thread count from the uncompressed input size | W23's synthetic 320 MiB `--build-id` link 905 → 169 ms |
| 99c944a | Faster portable SHA-1 (one loop per round function) | `vmlinux`, one thread: write 381 → 324 ms |
| 31f03fa | Relocation lookups: `Target` always inlined (no store-forwarding stall), section kinds in a dense vector | clang, one thread: 677 → 624 ms |
| 79778ea | Per-thread hint for the last merge piece found | clang-debug, one thread: 4322 → 3795 ms |
| 59dd42a | Merge string ends found a word at a time | clang-debug resolution 121 → 103 ms |
| 5a9ea23 | Symbol interning's linear steps parallel from 2^17 names | clang resolution 53 → 47 ms |
| 37ae6aa | `--gc-sections` marks without building the section graph (unless `--why-live`) | libclang-cpp gc 12.2 → 8.5 ms |
| 8b1de35 | COMDAT signatures hashed once, while parsing | clang, one thread: resolution 171 → 165 ms |
| befd466 | Live merge sections found from the dense section vectors | clang merge 6.6 → 3.9 ms |
| b50f406 | `.strtab` written in parallel from the plan's offsets | clang write, median 58 → 55 ms |

Tried and not kept (no gain beyond noise, or slower somewhere): a lock-free
pass 1 for symbol interning (faster at 16 threads on clang's first round,
slower on one thread), numbering new symbols by job order instead of
sorting, building the write chunk list in parallel, a single writer
thread fed by the rendering workers, reusing region buffers per thread,
1,024 merge shards instead of 256, carrying piece hashes in the merge
buckets, a further SHA-1 variant, and the mapped output backing at one
thread.

## Where qld's time went after W24

Stage laps (`QLD_TIMING=1`, two runs at load 3) at default threads, after
the optimizations:

| stage | clang | clang-debug |
| --- | --- | --- |
| inputs | 13 ms | 18 ms |
| resolution | 49–51 ms | 98 ms |
| placement | 12 ms | 13 ms |
| scan | 8–10 ms | 9 ms |
| merge | 4–5 ms | 186–192 ms |
| dynamic | 15 ms | 12 ms |
| layout | 11–13 ms | 9 ms |
| write | 56–57 ms | 414–423 ms |
| after the write: freeing, unmapping the inputs, exit | 29–34 ms | 150–157 ms |

Threads doing work over time (perf samples, 5 ms buckets) show qld's
default 16 threads mostly busy from input loading to the relocation scan;
the gaps are the write (below), process start and exit, and short serial
steps between stages. Beyond 12–16 threads little changes: clang 204 ms
at 12 threads, 204 at 16 and 217 at 32; clang-debug 861, 829 and 818.

- **Process teardown**: unmapping the inputs (2.5 GiB of mapped pages for
  clang-debug) takes about as long whether qld unmaps them or the kernel
  does at exit (measured both ways). mold and wild avoid the wait by
  forking: the parent exits as soon as the output is complete. qld could do
  the same by running the link in a child process started from `main`
  (without `unsafe`: `std::process` to start it, a Unix socket for the
  child to report that the output is complete and with which status),
  which is a change to the frozen `src/main.rs`; the gain is about the last
  row above.
- **Write**: rendering is parallel, but buffered writes to one file are
  serialized by the kernel: a test program writing 1 GiB in 1 MiB pieces
  to this btrfs got 3.8 GiB/s from one thread and 3.6 GiB/s from 16, and a
  mapped file was slower (2.1–2.7 GiB/s). The 1.2 GiB copy into the page
  cache is most of clang-debug's write (wild spends 485 ms in its single
  flush).
- **Resolution** (clang: 49 ms against about 15 ms for wild's loading and
  resolution): interning 298,000 archive index names in the first round
  (hash probes under shard locks, then numbering new names by first
  occurrence), loading and COMDAT claims in later rounds. Deterministic,
  dense symbol IDs by first occurrence are the costly part.
- **Merge** (clang-debug): 18.1 million `.debug_str` pieces; the shard
  tasks' hash-table work is 132 ms at 16 threads, dominated by cache misses
  on piece bytes and tables (wild's string merging takes 181 ms).
- **64 threads**: qld's CPU time triples from 16 to 64 threads for the same
  work: at 64, a third of its samples are rayon workers looking for work
  (`Stealer::steal`, epoch advancing) between parallel steps, and page
  fault and allocator contention grows (clang-debug: 5 s of system time at
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
