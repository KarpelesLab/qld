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
- [ ] Compressed debug sections in inputs (`-gz`), waiting on W10
- [ ] Musl C programs verified (only the Rust musl target is tested so far)
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

- [ ] Shared object inputs: `.dynsym`, `DT_NEEDED`/`DT_SONAME`, symbol versions
      (`.gnu.version`, `.gnu.version_d`, `.gnu.version_r`)
- [~] Linker scripts as inputs (`libc.so` style `GROUP`/`AS_NEEDED`/`INPUT`): parsing done, driver hookup pending
- [ ] Output kinds: PIE (`-pie`), non-PIE dynamic, shared objects (`-shared`)
- [ ] PLT/GOT, lazy and `-z now` binding, `.plt.got`, IBT-enabled PLT
- [ ] Copy relocations and canonical PLT entries for non-PIC executables
- [ ] `.dynamic`, `.dynsym`, `.dynstr`, `.hash`/`.gnu.hash` (`--hash-style`)
- [ ] `-z relro`, `-z now`, `PT_GNU_RELRO`, `PT_INTERP`, `--dynamic-linker`
- [ ] `DT_RELR` (`-z pack-relative-relocs`), sorted `.rela.dyn`, `-z combreloc`
- [ ] TLS in shared objects: GD/LD/IE with the dynamic TLS relocations
- [ ] Symbol visibility, `--export-dynamic`, `--dynamic-list`, `--exclude-libs`,
      version scripts (`--version-script`), `-Bsymbolic`, `--no-undefined`,
      `--allow-shlib-undefined`
- [ ] `--as-needed`, `-rpath`, `-rpath-link`, `--enable-new-dtags`, `$ORIGIN`
- [ ] `--wrap`, `--defsym`, `--trace-symbol`, `-Map`, `--cref`,
      `--print-gc-sections`, `--why-live`
