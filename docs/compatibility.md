# Command-line compatibility

qld takes the command lines that compiler drivers already pass to existing
linkers. This document defines how argv is interpreted, and lists every place
where qld *intentionally* behaves differently from the linker it replaces.

## Flavors

A flavor is a command-line dialect together with its default target family.

| Flavor | Emulates | Selected by |
| --- | --- | --- |
| `gnu` | GNU ld (BFD), gold, lld (ELF), mold | `argv[0]` = `qld`, `ld`, `ld.qld`, `ld.*`; default |
| `gnu` + PE emulation | GNU ld for MinGW (`i386pep`, `i386pe`, `arm64pe`) | `gnu` flavor with `-m i386pep`/`i386pe`/`arm64pe`, or a COFF first input |
| `darwin` | Apple ld64 / ld-prime | `argv[0]` = `ld64`, `ld64.qld`; `-flavor darwin` |
| `msvc` *(future)* | `link.exe` / `lld-link` | `argv[0]` = `lld-link`, `link.exe`; `-flavor link` |

The priority order is: an explicit `-flavor <name>` as the first argument,
then the basename of `argv[0]`, then `gnu`.

## Target selection (GNU flavor)

1. `-m <emulation>`, if given (`elf_x86_64`, `elf_i386`, `elf32_x86_64`,
   `aarch64linux`, `aarch64elf`, `elf64lriscv`, `elf32lriscv`,
   `armelf_linux_eabi`, `elf64lppc`, `elf64ppc`, `elf64loongarch`, `elf64_s390`,
   `i386pep`, `i386pe`, `arm64pe`, …)
2. Otherwise, the machine type of the first object file input (as lld does)
3. Otherwise, `OUTPUT_FORMAT`/`OUTPUT_ARCH` from a linker script
4. Otherwise, the host target

An input whose machine type conflicts with the selected target is an error.

## Option syntax (GNU flavor)

These rules follow GNU ld's parser:

- A multi-letter option can take one or two dashes: `-soname` = `--soname`.
  **Exception:** a multi-letter option starting with `o` needs two dashes
  (`--omagic`, `--output`), because `-omagic` means `-o magic`.
- Values can be joined or separate: `-Lpath` / `-L path`, `-lc` / `-l c`,
  `--library-path=path` / `--library-path path`. `-l:libfoo.a` searches for
  the exact file name.
- `-z keyword` and `-zkeyword` are equivalent. Unknown `-z` keywords produce a
  warning (as in GNU ld), not an error.
- `@file` response files are expanded recursively, with GNU quoting rules.
  On Windows hosts, and for the `msvc` flavor, Windows quoting rules apply.
- Paths given to `-L`, `-T` and `-rpath-link` whose *value* starts with `=`
  or `$SYSROOT` are resolved relative to `--sysroot` (`-L=/usr/lib`,
  `-L =/usr/lib`, `-rpath-link =/x`). In `--rpath-link=/x` the `=` is the
  value separator, not a sysroot prefix. With no `--sysroot`, the prefix is
  simply removed. The parser records the prefix; resolution is a pure string
  step before any file is opened.
- `--` ends option parsing; everything after it is an input file.
- **Single-dash long names win over short options with joined values**, as in
  lld: `-export-dynamic` is `--export-dynamic`, not `-e xport-dynamic` as GNU
  ld's getopt would read it.
- **Not supported:** GNU's unique-prefix abbreviations (`--whole-arch` for
  `--whole-archive`) and clustered short options (`-sS`). Both are
  error-prone and no compiler driver emits them.
- **A missing response file is an error.** GNU ld keeps `@file` as a literal
  file name when it doesn't exist.

### Positional options

These options change the state that applies to the input files *after* them
on the command line. qld records them as attributes on each input.

`--whole-archive`/`--no-whole-archive`, `--as-needed`/`--no-as-needed`,
`-Bstatic` (aliases `-dn`, `-non_shared`, `-static`) / `-Bdynamic` (aliases
`-dy`, `-call_shared`), `--start-group`/`--end-group` (`-(`/`-)`),
`--push-state`/`--pop-state`, `--copy-dt-needed-entries`, `-b`/`--format`,
`--start-lib`/`--end-lib` (lld). Unbalanced groups, lib markers and
`--pop-state` are errors.

### Output kind

- The last of `-shared` and `-pie`/`-no-pie` wins, as in GNU ld. `-r`
  combined with either is an error.
