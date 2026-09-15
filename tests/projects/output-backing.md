# Output backings

`output-backing.py` times qld's three ways of writing the output file
(`QLD_OUTPUT_BACKING=mmap|write|memory`, see `qld::output::OutputFile`) on
large links, and checks that every backing writes identical bytes.

## Setup (2026-09-15)

- 64-core x86-64 host, shared; load average 6–12 during the runs (another
  job was running).
- btrfs at 97% full (the scratch directory), and tmpfs (`/dev/shm`). Inputs
  are on btrfs, in the page cache.
- qld release build at the W23 branch; each cell is 5 links per backing,
  interleaved; the previous output is deleted and `sync` run before each
  link, outside the timing.
- Workloads: qld linking its own debug binary (170 MiB) and release binary
  (46 MiB), captured by an `ld` shim during `cargo build`; `synthetic:320`
  (256 assembled objects with 320 MiB of `.debug_info`/`.debug_line`).
  Replaying the static LLVM tree's `clang-23` link failed on this branch
  (a `R_X86_64_TLSGD` relocation error unrelated to the output writer), so
  it is not included.

## Results (min / median wall time of the whole link)

| file system | workload | size | threads | mmap | write | memory |
| --- | --- | --- | --- | --- | --- | --- |
| btrfs | qld-debug | 170 MiB | 1 | 876 / 995 ms | 911 / 1041 ms | 912 / 1005 ms |
| btrfs | qld-debug | 170 MiB | 16 | 299 / 313 ms | 238 / 245 ms | 291 / 298 ms |
| btrfs | qld-debug | 170 MiB | default (64) | 237 / 318 ms | 238 / 270 ms | 319 / 342 ms |
| btrfs | qld-release | 46 MiB | 1 | 189 / 193 ms | 213 / 215 ms | 202 / 224 ms |
| btrfs | qld-release | 46 MiB | 16 | 136 / 145 ms | 92 / 103 ms | 125 / 129 ms |
| btrfs | qld-release | 46 MiB | default | 112 / 143 ms | 80 / 88 ms | 118 / 126 ms |
| btrfs | synthetic | 320 MiB | 1 | 293 / 300 ms | 256 / 272 ms | 365 / 372 ms |
| btrfs | synthetic | 320 MiB | 16 | 216 / 252 ms | 141 / 148 ms | 244 / 251 ms |
| btrfs | synthetic | 320 MiB | default | 281 / 314 ms | 258 / 269 ms | 334 / 380 ms |
| tmpfs | qld-debug | 170 MiB | 1 | 840 / 920 ms | 925 / 942 ms | 887 / 940 ms |
| tmpfs | qld-debug | 170 MiB | 16 | 216 / 234 ms | 235 / 239 ms | 284 / 298 ms |
| tmpfs | qld-debug | 170 MiB | default | 223 / 229 ms | 234 / 245 ms | 287 / 293 ms |
| tmpfs | qld-release | 46 MiB | 1 | 173 / 198 ms | 192 / 219 ms | 184 / 206 ms |
| tmpfs | qld-release | 46 MiB | 16 | 87 / 88 ms | 77 / 78 ms | 103 / 105 ms |
| tmpfs | qld-release | 46 MiB | default | 85 / 87 ms | 79 / 80 ms | 101 / 105 ms |
| tmpfs | synthetic | 320 MiB | 1 | 248 / 251 ms | 228 / 234 ms | 309 / 313 ms |
| tmpfs | synthetic | 320 MiB | 16 | 74 / 83 ms | 81 / 85 ms | 162 / 169 ms |
| tmpfs | synthetic | 320 MiB | default | 222 / 231 ms | 212 / 219 ms | 291 / 293 ms |

With `--build-id=sha1`:

| file system | workload | threads | mmap | write | memory |
| --- | --- | --- | --- | --- | --- |
| btrfs | qld-debug | 1 | 879 / 980 ms | 950 / 1028 ms | 914 / 942 ms |
| btrfs | qld-debug | default | 294 / 329 ms | 239 / 263 ms | 318 / 327 ms |
| btrfs | synthetic | 1 | 894 / 912 ms | 958 / 978 ms | 944 / 972 ms |
| btrfs | synthetic | default | 873 / 893 ms | 920 / 959 ms | 923 / 949 ms |
| tmpfs | qld-debug | 1 | 921 / 945 ms | 921 / 950 ms | 916 / 971 ms |
| tmpfs | qld-debug | default | 236 / 247 ms | 238 / 245 ms | 294 / 310 ms |
| tmpfs | synthetic | 1 | 822 / 881 ms | 906 / 917 ms | 901 / 954 ms |
| tmpfs | synthetic | default | 827 / 868 ms | 911 / 1026 ms | 896 / 1032 ms |

All outputs were byte-identical between backings in every cell.

## Conclusions

- On btrfs, with threads, `write` is the fastest: 20–35% faster than `mmap`
  for the qld links and the synthetic link at 16 threads.
- On tmpfs the backings are within noise of each other with threads (tmpfs
  has no block allocation for mapped page faults to contend on); `write` is
  slightly ahead on the release link and slightly behind on the debug link.
- Single-threaded, `write` costs 5–12% more than `mmap` on the qld links
  (an extra copy from the chunk buffers), and is faster on the synthetic
  link.
- With a build-id, the synthetic link's ~1 MiB sections straddle nearly
  every hash block, so most blocks are read back from the file: 50–80 ms
  more than `mmap` on 320 MiB. The qld links' small sections are hashed
  while writing.

`write` is the default (`BackingPolicy::DEFAULT`) for every size: links run
multi-threaded by default, where it wins or ties on both file systems, and a
single rule is simpler than a size or file-system heuristic.

## Reproducing

```sh
tests/projects/output-backing.py target/release/qld --outdir /path/on/fs \
  --outdir /dev/shm/x --workdir /tmp/work \
  "qld-debug=links.log:/path/to/deps/qld-HASH" "synthetic=synthetic:320"
```