- [ ] Relocatable output (`-r`) and `--emit-relocs`
- [ ] Library symbol matching: suggest the missing `-l` flag, version mismatch
      hints, "did you mean" for near-miss names
      ([optimizations.md](docs/optimizations.md#intelligent-library-symbol-matching))

**Exit criteria:** Used as the system linker (`-fuse-ld=qld`), qld builds and
passes the test suites of coreutils, curl, openssl, zlib, Python, Rust
(`rustc` bootstrap), and LLVM/clang. The shared objects it produces load in
glibc and musl `ld.so`. Differential tests against GNU ld show the same
dynamic symbol tables and `DT_*` entries, apart from documented differences.

---

## M3: Linker scripts, raw binary and embedded targets

- [~] Full GNU linker script language (parser, evaluator and pattern matching done; layout integration pending): `SECTIONS`, `MEMORY`, `PHDRS`, `ENTRY`,
      `PROVIDE`/`PROVIDE_HIDDEN`, `ASSERT`, `INCLUDE`, `INSERT AFTER/BEFORE`,
      `OVERWRITE_SECTIONS`, `/DISCARD/`, `KEEP`, `SORT_*`, `EXCLUDE_FILE`,
      `AT`/`AT>` load addresses, `>region`, `FILL`, `BYTE`/`LONG`/`QUAD`,
      location counter arithmetic and all built-in functions
- [ ] `-T`, `--default-script`, `--verbose` (dump the effective default script)
- [ ] `-Ttext`/`-Tdata`/`-Tbss`/`--section-start`, `--image-base`
- [ ] Raw binary output: `--oformat binary`, `OUTPUT_FORMAT(binary)`, gap fill
- [ ] Binary input: `-b binary` / `--format=binary` with
      `_binary_<name>_start/_end/_size` symbols
- [ ] Intel HEX and S-record output (`--oformat ihex`/`srec`)
- [ ] `--no-relax`, `--nmagic`/`--omagic`, `-N`/`-n`

**Exit criteria:** The Linux kernel (x86-64, `vmlinux` and bzImage) builds and
boots in QEMU. A bare-metal x86-64 image built from a linker script matches
the section and symbol addresses GNU ld produces, and gives the same flat
binary.

---

## M4: More ELF architectures

In priority order. Each one needs its relocations, thunks and relaxations,
and TLS models.

- [ ] **AArch64**: range extension thunks, ADRP/ADD relaxation, TLSDESC,
      BTI/PAC properties, `-z force-bti`, erratum 843419 workaround
- [ ] **RISC-V 64/32**: linker relaxation with section shrinking (iterative
      layout), `__global_pointer$`, attribute section merging
- [ ] **i386**: GOT-relative relocations, `-z ibtplt`, TLS GNU dialect
- [ ] **ARM (32-bit)**: Thumb/ARM interworking, veneers, `.ARM.exidx`
      ordering and synthesis, BE8, `R_ARM_V4BX`
- [ ] **x32** (`elf32_x86_64`)
- [ ] **PowerPC64 LE/BE** (ELFv2 / ELFv1 with OPDs, TOC, long-branch stubs)
- [ ] **LoongArch64**, **s390x**
- [ ] Big-endian ELF and ELF32 handled through the same generic code
      (monomorphized, no run-time endianness checks on hot paths)

**Exit criteria:** For each architecture, the fixture suite passes under
`qemu-user`, and a Debian or Alpine userland package set builds with qld as
its linker.

---

## M5: Performance and optimization

- [ ] Benchmark suite and dashboard: clang, chromium, rustc, the Linux kernel,
      and a large debug-info-heavy Rust binary. Compared against GNU ld, gold,
      lld, mold and wild on wall time, CPU time, peak RSS and output size.
- [ ] Profiling passes on every pipeline stage. Thread scaling from 1 to 64+ cores.
- [ ] Identical code folding: `--icf=safe` (using `.llvm_addrsig`) and `--icf=all`
- [ ] String tail merging (`-O2`)
- [ ] Section ordering: `--symbol-ordering-file`, `--call-graph-profile-sort`
- [ ] Compressed debug sections: read and write zlib and zstd
      (`--compress-debug-sections`)
- [ ] `--gdb-index` and `--debug-names` generation
- [ ] Unlinking a large old output file in the background
- [ ] Optional separate-debug output (`--separate-debug-file`) with
      `.gnu_debuglink`

**Exit criteria:** On the benchmark suite, qld's wall-clock time is at or below
mold's and wild's on x86-64, on both 8-core and 64-core machines, with peak
RSS no higher than lld's. Output is identical across 1, 2 and N threads for
every benchmark.

---

## M6: Link-time optimization

- [ ] GNU linker plugin API host (`-plugin`, `-plugin-opt`, `--plugin-opt=`),
      dynamically loading `liblto_plugin.so` (GCC) and `LLVMgold.so` (LLVM)
- [ ] Claim-file handling, symbol resolution reporting (`LDPR_*`), adding
      compiled objects back into the link, archives containing IR members
- [ ] ThinLTO options passthrough: jobs, cache directory and pruning policy,
      `thinlto-index-only`
- [ ] Clean fallback and a clear diagnostic when an IR input has no plugin

**Exit criteria:** `gcc -flto` and `clang -flto` / `-flto=thin` builds of the M2
project set link and pass their test suites. The linker plugin is behind the
`plugin` cargo feature, which is on by default in the binary and off by default
in the library.

---

## M7: PE/COFF (MinGW flavor)

- [ ] COFF object and archive parsing, `.drectve` linker directives
- [ ] Short import libraries (MSVC/LLVM style) and long import libraries
      (GNU dlltool `.idata$N` objects); linking directly against `.dll` files
- [ ] PE32+ (x86-64) then PE32 (i386), then ARM64
- [ ] EXE and DLL output, `--out-implib`, `.def` files, `--export-all-symbols`,
      export and import tables, base relocations, TLS directory
- [ ] x86-64 SEH: `.pdata`/`.xdata` handling. i386 SafeSEH.
- [ ] Auto-import and runtime pseudo-relocations (`--enable-auto-import`,
      `__RUNTIME_PSEUDO_RELOC_LIST__`)
- [ ] Resources (`.rsrc` from windres objects), subsystem and OS version fields,
      `--dynamicbase`, `--nxcompat`, `--high-entropy-va`, deterministic timestamps
- [ ] DWARF in PE for MinGW debugging

**Exit criteria:** A MinGW-w64 GCC and a clang cross toolchain can use qld to
build and run a C/C++ test suite under Wine and on a Windows CI runner. qld
builds a working `x86_64-pc-windows-gnu` Rust binary, and it runs.

---

## M8: Mach-O (ld64 flavor)

- [ ] ld64 argv flavor: `-arch`, `-platform_version`, `-syslibroot`,
      `-framework`, `-dylib`, `-bundle`, `-dead_strip`, `-undefined`, `-exported_symbols_list`
- [ ] Mach-O object parsing including `.subsections_via_symbols` atomization
- [ ] `.tbd` text stubs (v3/v4/v5) and dylib inputs, two-level namespace, re-exports
- [ ] arm64 and x86_64: stubs, GOT, thunks (arm64), TLV
- [ ] `LC_DYLD_CHAINED_FIXUPS` and legacy `LC_DYLD_INFO_ONLY` output
- [ ] Compact unwind (`__unwind_info`) synthesis, `__eh_frame`
- [ ] Ad-hoc code signature (`LC_CODE_SIGNATURE`), which arm64 macOS requires
- [ ] STABS debug map (`N_OSO`) so `dsymutil` can find DWARF in object files
- [ ] Objective-C / Swift sections handled correctly (category merging later)
- [ ] **Universal (fat) binaries**: link each slice in parallel from multiple
      `-arch` values and write the `fat_header`; accept fat objects, archives
      and dylibs as inputs by selecting the matching slice

**Exit criteria:** clang on macOS builds and runs a C, C++ and Objective-C
test suite on arm64 and x86_64 CI runners with `-fuse-ld=qld`. A Rust
`aarch64-apple-darwin` binary runs. A universal binary passes `lipo -info`
and runs natively on both architectures.

---

## M9: Library API stabilization and 1.0

- [ ] Public API review ([library-api.md](docs/library-api.md)); semver guarantees
- [ ] In-memory inputs and outputs, caller-provided thread pool, cancellation
- [ ] Complete rustdoc with examples; published on crates.io
- [ ] Packaging: prebuilt binaries, distribution packages, `ld.qld` and `ld64.qld` symlinks

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
