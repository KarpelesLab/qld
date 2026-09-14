# Bare metal

The other M3 exit criterion: a microcontroller-style firmware image laid out
by a linker script, with the same addresses as GNU ld and a byte-identical
flat image.

- **Script:** `tests/projects/baremetal.sh QLD SCRATCH`
- **Inputs:** the fixture sources, assembled with `as` (no compiler driver,
  so nothing depends on gcc's defaults):
  `tests/fixtures/script-bare-metal-flash/{start.s,main.s,flash.ld}` and
  `tests/fixtures/script-static-phdrs/{start.s,layout.ld}`
- **Reference:** the GNU ld on `PATH` (`ld.bfd`; override with `GNU_LD=`)
- **Time:** under a second; no network access

## What it compares

1. **Flash image** (`flash.ld`): a Cortex-M-style layout on x86-64 — a
   vector table at the start of flash, code and read-only data in flash,
   `.data` running in RAM but loaded from flash (`>RAM AT> FLASH`), `.bss`
   `(NOLOAD)` in RAM. It exercises `MEMORY` with attributes, `ENTRY`,
   `KEEP`, `SORT_BY_INIT_PRIORITY`, `PROVIDE_HIDDEN`, a `=0x90909090` fill,
   `BYTE`/`SHORT`/`LONG`/`QUAD` data commands, `ALIGN`, `LOADADDR`,
   `ORIGIN`/`LENGTH`, `ASSERT` and `/DISCARD/`.
   - section headers and program headers (`readelf -SlW`, section numbering
     stripped): addresses, sizes, flags, file offsets, and each segment's
     `PhysAddr` — identical;
   - every symbol address (`nm -n`): 16 symbols, identical;
   - `--oformat binary`, `ihex` and `srec`: byte-identical (208, 737 and
     800 bytes). Both linkers are given the same output name, because an
     S-record file's S0 header record holds it.
2. **PHDRS executable** (`layout.ld`): a runnable Linux executable laid out
   entirely by a script with explicit `PHDRS` (`PT_PHDR`, `FILEHDR PHDRS`,
   `FLAGS`), `SIZEOF_HEADERS`, `SUBALIGN`, `SORT`/`SORT_BY_NAME`, `KEEP`,
   symbol arithmetic outside sections, `DEFINED`, `QUAD`, `PROVIDE` and
   `ASSERT`. Sections, segments and all 10 symbol addresses are identical,
   and both binaries print `linked by a script` and exit 37.

`.symtab`, `.strtab` and `.shstrtab` are excluded from the section
comparison: qld does not emit `STT_FILE` symbols yet, so those three
sections have different sizes (every other section matches exactly).

## What the fixtures already cover

Most of this also runs inside `cargo test`, which is where a regression
should be caught first:

| Check | Covered by |
| --- | --- |
| Flash-image addresses, sizes, LMAs, program headers, script symbols | `tests/fixtures/script-bare-metal-flash` (expectations taken from GNU ld's output) |
| Flat binary bytes, including fill and inter-section gaps | `tests/fixtures/raw-binary` |
| Intel HEX and S-record text, including CRLF and the 04/05/01 and S0/S3/S7 records | `tests/fixtures/raw-ihex-srec` |
| The PHDRS executable's layout and its exit status | `tests/fixtures/script-static-phdrs` |
| A live GNU-ld-versus-qld comparison of the same inputs (skipped when GNU ld is absent) | `tests/script_link.rs` (16 tests: `memory_regions_load_addresses_and_data_commands`, `program_headers_from_phdrs`, `overlays_and_expressions`, region overflow, `ASSERT`, `NOCROSSREFS`, and more) |
| Fixtures validated against GNU ld | `cargo test --test fixtures -- --ignored` |

`baremetal.sh` is the standalone form: it does the whole comparison against
the installed GNU ld in one run, prints what matched, and exits non-zero on
any difference — the milestone evidence without the test harness.

## Result

```
PASS: flash: same sections, segments and 16 symbol addresses
PASS: flash binary: identical (208 bytes)
PASS: flash ihex: identical (737 bytes)
PASS: flash srec: identical (800 bytes)
PASS: phdrs: same sections, segments and 10 symbol addresses
PASS: phdrs.gnu runs (exit 37)
PASS: phdrs.qld runs (exit 37)
bare metal: status 0
```

(GNU ld 2.46.1, binutils `as` 2.46.1.)

## Bugs found

Fixed during W19: `.bss` load-address propagation (`lang_propagate_lma_regions`),
`_estack`-style absolute symbols wrongly marked `ABS`, fill patterns of
value zero being dropped, code gaps padded with zeros instead of BFD's long
NOPs, and orphan placement for empty synthetic sections.
