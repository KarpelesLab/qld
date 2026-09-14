# OpenSSL

- **Version:** 3.6.4 (`https://github.com/openssl/openssl/releases/download/openssl-3.6.4/openssl-3.6.4.tar.gz`)
- **Script:** `tests/projects/openssl.sh QLD SCRATCH [gnu]`
- **Commands:** `./Configure CC=$SCRATCH/bin/qcc`, `make -j64`,
  `make test HARNESS_JOBS=64`

The default configuration builds `libcrypto.so.3` and `libssl.so.3` with
version scripts (`-Wl,--version-script=libcrypto.ld`, symbol versions
`OPENSSL_3.0.0` … `OPENSSL_3.6.0`), the `legacy` provider and the engines as
modules (`-z defs -z nodelete -Bsymbolic` with their own version scripts),
and 355 test programs.

## Result

| | qld | GNU ld 2.46.1 |
| --- | --- | --- |
| `make test` | PASS: 355 files, 4555 tests | PASS |
| Time | configure 4 s, make 8 s, test 39 s | configure 8 s, make 14 s, test 41 s |

360 executables and shared objects, all linked by qld.

`elfdiff-tree.sh` against the GNU ld build: all 360 have identical
`.dynsym` (names, versions, types, bindings, visibility) and `DT_*` entries
(`NEEDED`, `SONAME`, `VERDEF`/`VERNEED` counts, flags).

## Bugs found

1. **`providers/legacy.so` imported MD5, RC4 and DES from libcrypto**
   (fixed, commit `elf: archive members before a shared library win, as in
   GNU ld`). The module is linked from `providers/liblegacy.a` and
   `providers/libcommon.a` *before* `-lcrypto`; GNU ld extracts the archive
   members that define `MD5_Init`, `RC4_set_key`, `DES_ncbc_encrypt`…, qld
   bound them to `libcrypto.so.3` (`MD5_Init@OPENSSL_3.0.0` imports and
   PLT relocations). Regression test: `archive_before_shared_library_is_extracted`.
2. **`__timezone` missing from `.dynsym`** of `asn1_time_test` and
   `ca_internals_test` (fixed, commit `elf: import the strong alias of an
   imported weak data symbol`). A reference to glibc's weak `timezone` makes
   GNU ld import its strong alias `__timezone` too. Regression test:
   `weak_data_imports_bring_their_strong_alias`.
