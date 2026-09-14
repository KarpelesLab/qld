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
- Paths given to `-L`, `-T`, `--sysroot` and `-rpath-link` that start with `=`
  or `$SYSROOT` are resolved relative to `--sysroot`.
- `--` ends option parsing; everything after it is an input file.

### Positional options

These options change the state that applies to the input files *after* them
on the command line. qld records them as attributes on each input.

`--whole-archive`/`--no-whole-archive`, `--as-needed`/`--no-as-needed`,
`-Bstatic` (aliases `-dn`, `-non_shared`, `-static`) / `-Bdynamic` (aliases
`-dy`, `-call_shared`), `--start-group`/`--end-group` (`-(`/`-)`),
`--push-state`/`--pop-state`, `--copy-dt-needed-entries`, `-b`/`--format`.

### Option handling policy

The option table lists every option from GNU ld, gold, lld and mold, and gives
each one a status:

| Status | Behavior |
| --- | --- |
| **implemented** | Works as documented upstream |
| **accepted-ignored** | Parsed and ignored with no diagnostic. Only for options that have no observable effect for qld (e.g. `--no-keep-memory`, `--reduce-memory-overheads`, `-O0`, `--threads` in some forms) |
| **unsupported** | An error that names the option. Used when silently ignoring it could produce a wrong binary |

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
| rustc | `-C linker=clang -C link-arg=-fuse-ld=qld`, or `-C link-arg=-fuse-ld=/path/to/qld` with clang |
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

### Default library search paths

GNU ld has built-in `SEARCH_DIR`s from its default linker script. qld, like
lld, has **none**: compiler drivers always pass `-L` for the system
directories. A bare `qld -lc` without `-L` therefore fails where GNU ld would
succeed. The error message names the missing search path.

### Other differences

- **Unknown `-z` keywords** produce a warning, not an error (same as GNU ld).
- **Output file replacement.** The existing output file is unlinked and a new
  one is created, rather than truncated in place. This matches gold, lld and
  mold. It means hard links to the old output are not updated.
- **Threads.** Parallel by default. `--threads=N`, `--no-threads` and
  `--thread-count=N` (gold) are honored.

## ld64 flavor notes

- Single-dash long options only (`-dylib`, `-framework Foo`, `-arch arm64`).
- `-arch` may be given several times. qld then links each architecture
  (in parallel) and writes a universal binary. This is an extension:
  ld-prime requires `lipo` for this.
- `-lto_library` is accepted. Mach-O LTO uses `libLTO` through the plugin
  layer, not the GNU plugin API. See [optimizations.md](optimizations.md#lto).
