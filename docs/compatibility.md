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

**Shared library versus archive member:** a definition in a shared library
always beats a lazy archive member, wherever each appears. GNU ld and lld
extract the member when its archive comes first on the command line. qld
does not, because deciding by position would make whether a member is pulled
in depend on command-line order again.

### Shared libraries beat archive members

If a symbol is defined both by a shared library and by an archive member that
has not been extracted, qld uses the shared library's definition, wherever the
two appear on the command line. GNU ld and lld extract the member when the
archive comes first. To force the static definition, name the object directly
or wrap the archive in `--whole-archive`.

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
  `--thread-count=N` (gold) are honored. Without them, the thread count is
  sized from the input (one thread per 16 MiB, at most 32), because small
  links run faster on few threads. Output never depends on the thread count.
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

## ld64 flavor notes

- Single-dash long options only (`-dylib`, `-framework Foo`, `-arch arm64`).
- `-arch` may be given several times. qld then links each architecture
  (in parallel) and writes a universal binary. This is an extension:
  ld-prime requires `lipo` for this.
- `-lto_library` is accepted. Mach-O LTO uses `libLTO` through the plugin
  layer, not the GNU plugin API. See [optimizations.md](optimizations.md#lto).
