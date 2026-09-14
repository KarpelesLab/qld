# LTO builds (M6)

The M6 exit criterion: `gcc -flto` and `clang -flto` / `-flto=thin` builds
of the M2 project set link with qld and pass their test suites.

- **Scripts:** `tests/projects/lto.sh PROJECT QLD SCRATCH [gcc|clang|thin] [qld|gnu]`
  (zlib, lua, curl, OpenSSL, coreutils, Python),
  `tests/projects/rust-lto.sh QLD SCRATCH` (rustc `-C linker-plugin-lto`),
  `LLVM_LTO=Thin tests/projects/llvm.sh QLD SCRATCH static [targets]`
- **Compilers:** gcc 15.3.0 (`-flto=auto`, `gcc-ar`), clang 22.1.8
  (`-flto` / `-flto=thin`, `llvm-ar`) with its `LLVMgold.so`; rustc 1.98.0
  (LLVM 22.1.8) with the same clang. The fixtures also pass with gcc 13 and
  14 and clang 18, 19 and 20.
- **How LTO reaches qld:** the compiler driver passes `-plugin` and its
  `-plugin-opt`s. The scripts count the IR objects ("IR check": every
  object is IR, except assembly sources), and qld refuses IR it cannot hand
  to a plugin, so a successful link went through the plugin.

## Results

| Project | gcc -flto | clang -flto | clang -flto=thin |
| --- | --- | --- | --- |
| zlib 1.3.1 (`make test`: static, shared, 64-bit) | OK | OK | OK |
| lua 5.4.7 (`lua-5.4.7-tests`, `_U=true all.lua`) | final OK | final OK | final OK |
| curl 8.22.0 (`make test`) | 1629 of 1629 OK | 1629 of 1629 OK | 1629 of 1629 OK |
| OpenSSL 3.6.4 (`make test`) | PASS (4555 tests, 355 files) | PASS | PASS |
| coreutils 9.11 (`make check`) | 602 pass, 130 skip, 1 fail; gnulib 528 pass | same | same |
| Python 3.14.7 (`--with-lto --enable-shared`, `python -m test`) | 467 OK, 3 failed | 464 OK, 6 failed | 464 OK, 6 failed |
| LLVM, clang and lld 23.1.1 (`-DLLVM_ENABLE_LTO=Thin`, `check-lld`) | | | builds (132 outputs, 964 s); 2063 passed, 0 failed |
| Rust + C crate (`-C linker-plugin-lto`, C in a bitcode archive) | | | builds, runs, `cargo test` OK; the C function is inlined into Rust |
| qld's own unit tests (`-C linker-plugin-lto`) | | | 313 passed |

Every ELF output of these builds has `Linker: qld` in `.comment`. The
failures are the same with GNU ld 2.46.1 and the same compiler (checked by
building with `lto.sh ... gnu`):

- coreutils `tests/cp/reflink-auto.sh`: the scratch file system is btrfs,
  which has reflinks (see [coreutils.md](coreutils.md)).
- Python `test_tkinter`, `test_urllib`, `test_urllib2`: the environment (see
  [python.md](python.md)). With clang also `test_cext`, `test_cppext` (the
  `qclang` wrapper's `--ld-path` is an unused argument under `-Werror`) and
  `test_peg_generator` (it compiles an extension with Python's `-flto` but
  links it without, and both linkers reject bitcode then). The GNU ld build
  with the same compiler fails the same six.

Python's `--with-lto` with GCC compiles fat LTO objects
(`-ffat-lto-objects -flto-partition=none`), which qld claims through the
plugin rather than linking their native code.

### Resolutions against GNU ld

GCC's plugin writes the resolution the linker reported for every IR symbol
to its `-fresolution=` file. Replaying each logged `gcc -flto` link with
qld and with GNU ld and comparing the two files:

| Build | Links compared | Symbol resolutions | Differences |
| --- | --- | --- | --- |
| coreutils | 604 | 33866 | 0 |
| curl | 8 | 12222 | 0 |
| Python | 82 | 46946 | 0 |
| OpenSSL | 360 | 2573417 | 2 links |

(Links whose temporary inputs are gone, such as Python's test extensions,
were not compared.) The two OpenSSL links that differ, `endecode_test` and
`evp_extra_test`, report the same resolution for every symbol; 283 of them
come from a member of `providers/libcommon.a` under qld and from a member
of `libcrypto.a` under GNU ld, both of which define them. That is the
archive-order difference of
[compatibility.md](../../docs/compatibility.md#archive-resolution-order),
not an LTO difference.

The comparison found two real differences, now fixed: a definition that a
shared library in the link also has is `PREVAILING_DEF_IRONLY_EXP` in GNU
ld, and an `--as-needed` library that only IR refers to is kept for
unversioned symbols but not for versioned ones (so a reference to
`SHA512_Init@@OPENSSL_3.0.0` from IR is `UNDEF` until the generated code
needs `libcrypto.so.3` again).

### Flaky tests seen under load

The first runs shared the host with other builds and test suites:

- curl `1399` (`Curl_pgrsTime` expects about 2 s to have passed) and `3300`
  (thread pool; valgrind reports a thread's TLS block as possibly lost)
  failed once each; both pass 10 times out of 10 when rerun alone, in the
  qld build and in the GNU ld build, and the full suites above were run
  again alone.
- OpenSSL `70-test_quic_radix.t` timed out once (`poll_abort_blocking`), and
  passes when rerun.
- curl's four SOCKS-over-Unix-socket tests are skipped when the build
  directory is too deep for a 108-byte socket path: use a short scratch
  path to get all 1629.

## Link time against GNU ld

Replaying the logged links (best of 3, 64-core host; both linkers run the
same plugin, so code generation dominates). The last three columns come
from one run with `QLD_TIMING=1`, which shows where qld spends the time.

| Output | qld | GNU ld | qld: claims | qld: code generation | qld: everything else |
| --- | --- | --- | --- | --- | --- |
| libcrypto.so.3, gcc -flto=auto (984 IR objects) | 2.39 s | 2.21 s | 198 ms | 2255 ms | 43 ms |
| libcrypto.so.3, clang -flto | 20.23 s | 20.65 s | 206 ms | 23309 ms | 38 ms |
| libcrypto.so.3, clang -flto=thin | 1.87 s | 1.78 s | 199 ms | 1586 ms | 41 ms |
| libpython3.14.so.1.0, gcc fat LTO | 4.55 s | 4.34 s | 327 ms | 4227 ms | 64 ms |
| libpython3.14.so.1.0, clang -flto=thin | 5.13 s | 5.25 s | 108 ms | 4997 ms | 86 ms |
| libcurl.so.4.8.0, gcc -flto=auto | 1.09 s | 0.99 s | 95 ms | 952 ms | 18 ms |

LTO links take as long as GNU ld's, within the run-to-run variation of the
plugins' own parallel code generation: what qld saves in its part of the
link (tens of milliseconds) is small next to code generation, and the
claims (the plugins reading every IR symbol table, on one thread, as the
plugin interface requires) are the plugins' work under either linker.

LLVM with ThinLTO (the third table row above) is the largest of these
links: `ninja` builds every tool with `-flto=thin`, so each of the 132
outputs runs LLVM's ThinLTO backends through the plugin.
