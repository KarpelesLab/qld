# LLVM, clang and lld

- **Version:** 23.1.1 (`llvm-project-23.1.1.src.tar.xz` from the GitHub release)
- **Script:** `tests/projects/llvm.sh QLD SCRATCH [static|shared] [ninja targets...]`
- **Configure:** `cmake -G Ninja llvm -DCMAKE_BUILD_TYPE=Release
  -DCMAKE_C_COMPILER=clang -DCMAKE_CXX_COMPILER=clang++ (system clang 22)
  -DLLVM_USE_LINKER=$SCRATCH/bin/ld -DLLVM_ENABLE_PROJECTS="clang;lld"
  -DLLVM_TARGETS_TO_BUILD=X86 -DLLVM_ENABLE_ASSERTIONS=OFF
  -DLLVM_INCLUDE_BENCHMARKS=OFF [-DBUILD_SHARED_LIBS=ON]`
- **Build:** `ninja -j64`; **tests:** `ninja check-llvm`, `ninja check-clang`,
  `ninja check-lld` (full suites, `LIT_OPTS="-sv"`)
- `RELINK=1` deletes every linked output of an existing build so that ninja
  relinks them with the current qld (ninja does not track the linker).

## Result

| | static (default) | `BUILD_SHARED_LIBS=ON` |
| --- | --- | --- |
| Outputs linked by qld | 132 | 304 (incl. 170 shared libraries) |
| Build time (from scratch) | 220 s | 181 s |
| `check-llvm` | 38628 passed, 59 xfail, 36742 unsupported, 408 skipped, 0 failed | 38625 passed, 0 failed |
| `check-clang` | 48078 passed, 27 xfail, 0 failed | 48073 passed, **1 failed** |
| `check-lld` | 2063 passed, 1156 unsupported, 0 failed | 2063 passed, 0 failed |
| Check time | 99 s | 158 s |

The many unsupported tests need targets other than X86.

The single failure in the shared build, `Clang :: Driver/amdgpu-toolchain.c`,
is not a linker issue: its `RELO-NOT: -shared` check matches the build
directory's name (`.../llvm-23.1.1-shared/bin` in clang's `InstalledDir:`
line). It passes in the static build, whose directory name has no
`-shared`.

## Dynamic symbol tables against GNU ld

With `QLD_LINK_COMPARE`, every output was linked a second time with GNU ld
2.46.1 from the same inputs and compared with `elfdiff.py`:

- static build: 137 outputs of all sizes (and 181 over 1 MiB in the final
  run), all identical in `.dynsym` and `DT_*`;
- shared build: 309 outputs, all identical.

GNU ld could not link `bin/clang-repl`: LLVM's CMake adds `-Wl,--long-plt`
when the linker accepts it, qld accepts the (ARM-only) option and GNU ld on
x86-64 rejects it, so that one link was not compared.

Relocations differ in expected ways: in executables qld relaxes
initial-exec TLS accesses to exported symbols to local-exec where GNU ld
keeps `R_X86_64_TPOFF64` dynamic relocations, and GNU ld keeps an unused
`__tls_get_addr` PLT entry. `libLTO.so` has 3 fewer `R_X86_64_RELATIVE`
relocations: `gcdiff.py` shows GNU ld keeping `.data.rel.ro..Lconstinit`
sections of a different COMDAT copy (the documented COMDAT selection
difference), not a missing relocation.

## Bugs found

1. **`PT_TLS` did not start on its alignment** (fixed, commit
   `elf: start PT_TLS on its alignment`). `libLLVMSupport.a` has a 4-byte
   `.tdata` followed by an 8-aligned `.tbss`; the TLS segment started 4 bytes
   past an 8-byte boundary, and glibc, which places the block by
   `p_vaddr % p_align`, disagreed with every `@tpoff` qld computed.
   `DebugInfoPDBTests` crashed in `timeTraceProfilerBegin` and 14 `dsymutil`
   tests aborted (16 `check-llvm` failures). Test: `tls_segment_starts_aligned`.
2. **Executable symbols used by indirect dependencies were not exported**
   (fixed, commit `elf: export executable symbols that indirect dependencies
   use`). In the shared build, tools define C++ template instances that
   `libLLVMCore.so` also defines; GNU ld loads the `DT_NEEDED` dependencies
   of the libraries on the command line and exports such definitions (so
   there is one copy at run time), qld did not look past the command line.
   The same gap made a callback that an indirect dependency calls in the
   executable fail with "symbol lookup error". Test:
   `transitive_dependencies_see_executable_definitions`.
3. **Undefined symbols only dead code used stayed in a shared object's
   `.dynsym`** with `--gc-sections` (fixed, commit `elf: leave undefined
   symbols only dead code uses out of a shared object's .dynsym`): the pass
   plugins listed `llvm::DisableABIBreakingChecks`. Test:
   `gc_sections_drops_imports_only_dead_code_uses`.

## Link time

See the table in [README.md](README.md#performance).
