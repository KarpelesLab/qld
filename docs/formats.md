# Formats and targets

This document covers the input and output formats qld plans to support, the
scope of each, and the order in which they arrive. Milestone numbers refer to
[ROADMAP.md](../ROADMAP.md).

## Input formats

| Input | Recognized by | Milestone |
| --- | --- | --- |
| ELF relocatable (`ET_REL`) | `\x7fELF` | M1 |
| ELF shared object (`ET_DYN`) | `\x7fELF` | M2 |
| `ar` archives: GNU, BSD, COFF, thin; with and without symbol index | `!<arch>\n`, `!<thin>\n` | M0 |
| GNU linker scripts as inputs (`GROUP`, `INPUT`, `AS_NEEDED`) | text | M2 |
| Raw binary (`-b binary`) | option | M3 |
| LLVM bitcode (`.o` / archive members) | `BC\xC0\xDE`, bitcode wrapper | M6 |
| GCC LTO IR (ELF with `.gnu.lto_*`) | ELF + section names | M6 |
| COFF objects, regular and bigobj (x86-64, i386, ARM64, ARM64EC/ARM64X, ARMNT) | machine field + heuristics | M7 (reader done) |
| COFF short import libraries | `IMPORT_OBJECT_HEADER` sig `0000 FFFF` | M7 (reader done) |
| PE DLLs (link directly against `.dll`) | `MZ` … `PE\0\0` | M7 (export reader done) |
| Module-definition files (`.def`) | extension / text | M7 (parser done) |
| Mach-O objects and dylibs | `MH_MAGIC_64` etc. | M8 (reader done) |
| Apple text-based stubs (`.tbd` v1–v5) | text (YAML / JSON) | M8 (reader done) |
| Universal (fat) inputs: select a slice | `FAT_MAGIC`, `FAT_MAGIC_64` | M8 (reader done) |

Archive variant notes:

- **BSD/Darwin** archives (`__.SYMDEF`, `#1/<len>` names) pad member data to
  8 bytes with `\n` and count the padding in the member size. A member's
  byte range can therefore run past the object's real end; parsers take the
  size from the object's own headers.
- **COFF** archives (MSVC, `llvm-ar --format=coff`) have a second `/` linker
  member with a sorted index, and NUL-terminated long names.
- **Thin** archives store member paths relative to the archive's directory.

Compressed input sections (`SHF_COMPRESSED` with zlib or zstd, and the legacy
`.zdebug_*`) are decompressed on demand, in parallel, only for sections that
survive GC.

## ELF output

### Output kinds

| Kind | Options | Milestone |
| --- | --- | --- |
| Static executable | `-static` | M1 |
| Static PIE | `-static-pie` / `-static -pie` | M2 |
| Dynamic executable | default | M2 |
| PIE | `-pie` | M2 |
| Shared object | `-shared` | M2 |
| Relocatable | `-r` | M2 |

### Architectures

| Architecture | Emulation(s) | Milestone | Notable work |
| --- | --- | --- | --- |
| x86-64 | `elf_x86_64` | M1–M2 | GOTPCRELX relaxation, TLS relaxation, CET/IBT PLT |
| AArch64 | `aarch64linux`, `aarch64elf` | M4 | range thunks, TLSDESC, BTI/PAC, erratum 843419 |
| RISC-V 64/32 | `elf64lriscv`, `elf32lriscv` | M4 | size-changing relaxation, GP relaxation, attributes |
| i386 | `elf_i386` | M4 | GOT-relative relocations |
| ARM | `armelf_linux_eabi` | M4 | interworking, veneers, `.ARM.exidx` |
| x32 | `elf32_x86_64` | M4 | ILP32 on x86-64 |
| PowerPC64 | `elf64lppc`, `elf64ppc` | M4 | TOC, ELFv1 OPD, long-branch stubs |
| LoongArch64 | `elf64loongarch` | M4 | relaxation |
| s390x | `elf64_s390` | M4 | big-endian 64-bit |

### DWARF and debug information

DWARF is not an output format of its own. qld handles debug sections inside
every output format:

- `.debug_*` sections are concatenated and relocated in parallel, split by
  input file. Debug sections are usually the largest part of a big link, so
  this path is heavily optimized. Relocations that point into GC'ed code
  resolve to tombstone values, following lld: `1` in `.debug_ranges` and
  `.debug_loc` (where `0` would end a list), `-1` in `.debug_names`, and `0`
  elsewhere. GNU ld differs for `.debug_loc`; see
  [compatibility.md](compatibility.md). Rules can be overridden with
  `-z dead-reloc-in-nonalloc=<glob>=<value>`.
