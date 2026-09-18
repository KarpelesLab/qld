# musl

The M1 item "musl C programs verified" and the M2 musl `ld.so` checks.

- **musl version:** 1.2.5, built from source into
  `$SCRATCH/build/musl-prefix` with `--syslibdir=$prefix/lib` (so its
  dynamic linker, `ld-musl-x86_64.so.1`, is found without installing into
  `/lib`). musl itself is built with the system linker: this checks qld's
  outputs against musl, not a musl built by qld.
- **Script:** `tests/projects/musl.sh QLD SCRATCH` (`JOBS=n` limits make's
  parallelism)
- **Compiler:** the `musl-gcc` wrapper from that build, plus `-B$SCRATCH/bin`
  for qld and `-fuse-ld=bfd` for GNU ld
- **Time:** musl build 7 s (first run), tests 3 s

## What runs

1. `libqldtest.so` (TLS variable, exported data, a weak function, a
   constructor) and `libplugin.so` (depends on it), then a program that
   starts a thread using the library's TLS, `dlopen`s the plugin, calls
   back into the executable through a function pointer, and reads the
   library's data:
   - PIE (`-fPIE -pie`),
   - non-PIE (`-fno-PIE -no-pie`; copy relocation for `lib_data`),
   - PIE with `-z now -z pack-relative-relocs`.
2. Three C programs, each linked by qld and by GNU ld from the same
   objects in four ways: static (`-static -no-pie`, `crt1.o` and
   `libc.a`), static PIE (`-static-pie`, `rcrt1.o`: musl relocates itself
   with the `R_X86_64_RELATIVE` relocations qld writes), PIE and non-PIE
   dynamic under musl's `ld.so`. Each pair must print the same output and
   exit with the same status; the script also notes any difference in the
   list of program headers.
   - `basics`: `qsort`, `printf`/`snprintf` with floating point, `strtod`,
     `libm`, `malloc`/`realloc` of megabytes, `setjmp`/`longjmp`, a signal
     handler, an undefined weak function, `argv`.
   - `threads`: four `pthread`s updating `__thread` and `_Thread_local`
     variables (initialized and zero-initialized TLS), a constructor, a
     destructor and `atexit`.
   - `files`: `mkstemp`/`fdopen`/`fgets`, `errno` and `strerror`,
     `setenv`/`getenv`, `clock_gettime` (vDSO), a non-zero exit status.
3. zlib 1.3.1 (`CC="musl-gcc -B$SCRATCH/bin" ./configure; make; make test`):
   the static `example`, the shared `examplesh` against `libz.so.1`, and
   `example64`.

## Result

All pass. The dynamic programs print `42 8 41 16 16`; all 12 program pairs
of step 2 print the same as GNU ld's and have the same program headers;
zlib reports `*** zlib test OK ***`, `*** zlib shared test OK ***` and
`*** zlib 64-bit test OK ***`. `examplesh` has
`interpreter: .../musl-prefix/lib/ld-musl-x86_64.so.1` and
`NEEDED libz.so.1`, `NEEDED libc.so`.

musl's loader binds every symbol at load time (no lazy binding) and
handles `DT_RELR` since 1.2.4, so the `-z pack-relative-relocs` case is
meaningful on 1.2.5.

## Bugs found

None.
