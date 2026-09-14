# Optimizations and symbol matching

## Garbage collection (tree shaking)

Enabled with `--gc-sections` (GNU flavor, including MinGW PE) or `-dead_strip`
(ld64). Off by default, as in the linkers qld replaces.

**Roots:** entry symbol · `-u`/`--undefined`/`--require-defined` · symbols
exported to the dynamic symbol table (`-shared`, `--export-dynamic`,
`--dynamic-list`) · `KEEP(...)` in linker scripts · `.init_array`, `.fini_array`,
`.preinit_array`, `.ctors`, `.dtors`, `.init`, `.fini` · `SHF_GNU_RETAIN` ·
non-`SHF_ALLOC` sections · `.note.*` · sections referenced through
`__start_<sec>`/`__stop_<sec>` (with lld's `-z start-stop-gc` semantics
available).

**Edges:** relocations from a live section, `SHF_LINK_ORDER` dependencies, and
section group membership (a live member keeps its whole COMDAT group alive).

**Granularity:** the unit is the input section. ELF code needs
`-ffunction-sections -fdata-sections` to be removed per function. For Mach-O,
`MH_SUBSECTIONS_VIA_SYMBOLS` lets qld split sections into per-symbol atoms,
which is safe for that format. qld does not split ELF sections at symbol
boundaries, because nothing in the format guarantees that splitting is safe.

**Removed along with dead sections:**

- `.eh_frame` FDEs of dead functions, and CIEs no FDE uses any more
- GOT, PLT and dynamic symbol entries that only dead code needed
- `DT_NEEDED` entries under `--as-needed` when no live reference remains
- `.debug_*` references to dead code, which resolve to tombstones

**Diagnostics:** `--print-gc-sections`, and `--why-live=<symbol>` prints the
reference chain from a root.

## Identical code folding

- `--icf=all`: fold any sections with identical contents and equivalent
  relocations, even if their addresses are compared somewhere.
- `--icf=safe`: fold only sections whose address is not significant, according
  to `.llvm_addrsig`. Clang emits that table by default; GCC does not, so for
  GCC objects `safe` folds only read-only data and functions that are not
  address-taken.
- **Algorithm:** a parallel initial hash over contents, flags and relocation
  shapes, followed by iterative refinement: each round rehashes every section
  using the current equivalence class of its relocation targets, until no
  class splits. Within a class, the section that appears first in input order
  is kept, so the result is deterministic.
- `--print-icf-sections`.

## Merge sections

`SHF_MERGE` sections are split into pieces (NUL-terminated strings for
`SHF_STRINGS`, fixed-size entries otherwise) in parallel. The pieces go into
a sharded concurrent map keyed by content hash. Output offsets are assigned
by walking the pieces in input order.

- `-O2` enables **tail merging** of strings (`"bar"` shares the storage of
  `"foobar"`). It costs a suffix sort per output section, so it is off at `-O1`.
- Relocations that point into merge sections are rewritten to (piece,
  addend-within-piece) during the relocation scan.

## Relocation relaxation

qld rewrites instructions when the final layout allows it. `--no-relax`
disables this.

| Arch | Relaxations |
| --- | --- |
| x86-64 | `GOTPCRELX`/`REX_GOTPCRELX` → direct `lea`/`mov`/`call`/`jmp`; TLS GD/LD/IE/TLSDESC → LE (and GD → IE for shared libraries) |
| i386 | `GOT32X` → direct; TLS relaxations |
| AArch64 | ADRP+LDR GOT → ADRP+ADD; ADRP+ADD → ADR+NOP; TLSDESC → IE/LE |
| RISC-V | `CALL` → `JAL`, `LUI`+`ADDI` → GP-relative, `ALIGN` handling; section sizes shrink, so layout iterates |
| LoongArch | PCALA/GOT/call relaxations (size-changing) |

## Dynamic relocation compaction

- `-z pack-relative-relocs`: `DT_RELR` for relative relocations
  (with `GLIBC_ABI_DT_RELR` version need)
- `-z combreloc` (default): sort `.rela.dyn` so the relative relocations come
  first, and emit `DT_RELACOUNT`
- `--hash-style=gnu` (default for new outputs) / `sysv` / `both`

## Section ordering

- `--symbol-ordering-file` (lld) and `--section-ordering-file` (gold)
- `--call-graph-profile-sort` using `.llvm.call-graph-profile` or a supplied
  call graph, with the C³ heuristic
- `.text.hot.*`, `.text.unlikely.*` and `.text.startup.*` grouping, as in
  GNU ld's default script (`-z keep-text-section-prefix`)

## LTO

The GNU flavor implements the **GNU linker plugin API** (`plugin-api.h`), the
interface GCC and Clang expect when they run the linker with
`-plugin <path>`:

- **GCC:** `liblto_plugin.so`, which runs `lto-wrapper`
- **LLVM:** `LLVMgold.so` (full LTO and ThinLTO)

**Flow:** the plugin claims IR inputs (`claim_file_handler`) and qld adds
their symbols to resolution. When resolution settles, qld reports each
symbol's status (`LDPR_PREVAILING_DEF_IRONLY`, …) through `get_symbols`. The
plugin compiles the IR (`all_symbols_read_handler`) and adds native objects
(`add_input_file`). qld then re-runs resolution with those objects in place
of the IR.

**Pure Rust note:** building qld needs no C code. The plugin itself is native
code that the compiler toolchain provides, and it is loaded with `dlopen` at
run time, only when `-plugin` is given. This lives in `src/plugin/` behind the
`plugin` cargo feature. That feature is the only place qld uses FFI.

The ld64 flavor uses `libLTO` (`-lto_library`) through a thin adapter in the
same crate.

Because the plugin API is a C callback interface with global state, a
process can run only one plugin-based link at a time. The library API returns
an error if a second one is attempted concurrently.

## Intelligent library symbol matching

"Intelligent library symbol matching" covers these features:

1. **Order-independent archive resolution.** Archive order on the command line
   does not affect whether a symbol resolves; precedence is still decided by
   input order. See [compatibility.md](compatibility.md#archive-resolution-order).
2. **Missing-library suggestions.** When a symbol stays undefined, qld looks it
   up in the `.dynsym` and `.gnu.hash` tables, or the archive symbol indexes,
   of libraries in the `-L` paths that were not linked:
   ```
   qld: error: undefined symbol: cos
   >>> referenced by main.c:12 (main.o:(.text.main+0x1a))
   >>> note: 'cos' is defined in libm.so.6 (/usr/lib64/libm.so); did you forget -lm?
   ```
   The index of search-path libraries is built lazily, only after an error has
   already happened, so successful links pay nothing for it.
3. **Version-aware matching.** A reference to `memcpy@GLIBC_2.14` against a
   library that only provides `memcpy@GLIBC_2.2.5` gets a message that names
   the versions available, not a bare "undefined symbol".
4. **Near-miss hints.** qld suggests candidates that differ only in C++
   mangling (a C/C++ `extern "C"` mismatch), in namespace, in
   const/volatile/reference qualifiers, or in a leading underscore (a Mach-O
   or i386 COFF convention error). Names are shown demangled (Itanium and Rust
   v0/legacy) unless `--no-demangle` is given.
5. **Duplicate definition explanations.** When two definitions collide, qld
   reports which archive member was pulled in and which reference caused the
   extraction.
