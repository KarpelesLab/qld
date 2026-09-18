# macOS `-fuse-ld` suite (M8)

The M8 exit criterion: Apple clang on macOS builds and runs a C, C++ and
Objective-C test suite with qld as the linker, on arm64 and x86_64. It has
two parts, both run by the `macho-macos` CI job.

## In-repo programs: `cargo test --test macho_link fuse_ld_suite`

Sources in `tests/data/macho_link/suite/`, driver in
`tests/macho_link/suite.rs`. Each program checks itself and prints
`<language> suite: N checks, 0 failures`.

| Program | Links | Covers |
| --- | --- | --- |
| `c_suite` | executable, `libsuite_c.dylib`, `c_plugin.bundle` (`-bundle_loader`) | constructors with priorities, rebases in `__DATA`/`__DATA_CONST`, a 1 MiB `__bss`, libm, `qsort` callbacks, varargs, `setjmp`/`longjmp`, dylib data and thread-local variables, weak definitions in both images, a missing weak import (`-U`), pthreads with TLVs, `dlopen` of a bundle calling back into the executable |
| `cxx_suite` | executable, `libsuite_cxx.dylib` | exceptions thrown across the dylib boundary both ways, `exception_ptr`, nested exceptions, `bad_variant_access`/`bad_any_cast`, `dynamic_cast`/`typeid` across images, virtual inheritance, one copy of an inline function's static local, iostreams, `<regex>`, `<filesystem>`, threads, `thread_local` with destructors (`_tlv_atexit`), static initialization order |
| `objc_suite` | executable (with Objective-C++), `libsuite_objc.dylib`, `libsuite_category.a` (`-ObjC`) | a subclass of a dylib's class, categories from the executable, the dylib and an archive member nothing references, `+load` in classes and categories, protocols, category properties, KVC, blocks, GCD, `@try`/`@finally` across images, ARC weak references, notifications, `@synchronized`, C++ ivars (`.cxx_construct`/`.cxx_destruct`), C++ exceptions through methods and `NSException` caught by C++ |

Each is built for arm64 and x86_64 (run under Rosetta), four ways:
default (chained fixups), `-Wl,-dead_strip`, `-mmacosx-version-min=11.0`
(legacy `LC_DYLD_INFO_ONLY`), and `-Wl,-objc_category_merging
-Wl,-dead_strip`. Objective-C method lists are relative (the default from
macOS 11) in every variant.

## Downloaded projects: `tests/projects/macos-suite.sh`

```sh
cargo build --release
tests/projects/macos-suite.sh target/release/qld /tmp/macos-suite        # all
QLD_SUITE_ARCHS=arm64 tests/projects/macos-suite.sh target/release/qld /tmp/macos-suite lua
```

| Project | Version | Build | Test |
| --- | --- | --- | --- |
| zlib | 1.3.1 | `configure`, static and shared | `make test` |
| Lua | 5.4.7 | `make macosx`; the test suite's C modules as bundles with `-undefined dynamic_lookup` | `lua -e"_U=true" all.lua` (`final OK`, C modules loaded) |
| SQLite | 3.46.1 amalgamation | shell static, with `-dead_strip`, and against `libsqlite3.dylib` | `macos-suite.sql` (WAL, triggers, window functions, JSON, FTS5, R*Tree, math); output identical to the same objects linked by Apple's `ld` |
| {fmt} | 12.2.0 | CMake, `BUILD_SHARED_LIBS=ON`, `FMT_TEST=ON` | `ctest` |

The compilers get `-fuse-ld=$SCRATCH/bin/ld64.qld`, a wrapper that records
each output path and runs `qld -flavor darwin`. After each build, every
Mach-O executable, dylib and bundle left in the tree (except Apple-linked
references) must be in that log.

## Results

CI run 35323375730 (`macos-latest`, Xcode 26.6, arm64 runner with Rosetta):

| Part | arm64 | x86_64 |
| --- | --- | --- |
| `fuse_ld_suite` (C, C++, Objective-C; default, `-dead_strip`, macOS 11) | 9/9 | 9/9 |
| zlib | PASS | PASS |
| Lua | PASS | PASS |
| SQLite | PASS | PASS |
| {fmt} | not run: 11.0.2's tests do not compile with Xcode 26's libc++; now 12.2.0 | same |

Fixed on the way: clang passes `-O<n>` to a linker given with
`-fuse-ld=<path>`; relative `-L` paths were looked up under `-syslibroot`
(`-L.` found the SDK's `libsqlite3.tbd`); legacy output did not write weak
bindings.