- Whether the output is static is decided by the `-Bstatic`/`-Bdynamic` state
  at the **end** of the command line, as in mold. rustc's
  `-Bstatic … -Bdynamic` sequence therefore still produces a dynamic PIE, and
  gcc's `-static -pie --no-dynamic-linker` produces a static PIE.
- With several of `-s` and `-S`, the strongest wins (strip all). GNU ld uses
  the last one given.

### Option handling policy

The option table lists every option from GNU ld, gold, lld and mold, and gives
each one a status:

| Status | Behavior |
| --- | --- |
| **implemented** | Works as documented upstream |
| **accepted-ignored** | Parsed and ignored with no diagnostic. Only for options that have no observable effect for qld (e.g. `--no-keep-memory`, `--reduce-memory-overheads`, `--hash-size`) |
| **unsupported** | An error that names the option and, when support is planned, the roadmap milestone (`unsupported option: --subsystem (not implemented yet (roadmap M7: PE/COFF))`). Used when silently ignoring it could produce a wrong binary |

The table lives in `src/args/table.rs` (about 600 options and 100 `-z`
keywords), and `qld --help` is generated from it.

An option that appears in no table is an error (`qld: error: unknown option: --foo`),
as in GNU ld.

## Version probing

Build systems detect the linker by its version output, so qld prints:

```
$ qld -v
qld 0.1.0 (compatible with GNU linkers)
```

- libtool and autoconf treat a linker as GNU ld when `$LD -v` contains `GNU`,
  and qld's string contains it.
- `--version` prints the same first line followed by license text.
- Meson and CMake identify a linker from `-Wl,--version` output. Upstream
  detection will need to recognize qld; until then it is detected as a
  generic GNU-compatible linker.

## Using qld from compiler drivers

| Driver | Method |
| --- | --- |
| clang | `-fuse-ld=qld` (looks up `ld.qld` in `PATH`), or `--ld-path=/path/to/qld` |
| gcc | `-B<dir>`, where `<dir>/ld` is a symlink to qld. Newer GCC versions may accept `-fuse-ld=` values other than bfd/gold/lld/mold; check your version. |
| rustc | `-C linker=clang -C link-arg=--ld-path=/path/to/qld`. On targets where rustc links with its bundled `rust-lld` by default (x86-64 Linux on recent stable), also pass `-C linker-features=-lld`, otherwise rustc's own `-fuse-ld=lld` wins over a later `-B` or `-fuse-ld` |
| Apple clang | `-fuse-ld=/path/to/ld64.qld` or `--ld-path=` |

## Intentional behavioral differences

Any change to this list needs a matching entry in the changelog.

### Archive resolution order

**GNU ld:** an archive is scanned only at its position on the command line.
A member that defines a symbol referenced only by a *later* object is not
extracted, unless `--start-group` is used.

**qld:** order does not matter, as in lld and mold. A lazy archive member is
extracted whenever any live object references a symbol it defines, wherever
that object appears. `--start-group`/`--end-group` are accepted and have no
further effect.

**What is kept:** when several archives or objects can satisfy a symbol, the
one that appears *first on the command line* wins, which is the same choice
GNU ld makes in all of the links it accepts. A future
`--warn-backrefs` option (as in lld) will report links that GNU ld would
reject.

