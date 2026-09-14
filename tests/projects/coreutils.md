# GNU coreutils

- **Version:** 9.11 (`https://ftp.gnu.org/gnu/coreutils/coreutils-9.11.tar.xz`)
- **Script:** `tests/projects/coreutils.sh QLD SCRATCH [gnu]`
- **Commands:** `./configure CC=$SCRATCH/bin/qcc`, `make -j64`,
  `make -k -j64 check`
- **Host:** Gentoo, gcc 15.3, glibc, 64 cores, btrfs

## Result

| | qld | GNU ld 2.46.1 |
| --- | --- | --- |
| `tests/` | 733: 605 PASS, 127 SKIP, 1 FAIL | identical |
| `gnulib-tests/` | 577: 528 PASS, 49 SKIP, 0 FAIL | identical |
| `lib/config.h` from configure | identical | |
| Time | configure 43 s, make 2 s, check 16 s | configure 39 s, make 3 s, check 13 s |

Every executable (1159 links, including the gnulib tests) has
`Linker: qld` in `.comment`.

The one failure, `tests/cp/reflink-auto.sh`, fails the same way with GNU ld:
it expects `cp --reflink=auto` to fall back on a file system without
reflinks, and the scratch directory is on btrfs, which has them. It is not
a linker issue.

The skips are tests that need root, SELinux, or specific file systems, and
are the same list with both linkers.

## Dynamic symbol tables against GNU ld

`elfdiff-tree.sh` over the two build trees: 589 of 604 executables are
identical in `.dynsym` and `DT_*` entries. The 15 others (`env`, `printenv`,
`sort`, `split`, `ginstall`, posix_spawn tests, …) differ only by
`_environ@GLIBC_2.2.5` being exported: they copy-relocate `environ`, and qld
exports every alias glibc defines at the copied address (lld does the same);
GNU ld exports only the referenced name and its strong alias `__environ`.
This is the documented "copy-relocated aliases share one copy (lld style)"
difference.

## Bugs found

None specific to coreutils.
