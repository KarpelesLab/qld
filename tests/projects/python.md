# CPython

- **Version:** 3.14.7 (`https://www.python.org/ftp/python/3.14.7/Python-3.14.7.tar.xz`)
- **Script:** `tests/projects/python.sh QLD SCRATCH [qld|gnu] [shared|static]`
- **Commands:** `./configure CC=$SCRATCH/bin/qcc CXX=$SCRATCH/bin/qc++ [--enable-shared]`,
  `make -j64`, `./python -m test -j64 --timeout 1800 -u all,-network,-urlfetch,-largefile`

Both configurations build 77 extension modules as shared objects (plus the
`_testcext`/`_testcppext` modules the test suite builds with the compiler);
`--enable-shared` also builds `libpython3.14.so.1.0`.

## Result

| | qld shared | qld static | GNU ld shared | GNU ld static |
| --- | --- | --- | --- | --- |
| Modules | 77 shared, 0 failed on import | same | same | same |
| Test files | 467 OK, 3 failed, 16 skipped, 6 resource denied | same | same | same |
| Tests | 50508 run, 5 failures | 50508, 5 | 50508, 5 | 50508, 5 |
| Time (configure, make, test) | 16 s, 13 s, 53 s | 14 s, 10 s, 59 s | 14 s, 10 s, 72 s | 14 s, 11 s, 54 s |

The three failing test files fail identically with GNU ld:

- `test_urllib`, `test_urllib2`: the sandbox sets proxy environment
  variables, so `urlopen` goes through a tunnel and rejects the test hosts
  ("Tunnel host can't contain control characters").
- `test_tkinter`: a real X display is present; one test fails with
  "grab failed: another application has grab" (with both linkers, on every
  rerun of the test alone). Under `-j64` load it sometimes fails one more
  subtest with either linker.

## Dynamic symbol tables against GNU ld

- `--enable-shared`: all 84 executables and shared objects identical.
- default: 77 of 81 identical; `python`, `_bootstrap_python`,
  `_freeze_module` and `_testembed` export `_environ` in addition, the
  documented copy-relocated alias difference (see coreutils.md).

## Bugs found

The first comparison showed three differences, all fixed:

1. `libpython3.14.so`, `_bootstrap_python`, `_freeze_module` and
   `_testinternalcapi.so` imported `__popcountdi2@GCC_3.4` from
   `libgcc_s.so.1` (and gained a `DT_NEEDED` on it); see curl.md.
2. `__environ` missing from `.dynsym` next to `environ`; see openssl.md
   (weak data alias).
3. `_testembed` exported `_GLOBAL_OFFSET_TABLE_` with `-rdynamic`; fixed on
   master (9b75c7c) while this work was in progress.
