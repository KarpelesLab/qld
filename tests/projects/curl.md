# curl

- **Version:** 8.22.0 (`https://curl.se/download/curl-8.22.0.tar.xz`)
- **Script:** `tests/projects/curl.sh QLD SCRATCH [gnu]`
- **Commands:** `./configure CC=$SCRATCH/bin/qcc --with-openssl --enable-shared --disable-static`,
  `make -j64`, `make -C tests -j64`, `make test TFLAGS="-j64 -a"` (local
  test servers only; no network)
- **TLS:** the system OpenSSL 3

## Result

| | qld | GNU ld 2.46.1 |
| --- | --- | --- |
| `make test` | 1629 of 1629 OK (2072 considered) | 1629 of 1629 OK |
| Time | configure 8 s, make 14 s, test 37 s | configure 8 s, make 14 s, test 42 s |

The tests that are not run need servers or features this host lacks
(SSH, SMB, HTTP/3, …); the list is the same with both linkers.

`elfdiff-tree.sh` against the GNU ld build: `libcurl.so.4.8.0`, `curl` and
the unit test programs have identical `.dynsym` and `DT_*` entries.

## Bugs found

1. **libtool builds static libraries only** (shared module, not fixed here).
   libtool's `_LT_LINKER_SHLIBS` decides that a GNU linker cannot build
   shared libraries unless `$LD --help` prints `supported targets: ... elf`.
   qld's `--help` has no such line, so the first build silently had
   `Shared=no` and tested a static libcurl. GNU ld, gold, lld
   (`ld.lld: supported targets: elf`) and mold all print one. The build
   scripts work around it in the `ld` wrapper; the fix belongs in
   `src/args/parse.rs` (see the W16 report). Every libtool-based build is
   affected: an autotools build of expat or jansson with qld as `ld`
   produces static libraries only, without an error.
2. **libgcc helpers bound to `libgcc_s.so.1`** (fixed, commit
   `elf: archive members before a shared library win, as in GNU ld`).
   gcc passes `-lgcc --push-state --as-needed -lgcc_s --pop-state`; qld let
   the shared library's `__popcountdi2@GCC_3.4` beat `libgcc.a`'s member,
   so `libcurl.so` and `units` gained `DT_NEEDED libgcc_s.so.1` and a PLT
   import GNU ld does not produce. Regression test:
   `tests/elf_link.rs` `archive_before_shared_library_is_extracted`.