- Compressed inputs (`SHF_COMPRESSED` zlib/zstd and legacy `.zdebug_*`) and
  compressed outputs (`--compress-debug-sections=zlib|zlib-gnu|zstd`) use
  codecs implemented in-crate. Compression splits a section into chunks
  compressed in parallel; the output is identical for any thread count.
- `.debug_str` and `.debug_line_str` go through the parallel string merger.
- Output compression: `--compress-debug-sections=none|zlib|zstd`.
- Index generation: `--gdb-index`, `--debug-names`.
- Split DWARF: skeleton units are linked normally. The `.dwo` files are not
  touched; building a `.dwp` is `dwp`'s job.
- Stripping: `-S`/`--strip-debug`, `-s`/`--strip-all`.
- Separate debug files: `--separate-debug-file` plus `.gnu_debuglink` (M5).
- PE (MinGW): DWARF in named PE sections (M7). CodeView/PDB belongs to the
  future MSVC flavor.
- Mach-O: DWARF is not copied into the output. qld emits the STABS debug map
  (`N_OSO`, `N_FUN`, …) so that `dsymutil` can find it (M8).

## Raw binary and embedded outputs (M3)

- `--oformat binary` / `OUTPUT_FORMAT(binary)` writes the loadable sections at
  their load addresses (LMA), relative to the lowest LMA, and fills gaps with
  the `FILL` value or zeros.
- `--oformat ihex`, `--oformat srec`: Intel HEX and Motorola S-record.
- All three are byte-identical to GNU ld's output for the bare-metal images in
  `tests/projects/baremetal.sh` and the `raw-*` fixtures.
- `-b binary` inputs become a `.data` section with `_binary_<name>_start`,
  `_end` and `_size` symbols, using GNU's path mangling.
- Linker scripts with `MEMORY` regions, `AT>` load regions and
  `ASSERT` checks.
- An ELF file can be written next to the raw image (as `objcopy -O binary`
  workflows would produce) for debugging. The option name is still to be
  decided.

## PE/COFF output (M7, MinGW flavor)

| Item | Scope |
| --- | --- |
| Formats | PE32+ (x86-64, ARM64), PE32 (i386) |
| Kinds | EXE (console/windows subsystems), DLL, with `--out-implib` |
| Imports | short and long import libraries, direct `.dll` linking, delay-load (later) |
| Exports | `.def` files, `__declspec(dllexport)` via `.drectve`, `--export-all-symbols` |
| Runtime support | base relocations, TLS directory, load config, SEH (`.pdata`/`.xdata`), SafeSEH (i386) |
| MinGW specifics | auto-import with runtime pseudo relocations, `__CTOR_LIST__`/`__DTOR_LIST__` |
| Headers | subsystem, OS/image versions, `--dynamicbase`, `--nxcompat`, `--high-entropy-va`, `--no-seh`, `--image-base` |
| Determinism | fixed timestamp unless `--insert-timestamp` is given |
| Resources | `.rsrc` from GNU windres objects, `.rsrc$01`/`.rsrc$02` from `cvtres`/`llvm-windres` |

## Mach-O output (M8, ld64 flavor)

| Item | Scope |
| --- | --- |
| Architectures | arm64, x86_64 (arm64e and arm64_32 later) |
| Kinds | `MH_EXECUTE`, `MH_DYLIB`, `MH_BUNDLE`, `-r` |
| Dynamic info | `LC_DYLD_CHAINED_FIXUPS` + `LC_DYLD_EXPORTS_TRIE` (default for new deployment targets), legacy `LC_DYLD_INFO_ONLY` |
| Linking | two-level namespace, re-exports, `-undefined dynamic_lookup`, weak imports, `-exported_symbols_list` |
| Dead stripping | `-dead_strip` using `.subsections_via_symbols` atoms |
| Unwinding | `__unwind_info` synthesized from `__compact_unwind`, `__eh_frame` fallback |
| Signing | ad-hoc `LC_CODE_SIGNATURE` (required for arm64 executables to run) |
| Platform | `LC_BUILD_VERSION` from `-platform_version` |
| ObjC/Swift | correct section handling. `__objc_imageinfo` merging. |

### Universal (fat) binaries

- **Output:** each `-arch` value produces a complete, independent link. The
  links run in parallel and share the parsed state of inputs that are
  themselves fat. The slices are assembled under a `fat_header` with
  page-aligned offsets (2^14 for arm64 slices, 2^12 for x86_64).
  `FAT_MAGIC_64` is used only when an offset exceeds 4 GiB.
- **Input:** fat objects, archives and dylibs are accepted. qld picks the slice
  that matches the architecture being linked, and reports an error that lists
  the available architectures when none matches.
