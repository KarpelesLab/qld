# Linux kernel

The M3 exit criterion: a Linux kernel linked by qld whose layout matches GNU
ld's and which boots.

- **Kernel version:** 7.2.5, from
  `https://cdn.kernel.org/pub/linux/kernel/v7.x/linux-7.2.5.tar.xz`
  (override with `KERNEL_VERSION=`)
- **Configuration:** plain `make O=$build defconfig` (x86_64 defconfig; that
  implies `CONFIG_RELOCATABLE`, `CONFIG_RANDOMIZE_BASE`, `CONFIG_KALLSYMS`,
  `CONFIG_MODULES`, `CONFIG_WERROR`, `CONFIG_X86_KERNEL_IBT`,
  `CONFIG_DEBUG_INFO_NONE`), no local modifications
- **Script:** `tests/projects/kernel.sh QLD SCRATCH`
- **Compiler:** the host gcc (15.3.0 here); only the linker is replaced
- **Time:** configure 11 s, build 54 s (64 cores, warm ccache; a few
  minutes cold), comparison 1 s, boot 3 s

## Why the kernel

`arch/x86/kernel/vmlinux.lds` is the most demanding linker script in common
use: explicit `SECTIONS` with addresses and `. = ALIGN()` arithmetic,
`ASSERT`s on section sizes and alignment, `PROVIDE`d symbols (including the
`__pi_*` aliases), sorted and `KEEP`t tables (`__ksymtab*`, `__param`,
`.orc_unwind*`, `__bug_table`, `__ex_table`, `.altinstructions`),
`/DISCARD/` rules, `--orphan-handling=error` (so any orphan section qld
places differently is a hard error) and `--emit-relocs` (so every
relocation must survive with output addresses). The boot stub
(`arch/x86/boot/compressed/vmlinux.lds`) adds its own script, `-pie`,
`--no-dynamic-linker` and `-u efi_pe_entry`.

## The linker shim

`kernel.sh` writes `$SCRATCH/bin/ld-kernel` and builds with `LD=` pointing
at it. The shim sends three kinds of call to GNU ld and everything else to
qld:

| Sent to GNU ld | Why |
| --- | --- |
| `-m elf_i386` links (`realmode.elf`, `vdso32.so.dbg`, `setup.elf`) | ELF32 output is roadmap M4; qld links `elf_x86_64` only |
| `-v`, `-V`, `--version` | `scripts/ld-version.sh` parses GNU ld's version string to gate features |
| `-r` (`vmlinux.o`) | qld's relocatable output makes objtool fail with "can't find starting instruction" — the relocatable writer, not script layout. `KERNEL_QLD_R=1` sends these to qld to reproduce it |

Everything else is qld. A build from clean logs **6 qld links and 18 GNU ld
calls, 0 qld failures** (the shim logs each call with the linker that ran
it, to `$SCRATCH/build/kernel-links.log`; 14 of the GNU calls are `-v`
version probes, the other 4 are the ELF32 links and `vmlinux.o -r`):

| Output | Script |
| --- | --- |
| `.tmp_vmlinux1`, `.tmp_vmlinux2`, `vmlinux.unstripped` (3 passes: kallsyms feeds the next) | `arch/x86/kernel/vmlinux.lds`, `--emit-relocs`, `--orphan-handling=error` |
| `arch/x86/boot/compressed/vmlinux` | `arch/x86/boot/compressed/vmlinux.lds`, `-pie` |
| `arch/x86/entry/vdso/vdso64/vdso64.so.dbg` | `vdso64.lds`, `-shared`, `--hash-style=both`, `-Bsymbolic` |
| a temporary `-shared --pack-dyn-relocs=relr` probe in `/tmp` | `scripts/tools-support-relr.sh` |

An incremental rerun relinks only those five outputs; the script removes
them first (make does not track the linker binary) and then checks that each
one was linked by qld.

## Layout comparison

Step 3 of the script replays the recorded `.tmp_vmlinux1` argv — the
vmlinux.lds link of `vmlinux.o`, `.vmlinux.export.o`,
`init/version-timestamp.o` and `.tmp_vmlinux0.kallsyms.o` — twice from the
same inputs, once with GNU ld and once with `qld -O2`, into
`$SCRATCH/build/kernel-compare`, and compares:

- **Allocated sections**: name, address and size of every section with the
  `A` flag (43 of them), sorted by name. They all match.
- **Every symbol**: `nm -n` reduced to address and name, compared as a whole
  file with `diff`. All **238 897** symbols have GNU ld's addresses.

`-O2` is passed to qld because GNU ld tail-merges `SHF_MERGE|SHF_STRINGS`
sections by default while qld only does it at `-O2`; without it `.rodata`
and everything after it are a few hundred bytes larger and every later
address shifts. That is a string-merge default, not a layout difference.

### Remaining differences

Both are symbol table details, not layout, and the script reports them
without failing:

- **17 `__pi_*` aliases** are `GLOBAL HIDDEN` in `vmlinux.o` and qld writes
  them to `.symtab` as `LOCAL HIDDEN` (`nm` prints `t`/`d` instead of
  `T`/`D`); GNU ld keeps the global binding. Addresses, sizes and types
  match. This is what the script's "nm lines differing in binding only: 17"
  line counts.
- **`.rela.*` section order**: with `--emit-relocs`, GNU ld puts each
  `.rela.foo` next to `foo` in the section header table, qld puts them all
  after the allocated sections. They are not allocated, so no address
  changes; `readelf -S` order and section indices differ. The kernel's
  `relocs` tool reads them by name and is unaffected — `vmlinux.relocs` and
  the resulting `bzImage` are correct.

## Boot

Step 4 boots `arch/x86/boot/bzImage` in QEMU with an initramfs holding one
statically linked program, which prints a marker with the running kernel's
release and version and powers off:

```
QLD KERNEL BOOT OK: 7.2.5 #4 SMP PREEMPT_DYNAMIC Mon Sep 14 22:49:37 JST 2026
```

The console log is kept in `$SCRATCH/build/kernel-boot/console.log`. It has
the same messages as a GNU-ld-linked build of the same tree (same defconfig,
`LD=ld.bfd`), including the same benign firmware and ACPI warnings. The step
is skipped when `qemu-system-x86_64`, `cpio` or a static libc is missing.

## Link time

The `.tmp_vmlinux1` link (90 MB of input objects, `--emit-relocs`, a 92 MB
output), best of three on a 64-core host: **qld 0.24 s, GNU ld 0.53 s**.

## Bugs found

Fixed during W19:

- Linker-script symbols at a section boundary (`_end`, `__bss_stop`,
  `VO__end`) were given the following section's index, so `objcopy`
  dropped them from `vmlinux.bin` and the boot stub failed to compile
  (`VO__end undeclared`). Fixed by `defined::linker_shndx` and GNU's
  `section_for_dot` rule.
- Thin archives (`vmlinux.a`) spell long-name offsets `/7388          /`;
  the archive reader rejected the trailing slash.
- Synthetic (linker-generated) sections were ordered after all objects
  instead of after the first object's sections, which shifted every
  address.
- `.bss` load addresses needed GNU's `lang_propagate_lma_regions`.

Still open, and reported with the W19 results: the `-r` objtool failure
(`src/elf/relocatable.rs`) and the `__pi_*` binding above
(`src/elf/symtab.rs`).
