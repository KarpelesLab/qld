# musl `ld.so`

- **musl version:** 1.2.5, built from source into
  `$SCRATCH/build/musl-prefix` with `--syslibdir=$prefix/lib` (so its
  dynamic linker, `ld-musl-x86_64.so.1`, is found without installing into
  `/lib`). musl itself is built with the system linker: this checks qld's
  outputs under musl's loader.
- **Script:** `tests/projects/musl.sh QLD SCRATCH`
- **Compiler:** the `musl-gcc` wrapper from that build, plus `-B$SCRATCH/bin`
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
2. zlib 1.3.1 (`CC="musl-gcc -B$SCRATCH/bin" ./configure; make; make test`):
   the static `example`, the shared `examplesh` against `libz.so.1`, and
   `example64`.

## Result

All pass: every program prints `42 8 41 16 16`, and zlib reports
`*** zlib test OK ***`, `*** zlib shared test OK ***` and
`*** zlib 64-bit test OK ***`. `examplesh` has
`interpreter: .../musl-prefix/lib/ld-musl-x86_64.so.1` and
`NEEDED libz.so.1`, `NEEDED libc.so`.

musl's loader binds every symbol at load time (no lazy binding) and
handles `DT_RELR` since 1.2.4, so the `-z pack-relative-relocs` case is
meaningful on 1.2.5.

## Bugs found

None.