**Shared library versus archive member:** as in GNU ld, an archive listed
before a shared library that defines the same symbol has its member
extracted (gcc's `-lgcc --as-needed -lgcc_s` relies on this); a symbol only
referenced weakly still binds to the shared library. The one remaining
difference is the order-independence above: a member of an earlier archive
is also extracted when only a later object refers to it.

### Default library search paths

GNU ld has built-in `SEARCH_DIR`s from its default linker script. qld, like
lld, has **none**: compiler drivers always pass `-L` for the system
directories. A bare `qld -lc` without `-L` therefore fails where GNU ld would
succeed. The error message names the missing search path.

### Other differences

- **Unknown `-z` keywords** produce a warning, not an error (same as GNU ld).
- **Output file replacement.** The output is written to a temporary file and
  renamed over the old one, rather than truncated in place. As with gold, lld
  and mold, hard links to the old output are not updated, and a symlink at
  the output path is replaced rather than followed. Paths that are pipes or
  devices (anything under `/dev` or `/proc`, such as `-o /dev/stdout`) are
  written into directly.
- **Build ID values.** `--build-id=md5` and `--build-id=sha1` produce digests
  of the right length and kind, but not the same values as GNU ld: qld hashes
  1 MiB blocks in parallel and then hashes the block digests.
  `--build-id=fast` is an 8-byte xxHash64 tree hash. Values are stable
  across platforms and thread counts.
- **Threads.** Parallel by default. `--threads=N`, `--no-threads` and
  `--thread-count=N` (gold) are honored. Without them, inputs are mapped
  with at most 16 threads and the rest of the link uses one thread per 4 MiB
  of input (counting compressed debug sections at their inflated size), at
  most 16, because small links run faster on few threads. A
  library caller's own rayon pool is respected. Output never depends on the
  thread count. With more than 16 threads, every stage except the
  relocation scan and section merging runs on a 16-thread pool, which is
  faster on large machines.
- **`--fork` (default on Unix).** As in mold and wild, `qld` links in a
  child process (the same executable, started again with `QLD_FORK_CHILD`
  in its environment) and returns as soon as the output is complete; the
  child then frees memory and unmaps inputs. Output written to pipes is
  relayed so callers see end of file at once. The child's stdin reads as end
  of file. A link whose arguments name a path under `/dev` or `/proc` (such
  as `-Map=/dev/stdout`) stays in process. A signal sent to the parent alone
  lets the child finish; one sent to the process group stops both. If the
  child dies before reporting, the parent exits with its status (128 + the
  signal number when killed). `--no-fork` links in process; the library
  never forks.
- **TLS relaxation in executables.** Initial-exec accesses to thread-local
  symbols that the executable defines and exports are relaxed to local-exec;
  GNU ld keeps `R_X86_64_TPOFF64` dynamic relocations for them.
- **`--help`** lists `supported targets` and `supported emulations` only for
  what qld links today (libtool reads the first line to enable shared
  libraries).
- **Executable stack.** An object without a `.note.GNU-stack` section does not
  make the stack executable (lld's choice); GNU ld treats such objects as
  needing one. Use `-z execstack` to request it.
- **Hidden symbols in `.symtab`.** Hidden global symbols are written as
  `LOCAL`, as lld does; GNU ld keeps them `GLOBAL` in static executables.
- **No tail merging of `.debug_str` by default.** GNU ld tail-merges mergeable
  strings at every level; qld, like lld, does it only at `-O2`, so debug-heavy
  outputs can be a few percent larger.
- **`--print-gc-sections` and `--print-icf-sections`** write their lines as
  `qld: note: …` diagnostics.
- **`PT_GNU_RELRO`** is not emitted in static executables yet (M2).
- **Debug tombstones.** A relocation in `.debug_loc` whose target was
  discarded gets `1` (lld's value), so the location list is not cut short.
  GNU ld 2.46 writes `0` there, which ends the list early. `.debug_ranges`
  gets `1` in both linkers; `.debug_names` gets `-1` as in lld; everything
  else gets `0`. Override with `-z dead-reloc-in-nonalloc=<glob>=<value>`.
- **Compressed debug output bytes** differ from GNU ld's (different zlib
  implementation and chunked parallel compression); the decompressed contents
  are the same.

### Dynamic linking and `-r`

- **COMDAT selection** takes the earliest resolution round, then the lowest
  input position. GNU ld keeps the first copy it loads; the two differ only
  when an archive member extracted for a later file's reference carries the
  group (GNU keeps the member's copy, qld the later object's). The copies are
  interchangeable by definition.
- **Static executables get `PT_GNU_RELRO`**, as with dynamic ones.
- **GNU quirks kept on purpose:** a `.plt` header is emitted even when only
  `.plt.got` entries exist, and calls to undefined weak symbols in static PIEs
  go through `.plt.got`; copy-relocated aliases share one copy (lld style).
- Imports appear in `.symtab` as `name@VERSION`.
- **`-r`:** output sections appear in first-appearance order (GNU uses its
  script order); `SHF_MERGE` sections are concatenated, not deduplicated;
  `.eh_frame` is kept as-is (GNU removes duplicate CIEs); with
  `--gc-sections`, unreferenced common symbols are kept; `--defsym x=sym+off`
  makes `x` relative to `sym`'s section (GNU makes it absolute).
- **`--emit-relocs`:** relocations to discarded COMDAT copies become
  `R_X86_64_NONE` (GNU redirects debug relocations to the kept copy).
- **`--cref`** leaves out symbols mentioned only by shared libraries; **`-y`**
  prints `qld: note: main.o: reference to puts`.
- **Compressed debug output** uses zlib level 1 below `-O2`, so a section
  that barely compresses can end up stored uncompressed where GNU ld
  compresses it, or the reverse.

### LTO plugins

- qld reports GNU ld version 2.44 to plugins and sends no gold version (GCC's
  plugin changes behaviour when it believes it runs under gold). It
  negotiates plugin API level 1, which GNU ld 2.46 does not offer.
- Plugins are not `dlclose`d, and a plugin library serves one link per
  process.
- The plugin `message` callback formats integer and string arguments;
  floating-point arguments are shown unformatted.
- A fatal plugin message ends the link with an error. Used as a library, qld
  returns the error instead of exiting, and the plugin is not called again.

### LTO links

- Plugins are loaded only when an IR input appears, so a `-plugin` path that
  does not exist is not an error for a link with no IR (compiler drivers pass
  `-plugin` unconditionally).
- Archives of IR members **without** a symbol index are accepted (members are
  claimed to learn their symbols); GNU ld rejects them.
- Libraries a plugin asks for that are not found are skipped, and none are
  added for `-r`.
- IR that only the code generated by LTO needs is an error: it cannot be
  compiled after code generation.
- A fatal plugin message ends the link. The `qld` binary exits inside the
  plugin's callback as GNU ld does; a library caller gets an error instead.

### AArch64

- TLSDESC is bound eagerly through `.rela.dyn`, as lld does; GNU ld uses a
  lazy TLSDESC PLT (`DT_TLSDESC_PLT`/`DT_TLSDESC_GOT`). Both work under glibc.
- A preemptible function that also has a GOT entry goes through `.plt.got`,
  which GNU ld's AArch64 port does not have, so `.plt` has one fewer entry.
- Only the PLT header gets a `bti c` landing pad, as in GNU ld: entries are
  reached by direct branches.
- `-z separate-code` is off by default, matching GNU ld.
- Range-extension thunks are pooled per output section, so a single output
  section holding more than 128 MiB of code reports a relocation overflow
  instead of splitting the pool.

### Linker scripts

qld's script parser follows GNU ld's grammar and tokenization (including its
surprises: `foo=1` at top level is a single name, and `INPUT(a.o, b.o)` names
a file `a.o,`). Deliberate differences:

- **More lenient** where GNU ld rejects: a stray `;` inside `SECTIONS`
  (including after `ASSERT(...)`), an empty `INPUT()`, output section
  attributes in any order, a keyword used as a symbol name where no keyword
  could appear, and in version scripts `local:` before `global:` and an
  optional `;` before `}`.
- **Short-circuit evaluation**: `&&` and `||` do not evaluate their right
  operand when the result is already known. GNU ld evaluates both, which only
  matters when the right side would be an error.
- **Stricter**: invalid characters are errors (GNU ld warns); division or
  modulo by zero is always an error; `i64::MIN / -1` yields `i64::MIN`
  (GNU ld crashes with `SIGFPE`).
- **Nesting limits**: expressions 128 levels deep, `INCLUDE` 10 (as GNU ld),
  `AS_NEEDED` and version `extern` blocks 32.
- **Not supported**: MRI scripts (`-c`).
- `OVERWRITE_SECTIONS` is an lld extension and follows lld's semantics.

Errors are reported GNU-style as `file:line:column: message`.

Layout from scripts follows GNU ld's algorithms, including orphan placement
and the address fixpoint ("address assignment did not converge after 12
passes" when it cannot settle). Known differences:

- `-r` together with `-T` is not supported yet (relocatable output has its own
  layout path).
- `--verbose` does not dump the effective default script.
- `--defsym` expressions beyond `symbol+offset` are not supported.
- GLOBAL HIDDEN input symbols are written LOCAL in static links (17 `__pi_*`
  symbols differ this way in a kernel build); GNU ld keeps them GLOBAL.
- qld does not emit `FILE` symbols.

### Demangled names

Diagnostics, map files and `--print-*` output demangle names unless
`--no-demangle` is given.

- Itanium C++ output is byte-identical to `c++filt` from binutils 2.46 on
  every name it accepts (checked on 880k names from this machine's
  libraries). qld also demangles forms `c++filt` rejects: Mach-O `__Z`/`__R`
  prefixes, clone suffixes on data symbols (`_ZL3foo.llvm.123`),
  `_GLOBAL__sub_I_<file>` (shown as "global constructors keyed to"), and
  Clang's template-parameter declarations in template arguments.
- Rust names: the legacy `17h<hash>E` suffix is hidden by default, as
  `rustfilt` does. Rust v0 constants wider than 64 bits print as correct hex,
  where `c++filt` garbles them.
- Not yet demangled (left as-is): C++20 `requires` clauses and a few other
  recent Itanium extensions that `llvm-cxxfilt` handles.

### PE/COFF output (MinGW flavor)

- Input sections are ordered inside an output section by archive-member
  position; GNU ld uses extraction order. Addresses stay self-consistent.
- `IMAGE_COMDAT_SELECT_LARGEST` picks the largest copy within one resolution
  round; an earlier round's claim is final.
- The output symbol table carries globals and section symbols, not locals.
- MSVC import-library helper objects (`__IMPORT_DESCRIPTOR_*`,
  `__NULL_IMPORT_DESCRIPTOR`, `*_NULL_THUNK_DATA`) are dropped and the import
  directory is synthesized instead.
- `.pdata` is always sorted by address.
- `.drectve` `-defaultlib:`/`-include:` are honoured from command-line objects
  but not from archive members extracted later.
- `--out-implib` writes the long `dlltool` form of an import library.

### PE/COFF inputs (MinGW flavor)

The readers exist; linking PE output is M7. Behaviour already fixed by them:

- **`.drectve` quoting**: values are split outside quotes first, so
  `-export:"a,b"` is one name (GNU ld's reading; lld splits it).
- **`.def` files** accept the union of GNU dlltool/ld and lld syntax:
  `NONAME`, `DATA`, `PRIVATE`, `CONSTANT` in any case, commas between flags,
  `DESCRIPTION`, `SECTIONS`, `IMPORTS`, `CODE`/`DATA`, and `NONAME` without an
  ordinal (lld rejects these). Numbers may be `0x` hex; a leading `0` is
  decimal, not octal as in GNU. `EXPORTAS` is a keyword (GNU dlltool 2.4x
  reads it as two more exports).
- **Section alignment** with no `IMAGE_SCN_ALIGN_*` flag is left for the
  linker to choose (lld uses 1, MSVC and GNU ld use 16).
- **Data exports of a DLL linked directly** are recognized from the section's
  code/execute flags; GNU ld looks at the section name.

## ld64 flavor notes

- Single-dash long options only (`-dylib`, `-framework Foo`, `-arch arm64`).
- `-arch` may be given several times. qld then links each architecture
  (in parallel) and writes a universal binary. This is an extension:
  ld-prime requires `lipo` for this.
- **Atomization** for `-dead_strip` follows lld: `N_ALT_ENTRY` symbols don't
  start atoms, and ld64's special handling of `L`/`l` labels is not
  reproduced (clang does not put those labels in the symbol table).
- Selecting `x86_64` from a universal input also accepts a lone `x86_64h`
  slice.
- `@file` arguments stay literal when no such file exists, so `@rpath/...`
  and `@executable_path/...` pass through.
- References to exported weak definitions bind through weak lookup, as ld64
  does. Implicit re-exports (a public sub-library such as `libc++abi` under
  `libc++`) are followed.
- `ZERO_AR_DATE` in the environment zeroes the debug-map (`N_OSO`)
  timestamps.
- `-order_file` accepts ld64's syntax: one symbol per line, optional
  `arch:` and `object.o:` prefixes, `#` comments.
- Known differences: no Objective-C relative method lists; unused CIEs in
  `__eh_frame` are dropped; legacy (`LC_DYLD_INFO_ONLY`) output binds every
  import at load time and does not use weak binding. Undefined symbols are
  reported before dead stripping, where ld64 reports only those reached from
  live code.
- Weak definitions marked "can be hidden" in every object are hidden, as in
  ld64 and lld.
- `-r` keeps `.subsections_via_symbols`, compact unwind, `__eh_frame`,
  linker options, and merges weak definitions and literals. DWARF sections
  are dropped with a warning (link the original objects for debug info),
  data-in-code and linker optimization hints are dropped, and export lists
  and `N_INDR` symbols are rejected. ld64.lld has no `-r`; qld's output is
  compared with Apple's `ld -r` in CI.
- `-init` is an error unless `-dylib`, as in ld64 (ld64.lld ignores it).
- Not supported yet, rejected with an error: `-force_flat_namespace`,
  `-alias_list`, arm64e, and LTO/bitcode.
- `-lto_library` is accepted. Mach-O LTO uses `libLTO` through the plugin
  layer, not the GNU plugin API. See [optimizations.md](optimizations.md#lto).
