# Roadmap

Each milestone has a scope and **exit criteria**. A milestone is complete only
when all of its exit criteria pass in CI. The milestones are ordered so every
one produces a linker that is useful for something real. Later formats reuse
the infrastructure built for ELF.

Performance is a design constraint from M0 onwards, not a later phase. M5 is
about measuring and tuning, not about adding parallelism after the fact.

Legend: `[ ]` not started · `[~]` in progress · `[x]` done

---

## M0: Foundations

Set up the project skeleton and the infrastructure every later milestone needs.

- [x] Crate skeleton: one crate, module per pipeline stage (see
      [architecture.md](docs/architecture.md#module-layout)), `rust-version = "1.89"`,
      edition 2024, shared types (IDs, errors, diagnostics, target) in place
- [x] Work split into parallel workstreams ([workstreams.md](docs/workstreams.md))
- [x] CI: build and test on stable and 1.89, `clippy -D warnings`, `rustfmt --check`,
      `cargo doc`, `cargo deny` (licenses/advisories), Linux/macOS/Windows hosts
- [ ] Diagnostics framework: error/warning/note with input location, GNU-style
      `qld: error: ...` rendering, and structured form for library users
- [x] GNU flavor argument parser
  - [x] Complete option table covering every GNU ld 2.4x, gold, lld and mold
        option, each marked *implemented*, *accepted-ignored* or *unsupported*
  - [x] Single- and double-dash long options, joined/separate values, `-z` keywords
  - [x] Positional state: `--whole-archive`, `--as-needed`, `-Bstatic/-Bdynamic`,
        `--push-state/--pop-state`, `--start-group/--end-group`
  - [x] Response files (`@file`), `--sysroot` and `=`-prefixed paths
  - [x] `-v`/`--version` output that autoconf/libtool recognize as GNU-compatible
- [x] Flavor selection from `argv[0]` (`ld`, `ld.qld`, `ld64`, …) and `-flavor`
      (the ld64 parser itself is M8)
- [x] Concurrent symbol table with deterministic interning, and the
      order-independent archive extraction driver
- [x] Format-neutral GC, ICF and merge-section engines
- [x] Output writer: mapped output, parallel chunks, build-id
- [x] Input file loading: mmap, format detection by magic, `ar` archives
      (GNU, BSD, COFF and thin variants, symbol index), `-l` library search
- [x] Test harness: fixture compilation with the system gcc/clang,
      run-the-output tests, differential comparison against GNU ld
      ([testing.md](docs/testing.md))
- [ ] Fuzz targets for every parser (`cargo fuzz`) — deferred: needs a separate crate; randomized corruption tests meanwhile

**Exit criteria:** `qld --help` and `qld --version` work. All options on the
test corpus of real-world command lines (captured from gcc, clang and rustc
driver invocations) parse into the expected `LinkOptions`, and those options
serialize back. Archive symbol indexes are read correctly for GNU, BSD and thin
archives.

---

## M1: Static ELF on x86-64

The first end-to-end link: fully static x86-64 Linux executables.

- [x] ELF64 little-endian relocatable object parsing (zero-copy)
- [x] Symbol resolution: strong/weak/common/undefined; archive member extraction
      that does not depend on input order; COMDAT group deduplication
- [x] Section model: output section assignment from default rules, `SHF_MERGE`
      string/constant merging, `.init_array`/`.fini_array`/`.ctors` ordering
      (`SORT_BY_INIT_PRIORITY`)
- [x] `--gc-sections` with the usual roots (entry, `-u`, `KEEP`, init/fini arrays,
      `__start_`/`__stop_` references, `SHF_GNU_RETAIN`)
- [x] `.eh_frame` parsing, deduplicating CIEs, dropping FDEs of GC'ed sections,
      `.eh_frame_hdr` generation
- [x] x86-64 relocations for static links; GOT for `GOTPCREL`; `GOTPCRELX` relaxation
- [x] TLS: `PT_TLS`, local-exec, and IE/GD/LD/TLSDESC → LE relaxation
- [x] IFUNC in static binaries (`R_X86_64_IRELATIVE`, `__rela_iplt_start/end`)
- [x] Program headers: `PT_LOAD` (with `-z separate-code` default layout),
      `PT_TLS`, `PT_GNU_STACK`, `PT_GNU_PROPERTY`/`.note.gnu.property` (CET/IBT)
- [x] Linker-defined symbols (`_end`, `_etext`, `__bss_start`, `__ehdr_start`, …)
- [x] `--build-id` (parallel tree hash), `-s`/`-S`, `--strip-debug`
- [x] `--icf`, `-Map`, `--why-live`, `--print-gc-sections`
- [x] Compressed debug sections in inputs (`-gz`)
- [x] Musl C programs verified (static, static-PIE, PIE and dynamic, compared with GNU ld)
- [x] Parallel output writer: mapped output, disjoint slices per chunk,
      replacing an existing output atomically
- [x] Undefined-symbol diagnostics with `referenced by file.o:(.text+0x12)`

**Exit criteria:** Static C and C++ programs against glibc and musl link and run,
including exceptions, threads and TLS. The fixture suite passes. `readelf -a`
shows the output is well-formed. qld links a static build of itself and
the result passes the test suite.

---

## M2: Dynamic ELF on x86-64

Everything a typical Linux distribution build needs.

- [x] Shared object inputs: `.dynsym`, `DT_NEEDED`/`DT_SONAME`, symbol versions
      (`.gnu.version`, `.gnu.version_d`, `.gnu.version_r`)
- [x] Linker scripts as inputs (`libc.so` style `GROUP`/`AS_NEEDED`/`INPUT`)
- [x] Output kinds: PIE (`-pie`), non-PIE dynamic, shared objects (`-shared`)
- [x] PLT/GOT, lazy and `-z now` binding, `.plt.got`, IBT-enabled PLT
- [x] Copy relocations and canonical PLT entries for non-PIC executables
- [x] `.dynamic`, `.dynsym`, `.dynstr`, `.hash`/`.gnu.hash` (`--hash-style`)
- [x] `-z relro`, `-z now`, `PT_GNU_RELRO`, `PT_INTERP`, `--dynamic-linker`
- [x] `DT_RELR` (`-z pack-relative-relocs`), sorted `.rela.dyn`, `-z combreloc`
- [x] TLS in shared objects: GD/LD/IE with the dynamic TLS relocations
- [x] Symbol visibility, `--export-dynamic`, `--dynamic-list`, `--exclude-libs`,
      version scripts (`--version-script`), `-Bsymbolic`, `--no-undefined`,
      `--allow-shlib-undefined`
- [x] `--as-needed`, `-rpath`, `-rpath-link`, `--enable-new-dtags`, `$ORIGIN`
- [x] `--wrap`, `--defsym`, `--trace-symbol`, `-Map`, `--cref`,
      `--print-gc-sections`, `--why-live`
- [x] Relocatable output (`-r`) and `--emit-relocs` (`-r` limits: no non-COMDAT section groups, no `SHT_REL` inputs, no `--compress-debug-sections`)
- [x] Library symbol matching: suggest the missing `-l` flag, version mismatch
      hints, "did you mean" for near-miss names
      ([optimizations.md](docs/optimizations.md#intelligent-library-symbol-matching))

**Status: exit criteria met** (W16). coreutils 9.11, curl 8.22, OpenSSL 3.6,
Python 3.14 (shared and static), LLVM/clang/lld 23.1 (static and
`BUILD_SHARED_LIBS`), the rustc 1.98 stage-1 bootstrap with `tests/ui`, zlib,
and musl 1.2.5 shared-object tests all pass with qld as the linker; every
remaining test failure reproduces with GNU ld on the same machine. Build
scripts and notes are in `tests/projects/`.

**Exit criteria:** Used as the system linker (`-fuse-ld=qld`), qld builds and
passes the test suites of coreutils, curl, openssl, zlib, Python, Rust
(`rustc` bootstrap), and LLVM/clang. The shared objects it produces load in
glibc and musl `ld.so`. Differential tests against GNU ld show the same
dynamic symbol tables and `DT_*` entries, apart from documented differences.

---

## M3: Linker scripts, raw binary and embedded targets

- [x] Full GNU linker script language: `SECTIONS`, `MEMORY`, `PHDRS`, `ENTRY`,
      `PROVIDE`/`PROVIDE_HIDDEN`, `ASSERT`, `INCLUDE`, `INSERT AFTER/BEFORE`,
      `OVERWRITE_SECTIONS`, `/DISCARD/`, `KEEP`, `SORT_*`, `EXCLUDE_FILE`,
      `AT`/`AT>` load addresses, `>region`, `FILL`, `BYTE`/`LONG`/`QUAD`,
      location counter arithmetic and all built-in functions
- [x] `-T`, `--default-script`/`-dT` (`--verbose` does not dump the effective
      default script yet)
- [x] `-Ttext`/`-Tdata`/`-Tbss`/`--section-start`, `--image-base`
- [x] Raw binary output: `--oformat binary`, `OUTPUT_FORMAT(binary)`, gap fill
- [x] Binary input: `-b binary` / `--format=binary` with
      `_binary_<name>_start/_end/_size` symbols
- [x] Intel HEX and S-record output (`--oformat ihex`/`srec`)
- [x] `--no-relax`, `--nmagic`/`--omagic`, `-N`/`-n`
- [x] `-r` together with `-T`, and `--defsym` with full linker-script expressions

**Status: exit criteria met** (W19, verified by the integrator with
`tests/projects/kernel.sh` and `baremetal.sh`).

- **Kernel:** linux 7.2.5 `defconfig` builds with qld doing all six x86-64
  links (`vmlinux` through the kernel's own `SECTIONS` script with
  `--orphan-handling=error` and `--emit-relocs`, the vdso, and the compressed
  image); 43 allocated sections and all 238,897 symbol addresses match GNU
  ld's, and the bzImage boots in QEMU. 32-bit links (`-m elf_i386`) and `-r`
  still go to GNU ld: ELF32 is M4, and `-r` output of `vmlinux.o` makes
  objtool fail.
- **Bare metal:** a flash image (`MEMORY`, `AT>`, `KEEP`, `SORT`, fill, data
  commands, `NOLOAD`, `LOADADDR`, `ASSERT`, `/DISCARD/`) and a `PHDRS`
  executable match GNU ld's sections, segments and every symbol address, and
  the `binary`, `ihex` and `srec` outputs are byte-identical.

**Exit criteria:** The Linux kernel (x86-64, `vmlinux` and bzImage) builds and
boots in QEMU. A bare-metal x86-64 image built from a linker script matches
the section and symbol addresses GNU ld produces, and gives the same flat
binary.

---

## M4: More ELF architectures

In priority order. Each one needs its relocations, thunks and relaxations,
and TLS models.

- [x] **AArch64**: the static and dynamic relocation set, range extension
      thunks, PLT/GOT, all four TLS models with TLSDESC and the TLS
      relaxations, ADRP+LDR→ADRP+ADD and ADRP+ADD→NOP+ADR relaxations, BTI
      properties, `-z force-bti`, `-z pac-plt`, the Cortex-A53 843419 and
      835769 workarounds, `-r`, and script layout. TLSDESC is bound eagerly,
      as in lld: glibc and musl need no lazy TLSDESC PLT. Outstanding: erratum
      fixes under linker-script layout (the kernel uses them), a patch pool
      per 128 MiB of code
- [x] **RISC-V 64**: the full relocation set, PLT/GOT, TLS GD/IE/LE and
      TLSDESC with relaxation, linker relaxation with section shrinking (an
      architecture-neutral fixpoint in `src/elf/arch/shrink.rs`), `--relax-gp`,
      `__global_pointer$`, `.riscv.attributes` merging, `-r`, `--emit-relocs`;
      compared with lld symbolically, fixtures run under qemu in CI.
- [x] **RISC-V 32** (`elf32lriscv`, ILP32/ILP32F/ILP32D) on the same backend,
      with the word size as a parameter: 4-byte GOT and TLS entries, the `lw`
      PLT, `c.jal` relaxation, RV32 attribute merging; compared with lld and
      run under qemu-riscv32. Open for RISC-V: `DT_RISCV_VARIANT_CC`, and
      `PT_RISCV_ATTRIBUTES` under linker scripts
- [x] **i386**: GOT-relative relocations with GNU ld's GOT32X relaxations, `-z ibtplt`, the GNU TLS models and TLS descriptors with their relaxations, PLT and IFUNC; compared with GNU ld 2.46 and run natively
- [x] **ARM (32-bit)** (`armelf_linux_eabi`, ARMv7-A hard-float): the REL
      relocation set, ARM/Thumb interworking with `bl`↔`blx` rewriting,
      range-extension and interworking thunks, `.ARM.exidx` merging and
      `EXIDX_CANTUNWIND` synthesis with `PT_ARM_EXIDX`, build-attribute and
      float-ABI merging, PLT/GOT, all TLS models, IFUNC, `R_ARM_V4BX`;
      compared with lld symbolically, fixtures run under qemu. Outstanding:
      BE8 (needs big-endian ELF32), `-r` and `--emit-relocs`, mapping symbols
      for linker-generated code, GNU TLS descriptors, group relocations past
      G0
- [x] **x32** (`elf32_x86_64`): the x86-64 relocations and PLT in ELF32 with
      8-byte GOT entries, x32's TLS forms and `GOTPCRELX` relaxations, IFUNCs,
      copy relocations, IBT, `--emit-relocs`, `-z pack-relative-relocs`;
      compared with GNU ld 2.46 instruction for instruction, and with lld
      where lld can link x32
- [~] **PowerPC64 LE** (ELFv2): relocations including the Power10 prefixed
      forms, TOC and `.toc` relaxation, local entry points, `.plt.sec` call
      stubs that save r2, thunks (including TOC-saving and PC-relative
      ones), all TLS models with relaxations, `.glink`/PLT, IFUNCs, `-r`;
      compared with lld by meaning, fixtures run under qemu in CI.
      Outstanding: `_savegpr*`/`_restgpr*`, inline PLT sequences
      (`-fno-plt`/`-mlongcall`), multi-TOC, `DT_PPC64_OPT`, thunks under
      linker scripts
- [ ] **PowerPC64 BE** (ELFv1 with OPDs; needs big-endian ELF)
- [~] **LoongArch64**: the relocation set (including the extreme code model
      and ADD/SUB/ULEB128), PLT/GOT, all TLS models with TLSDESC and IE/TLSDESC
      relaxation, size-preserving relaxation, `-r`; compared with lld by
      meaning. Outstanding: shrinking relaxation (deleting `nop`s,
      `R_LARCH_ALIGN`), B26 thunks, ALIGN synthesis in `-r`; the fixtures run
      under qemu in CI
- [x] **s390x** (big-endian): the relocation set including the
      halfword-counted PC-relative forms, `GOTENT`/`GOTPCDBL`, the 32-byte PLT
      with lazy binding, IFUNC, all TLS models through `__tls_get_offset` with
      the `GDCALL`/`LDCALL` markers and their relaxations, `lgrl`→`larl`,
      eight-byte `.hash`, `--s390-pgste`; compared with GNU ld 2.42 function
      by function and run under qemu. Outstanding: `-r`, `--gdb-index` and
      `--debug-names` for big-endian output, `R_390_PLTOFF*`
- [x] Big-endian ELF and ELF32 handled through the same generic code
      (monomorphized, no run-time endianness checks on hot paths): the
      pipeline is generic over `ElfFormat` and chosen once in `elf::link`.
      ELF64/ELF32 little-endian and ELF64 big-endian are instantiated, the
      last exercised end to end by s390x; `Elf32Be` is only decoded in unit
      tests so far

**Status:** AArch64 is the second architecture. Validation without an arm64
machine: every fixture links with qld and with `aarch64-unknown-linux-gnu-ld`,
and the relocated code is compared symbol by symbol from `objdump -d` (all
symbols identical except padding splits in the static glibc links). The
`arm64-linux` CI job runs the fixtures natively on an `ubuntu-24.04-arm`
runner.

**Exit criteria:** For each architecture, the fixture suite passes under
`qemu-user`, and a Debian or Alpine userland package set builds with qld as
its linker.

---

## M5: Performance and optimization

- [~] Benchmark suite and dashboard: clang, chromium, rustc, the Linux kernel,
      and a large debug-info-heavy Rust binary. Compared against GNU ld, gold,
      lld, mold and wild on wall time, CPU time, peak RSS and output size.
      Capture/replay suite and results in
      [tests/projects/bench.md](tests/projects/bench.md) (clang, clang with
      debug info, `libclang-cpp.so`, `vmlinux`, a Rust debug binary). Missing:
      chromium, gold, `librustc_driver`, per-commit publishing.
- [~] Profiling passes on every pipeline stage (done, 16 speedups merged).
      Thread scaling from 1 to 64+ cores: qld stops scaling at 12–16 threads.
- [x] Identical code folding: `--icf=safe` (using `.llvm_addrsig`) and `--icf=all`
- [x] String tail merging (`-O2`)
- [x] Section ordering: `--symbol-ordering-file`, `--call-graph-profile-sort` (hfsort/C3 and cdsort), `--call-graph-ordering-file`, `--print-symbol-order`
- [x] Compressed debug sections: read and write zlib and zstd
      (`--compress-debug-sections`), in-crate codecs, parallel
- [x] `--gdb-index` (version 8, DIE scan when pubnames are absent) and `--debug-names` generation
- [x] Unlinking a large old output file in the background
- [x] Optional separate-debug output (`--separate-debug-file`) with
      `.gnu_debuglink`, following mold (GNU ld has no such option)

**Exit criteria:** On the benchmark suite, qld's wall-clock time is at or below
mold's and wild's on x86-64, on both 8-core and 64-core machines, with peak
RSS no higher than lld's. Output is identical across 1, 2 and N threads for
every benchmark.

**Status (W42, 2026-09-20, 32-core Threadripper, machine loaded):** met
against mold at default and 64 threads, not at 8 threads or on one thread;
wild still leads on every large link. Overlapping the dynamic, symbol-table
and `.eh_frame` stages and cheaper GOT/PLT planning took clang from 150 to
131 ms and libclang-cpp from 92 to 85 ms at default threads (a quiet-machine
A/B of the same branch: clang 112 → 101 ms), with 1.3% fewer instructions and
identical output at 1, 2, 8 and 64 threads. What is left, in order: kernel
time (0.34–0.40 s of a clang link at 16 threads against 0.10 s at 1, from
page faults and 12k `mprotect` calls — a `mallopt` call in `main.rs` is worth
~20 ms but needs `unsafe` in a frozen file), the write at its backing's floor
(~35 ms), slimmer `ObjectInput`/`InputSection`, and single-threaded
relocation work. Full tables: [tests/projects/bench.md](tests/projects/bench.md).

---

## M6: Link-time optimization

- [x] GNU linker plugin API host (`-plugin`, `-plugin-opt`, `--plugin-opt=`),
      dynamically loading `liblto_plugin.so` (GCC 13–15) and `LLVMgold.so`
      (LLVM 18–22): standalone `plugin::Session` API
- [x] Claim-file handling, symbol resolution reporting (`LDPR_*`), adding
      compiled objects back into the link, archives containing IR members
      (including archives with no symbol index)
- [x] ThinLTO options passthrough: jobs, cache directory and pruning policy,
      `thinlto-index-only` (distributed ThinLTO verified end to end)
- [x] Clean fallback and a clear diagnostic when an IR input has no plugin

**Status:** exit criteria met for zlib, lua, curl (1629/1629), OpenSSL (4555
tests), coreutils and Python (same failures as their GNU ld builds), LLVM +
clang + lld built entirely with ThinLTO (`check-lld` 2063/2063), and a Rust
crate with `-C linker-plugin-lto`. Not tried with LTO: the rustc bootstrap and
musl. Replaying every `gcc -flto` link of those projects and diffing the
plugin's `-fresolution=` files against GNU ld's matched on 2.6M+ resolutions,
apart from the documented archive-order difference.

**Exit criteria:** `gcc -flto` and `clang -flto` / `-flto=thin` builds of the M2
project set link and pass their test suites. The linker plugin is behind the
`plugin` cargo feature, on by default (cargo features are per package, not per
target, so the library carries it too; building with `--no-default-features`
drops the FFI entirely).

---

## M7: PE/COFF (MinGW flavor)

- [x] COFF object and archive parsing, `.drectve` linker directives
- [x] Short import libraries (MSVC/LLVM style) and long import libraries
      (GNU dlltool `.idata$N` objects); linking directly against `.dll` files
- [x] PE32+ (x86-64), PE32 (i386) and ARM64 PE32+ (thunks, packed and unpacked `.pdata`); ARM64EC/ARM64X refused
- [x] EXE and DLL output, `--out-implib`, `.def` files, `--export-all-symbols`,
      export and import tables, base relocations, TLS directory
- [x] x86-64 SEH: `.pdata`/`.xdata` handling (sorted); i386 SafeSEH (`.sxdata` handler table)
- [x] Auto-import and runtime pseudo-relocations (`--enable-auto-import`,
      `__RUNTIME_PSEUDO_RELOC_LIST__`)
- [x] Resources (`.rsrc` from windres objects), subsystem and OS version fields,
      `--dynamicbase`, `--nxcompat`, `--high-entropy-va`, deterministic timestamps
- [x] DWARF in PE for MinGW debugging

**Status:** qld links MinGW x86-64 and i386 executables and DLLs (checked
against GNU ld) and ARM64 images (checked against lld). The `pe-windows` CI
job runs the x86-64 and i386 images on a Windows runner, including SafeSEH
enforcement, and `pe-windows-arm64` runs the ARM64 images on Windows on ARM.
PE links honour the library options for in-memory inputs, in-memory output
and cancellation.

**Exit criteria:** A MinGW-w64 GCC and a clang cross toolchain can use qld to
build and run a C/C++ test suite under Wine and on a Windows CI runner. qld
builds a working `x86_64-pc-windows-gnu` Rust binary, and it runs.

---

## M8: Mach-O (ld64 flavor)

- [x] ld64 argv flavor: `-arch`, `-platform_version`, `-syslibroot`,
      `-framework`, `-dylib`, `-bundle`, `-dead_strip`, `-undefined`, `-exported_symbols_list`
- [x] Mach-O object parsing including `.subsections_via_symbols` atomization (reader)
- [x] `.tbd` text stubs (v1–v5) and dylib inputs; two-level namespace and re-export resolution, including implicit public re-exports
- [x] arm64 and x86_64: stubs, GOT, thunks (arm64), TLV
- [x] `LC_DYLD_CHAINED_FIXUPS` and legacy `LC_DYLD_INFO_ONLY` output
- [x] Compact unwind (`__unwind_info`) synthesis, `__eh_frame`
- [x] Ad-hoc code signature (`LC_CODE_SIGNATURE`), which arm64 macOS requires
- [x] STABS debug map (`N_OSO`) so `dsymutil` can find DWARF in object files
- [x] Objective-C / Swift sections handled correctly: selector stubs, relative method lists (default from macOS 11), category merging (`-objc_category_merging`)
- [x] `-r` (relocatable output; DWARF sections not carried over yet), C string and literal merging, `-init`, `-alias`, `-bundle_loader`, `-flat_namespace`
- [x] LTO through `libLTO` (full and thin, `-object_path_lto`, `-cache_path_lto`)
- [x] arm64e (ptrauth chained fixups `DYLD_CHAINED_PTR_ARM64E`/`_USERLAND24`, `__auth_stubs`, `__auth_got`); checked structurally, since stock macOS does not run third-party arm64e code
- [x] **Universal (fat) binaries**: link each slice in parallel from multiple
      `-arch` values and write the `fat_header`; accept fat objects, archives
      and dylibs as inputs by selecting the matching slice

**Exit criteria:** clang on macOS builds and runs a C, C++ and Objective-C
test suite on arm64 and x86_64 CI runners with `-fuse-ld=qld`. A Rust
`aarch64-apple-darwin` binary runs. A universal binary passes `lipo -info`
and runs natively on both architectures.

**Status:** everything above except arm64e and LTO is implemented.
`tests/macho_link.rs` compares against `ld64.lld` on Linux, and the
`macho-macos` CI job links C, C++ (exceptions, TLV), Objective-C and Rust
(std: threads, unwinding, TLS) programs with Apple clang and rustc against
the real SDK and runs them on arm64 and, under Rosetta, x86_64, including a
universal binary. `-r` output is checked against Apple's `ld -r` there.
The broader `-fuse-ld` suite runs there too: 18 self-checking C, C++ and
Objective-C programs in three variants per arch, and zlib, Lua, SQLite and
{fmt} built with `-fuse-ld=qld` running their own tests, with every image
checked to be linked by qld. Mach-O LTO goes through Xcode's libLTO. Open:
arm64e.

---

## M9: Library API stabilization and 1.0

- [~] Public API review ([library-api.md](docs/library-api.md)); semver guarantees.
      Review done ([api-review.md](tests/projects/api-review.md)): the 1.0 surface is
      the crate root, `args`, `diag`, `error` and `target`; other modules are
      `#[doc(hidden)]`. W41 settled the blockers: `default()` is `new()`,
      options are enums, one input list for every format, no printing or
      environment reads in the library, and a caller's thread pool is used as
      it is. The smaller items left are listed at the end of
      [api-review.md](tests/projects/api-review.md).
- [~] In-memory inputs and outputs (`InputKind::bytes`, `MemoryFiles`,
      `link_to_memory`), caller-provided thread pool, cancellation
      (`CancelToken`, `Error::Cancelled`): done for ELF, PE and Mach-O
- [~] Complete rustdoc with examples (crate-level and `link` examples, five
      programs in `examples/`); published on crates.io
- [~] Packaging: prebuilt binaries, distribution packages, `ld.qld` and `ld64.qld` symlinks.
      Release workflow (`.github/workflows/release.yml`, tags only) and recipes in
      `packaging/` (Gentoo, Arch, Debian, Homebrew) are in; the first tagged release is pending

**Exit criteria:** M1–M8 exit criteria still pass. The API has had no breaking
changes for one release cycle.

---

## Later / under consideration

These are not scheduled. Each needs a concrete use case before it moves into
a milestone.

- MSVC `link.exe` / `lld-link` flavor with CodeView and PDB output
- WebAssembly (`wasm-ld` flavor)
- Incremental relinking (reusing layout from a previous link)
- Profile-guided function layout from perf/`propeller` data
- Mach-O Objective-C category merging, `-order_file`, chained-fixup-aware ICF

## Non-goals

- Being a runtime dynamic loader (`ld.so`/`dyld`); qld is a build-time linker
- Byte-identical output to GNU ld. qld aims for compatible *behavior*, and every
  intentional difference is documented in [compatibility.md](docs/compatibility.md).
- Legacy formats GNU ld supports through BFD (a.out, XCOFF, ECOFF, SOM, …)
