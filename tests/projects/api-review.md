# Library API review for 1.0 (W36, milestone M9)

**Update (W41):** the five 1.0 blockers this review listed are settled; see
[What W41 changed](#what-w41-changed) at the end. The rows below are marked
**Done (W41)** where they were.

This review covers the public surface of the `qld` crate at `65d6536` plus
the W36 additions. It sorts every public item into one of three groups,
recommends what to do with each, and lists the concrete changes by owner.
The diffs proposed for the frozen files `src/lib.rs` and `src/error.rs` are
at the end.

## Method

The item list comes from
`cargo +nightly rustdoc --lib --all-features -- -Zunstable-options --output-format json`,
walked from the crate root through public modules. Associated items
(methods, fields, variants) are counted on their type rather than listed.
To see what the crate's own consumers depend on, `tests/`, `benches/` and
`src/main.rs` were searched for `qld::` paths.

| Kind | Count |
| --- | ---: |
| Modules | 191 |
| Structs | 369 |
| Enums | 130 |
| Traits | 9 |
| Functions | 336 |
| Type aliases | 8 |
| Constants and statics | 1,334 |
| Re-exports (`pub use`) | 442 |
| **Items reachable from the root** | **2,819** |

These items fall into a few large groups:

| Area | Items | Of which constants |
| --- | ---: | ---: |
| `elf::**` | 1,006 | 674 (613 in `elf::read::consts`) |
| `macho::**` | 610 | 366 (348 in `macho::read`) |
| `coff::**` | 442 | 262 (245 in `coff::read`) |
| `args::**` | 69 | 4 |
| `input::**` | 55 | 17 |
| `passes::**` | 39 | 0 |
| `debug::**` | 36 | 5 |
| `arch::**` | 34 | 13 |
| everything else | 528 | – |

About 95% of the surface is backend internals that are public for
integration tests, not for users. Keeping it in the 1.0 semver contract
would freeze qld's internals.

## Classification

- **Stable**: part of the 1.0 contract. Breaking changes need a major
  version.
- **Unstable**: useful to tools (object readers, the demangler, the hint
  engine), but not ready to freeze. Keep it public, but mark it
  `#[doc(hidden)]` and leave it out of semver until it is reviewed on its
  own.
- **Internal / accidental**: public only so that qld's own tests or sibling
  modules can reach it. Make it `pub(crate)` once the tests stop using it;
  until then, `#[doc(hidden)]`.

A Cargo feature (`unstable = []`, with `#[cfg_attr(not(feature =
"unstable"), doc(hidden))]`) would say this more explicitly, but it changes
`Cargo.toml` and needs `required-features` on a dozen integration tests.
Plain `#[doc(hidden)]` is enough for 1.0. The feature can come later without
breaking anyone.

## Proposed 1.0 surface

```text
qld::link, qld::link_to_memory (new), qld::version_line, qld::PROGRAM_NAME
qld::parse_gnu, qld::parse_gnu_with, qld::ParseOutcome
qld::{LinkOptions, InputKind, InputAttrs, OutputKind, OutputBuffer, CancelToken}
qld::{InputProvider, MemoryFiles}
qld::{Diagnostic, DiagnosticSink, Severity}, qld::diag::{Collect, Stderr, Location, SourceLocation}
qld::{Error, Result, ScriptError}
qld::{Target, BinaryFormat, Architecture, Endianness, OperatingSystem, PointerWidth}
qld::args::* (option model, parsers, FileReader) minus args::{table, emulation, response internals}
```

The rest stays reachable, marked `#[doc(hidden)]`. The `src/lib.rs` diff at
the end does exactly this.

---

## Crate root

| Item | Class | Recommendation |
| --- | --- | --- |
| `link` | stable | Keep. Add a check at entry for `LinkOptions::cancel`, and reject an `output_buffer` for formats whose driver ignores it (lib.rs diff). Add a rustdoc example (lib.rs diff). |
| `link_to_memory` *(new)* | stable | Add (lib.rs diff): `link` with a fresh `OutputBuffer`, returning `Vec<u8>`. |
| `version_line`, `PROGRAM_NAME` | stable | Keep. The wording of `version_line` is part of GNU compatibility and is already documented as fixed. |
| `LinkOptions`, `ParseOutcome`, `parse_gnu`, `parse_gnu_with` (re-exports) | stable | Keep. Also re-export `InputKind`, `InputAttrs`, `OutputKind`, `OutputBuffer`, `CancelToken`, `InputProvider`, `MemoryFiles` and `ScriptError`, so that a typical caller needs only `qld::*` (lib.rs diff). |
| `Diagnostic`, `DiagnosticSink`, `Severity` | stable | Keep. |
| `Error`, `Result` | stable | Keep. Add `Error::Cancelled` (error.rs diff). |
| `FileId`, `SectionId`, `SymbolId` | internal | No stable signature uses them. Make the re-exports `#[doc(hidden)]` now (lib.rs diff) and remove them after `tests/passes.rs` and `tests/symbols.rs` import them from `qld::ids`. |
| `Target`, `BinaryFormat`, `Architecture`, `Endianness`, `OperatingSystem`, `PointerWidth` | stable | Keep. |
| module `args` | stable | See below. |
| module `diag` | stable | See below. |
| module `error` | stable | Keep (its two items are re-exported at the root). |
| module `target` | stable | See below. |
| modules `arch`, `coff`, `debug`, `demangle`, `elf`, `hints`, `ids`, `input`, `macho`, `output`, `passes`, `plugin`, `script`, `symbols` | unstable / internal | `#[doc(hidden)]` (lib.rs diff). Per-module notes below. |

## `args` (69 items)

### `args::options`: the option model

| Item | Class | Recommendation |
| --- | --- | --- |
| `LinkOptions` (129 pub fields, 8 methods) | stable | **Done (W41): `#[non_exhaustive]`**, and the test literals are gone. Original note: **Add `#[non_exhaustive]`** so that fields can be added in minor releases. Field assignment still works on a non-exhaustive struct; only struct literals and `..Default::default()` stop working outside the crate. **Blocker:** `tests/coff_link.rs` builds `LinkOptions { .. }` literals at lines 406, 950, 958, 965 and 972. They must switch to `LinkOptions::new()` plus assignments before the attribute can land. A full builder is not needed: `new()` plus public fields, `push_input` and a few helpers is simpler and just as future-proof once the struct is non-exhaustive. |
| `LinkOptions::default()` vs `LinkOptions::new()` | accidental | **Done (W41):** `Default` is `new()`. The derived `Default` is gone; `new()` builds on a private `LinkOptions::blank()` that lists every field, so a new field names its default there. Original note: **Footgun:** the derived `Default` turns `demangle`, `relro`, `gnu_stack`, `copy_relocs`, `combine_relocs`, `extern_protected_data`, `section_header`, `relax`, `dependent_libraries` and `fork` off, while `new()` turns them on. Make `Default` return `new()` by writing out `new()`'s field list, or change those fields to `Option<bool>`, with `None` meaning the default. This needs agreement: every workstream that adds a field relies on `..Self::default()` in `new()`, and `tests/coff_link.rs` uses `..LinkOptions::default()`. |
| `LinkOptions` string-typed fields: `icf`, `orphan_handling`, `sort_section`, `compress_debug_sections`, `start_stop_visibility`, `output_format` | stable, needs change | **Done (W41):** `IcfMode`, `OrphanHandling`, `SortSection`, `DebugCompression`, `Visibility` and `OutputFormat`. `--oformat` names a BFD target, so `OutputFormat` keeps `binary`/`ihex`/`srec` as variants and every other name as `OutputFormat::Bfd(String)`, which the format driver checks. Original note: Change them to enums before 1.0 (`IcfMode`, `OrphanHandling`, `SortSection`, `DebugCompression`, `Visibility`, `OutputFormat`). A string cannot be validated when the options are built, and the drivers match on literals. |
| `LinkOptions::{fork, exit_on_plugin_fatal, on_output_complete}` | internal | Only the binary uses these. Make them `#[doc(hidden)]` and keep them out of semver. They could move into a `ProcessOptions` that `main.rs` owns. |
| `LinkOptions::{warnings, ignored}` | stable, should move | These are outputs of parsing, not options. Before 1.0, move them to a `Parsed { options, warnings, ignored }` in `ParseOutcome::Link`. Until then, `examples/link_argv.rs` shows the caller emitting them, as `main.rs` does. |
| `LinkOptions::{input_provider, output_buffer, cancel}` *(new, W36)* | stable | Added. |
| `LinkOptions::darwin.inputs` vs `LinkOptions::inputs` | stable, needs change | **Done (W41):** one list. `DarwinArgs::inputs`, `DarwinInput` and `DarwinInputKind` are gone; the ld64 front end fills `LinkOptions::inputs`, with `InputKind::Framework`, `InputAttrs::load` (`LoadMode`) and `InputAttrs::whole_archive` (`-force_load`). `InputKind::Bytes` works for Mach-O. Original note: Mach-O links read a separate input list (`DarwinArgs::inputs`, `DarwinInputKind`), so `InputKind::Bytes` and `push_input` have no effect on a Mach-O link. Before 1.0, merge them into one list: add `Framework`/`WeakLibrary`/… as `InputKind` variants or as attributes. |
| `LinkOptions::threads` | stable | **Done (W41):** the doc says what the driver does, including that a caller's pool is used as it is. |
| `LinkOptions::{output_complete, output_path, is_pic, is_dynamic, push_input, resolve_sysroot, check_cancelled}` | stable | Keep. `output_complete` is internal to the drivers: make it `#[doc(hidden)]`. |
| `InputKind` (non_exhaustive, 6 variants) | stable | Keep. `InputKind::bytes()` added. `Bytes::name` is a `String` while `Source::Bytes::name` is a `PathBuf`: pick one (`PathBuf`, as diagnostics treat it as a path) before 1.0. |
| `InputSpec` | stable | **Done (W41): `#[non_exhaustive]`**, with `InputSpec::new` for a spec outside a list; `position` is still a field, documented as assigned by `push_input`. Original note: Add `#[non_exhaustive]` once `tests/input.rs:561` and `tests/coff_link.rs:411,511,1059,1219` stop building literals. `position` is assigned by `push_input`, so it should become read-only, through an accessor. |
| `InputAttrs` | stable | **Done (W41): `#[non_exhaustive]`**, and it grew `load` for the Mach-O load modes. |
| `PeArgs`, `DynamicFlags`, `X86Features` | stable | **Done (W36): `#[non_exhaustive]`.** No external code builds them with literals. |
| `StripMode`, `DiscardMode`, `HashStyle`, `SymbolicMode`, `UnresolvedSymbols`, `ColorChoice`, `MagicMode`, `SeparateCode`, `ExecStack`, `ReportLevel`, `InputFormat` | stable | **Done (W36): `#[non_exhaustive]`.** Nothing outside the crate matches them exhaustively. `InputFormat` will get more `-b` formats. |
| `Flavor`, `OutputKind`, `BuildId` | stable | Already `#[non_exhaustive]`. |
| `OutputCompleteHook` | internal | Used only for `--fork`. `#[doc(hidden)]`. |
| `OutputBuffer`, `CancelToken` *(new)* | stable | Added. Both are opaque handles that compare by identity. |
| `sysroot_relative` | internal | Make it `pub(crate)`: only the drivers use it. |

### `args::parse` and `args::response`

| Item | Class | Recommendation |
| --- | --- | --- |
| `ParseOutcome` | stable | Add `#[non_exhaustive]`. Needs a wildcard arm in `src/main.rs:73` (frozen), `tests/corpus.rs:121` and `tests/args.rs:238`. Box the `Parsed` struct described above. |
| `parse_gnu`, `parse_gnu_with`, `parse_darwin`, `parse_darwin_with` | stable | Keep. |
| `select_flavor` | stable | Keep. |
| `usage` | unstable | This is the CLI's `--help` text; keep it, but `#[doc(hidden)]`. |
| `FileReader`, `FsReader`, `NoFiles` | stable | Keep: they are the hook for hermetic response-file reading. |
| `Quoting`, `response::{tokenize, expand, quoting_from_args}` | internal | Make the `response` module private and keep only the three types above public. |

### `args::table`, `args::emulation`, `args::darwin`

| Item | Class | Recommendation |
| --- | --- | --- |
| `table::{GNU_OPTIONS, Z_KEYWORDS, find, find_z, OptionDef, ZKeyword, ArgKind, ZArg, Status}` | internal | `tests/args.rs` enumerates the table, as the W1 brief intends. Make the module `#[doc(hidden)] pub mod table`. |
| `emulation::{EMULATIONS, lookup}` | internal | `pub(crate)`: no external user. |
| `darwin::{DarwinArgs, DarwinInput, DarwinInputKind, PlatformVersion, LoadMode, MachOutputType, UndefinedTreatment, UuidMode}` | stable (as part of `LinkOptions`) | Needed because `LinkOptions::darwin` is public. Add `#[non_exhaustive]` to `DarwinArgs` and to the enums (W34). |
| `darwin::{DARWIN_OPTIONS, find_option, darwin_usage, parse, DarwinOption, DarwinArg}` | internal | `#[doc(hidden)]` or `pub(crate)` (W34). `tests/macho_link.rs` does not use them. |

## `diag` (7 items)

| Item | Class | Recommendation |
| --- | --- | --- |
| `DiagnosticSink` | stable | Keep. `error_count` is a required method that nothing in the library calls (only a unit test). Give it a default body (`0`) or remove it before 1.0, so that a sink is just `emit`. |
| `Diagnostic` | stable | Add `#[non_exhaustive]`. It already has constructors and builder methods (`new`, `error`, `warning`, `at`, `detail`, `note`, `order`), and the linker may add fields (source spans, codes). |
| `Location` | stable | Add `#[non_exhaustive]` plus a constructor (`Location::file(path)` and builder methods): today it can only be built with a struct literal. |
| `SourceLocation` | stable | `#[non_exhaustive]` plus a constructor. |
| `Severity` | stable | Keep it exhaustive. Three levels is the model GNU tools use. |
| `Collect` | stable | Keep. `take_sorted` panics on a poisoned lock; recover with `into_inner` instead, as `OutputBuffer` does. |
| `Stderr` | stable | Keep. It is what `examples/link_argv.rs` uses. |

## `error` (2 items)

| Item | Class | Recommendation |
| --- | --- | --- |
| `Error` (non_exhaustive, 10 variants) | stable | Add `Cancelled` (error.rs diff). `Script(Box<script::ScriptError>)` puts a type from an otherwise internal module into a stable enum: re-export `ScriptError` at the root (lib.rs diff) and treat it as stable. Consider `#[non_exhaustive]` on the struct variants (`Malformed`, `Io`, `Reported`), so that fields such as a source span can be added. |
| `Result` | stable | Keep. |

## `target` (6 items) and `ids` (3 items)

| Item | Class | Recommendation |
| --- | --- | --- |
| `Target` (5 pub fields, 3 methods) | stable | Add `#[non_exhaustive]` (ABI variants such as soft-float and the RISC-V float ABI will need fields). Callers then build targets from the associated constants (`Target::X86_64_LINUX`, …) and struct update syntax inside the crate, or through a constructor to add. |
| `BinaryFormat`, `Architecture`, `OperatingSystem` | stable | Already non_exhaustive. |
| `Endianness`, `PointerWidth` | stable | Keep them exhaustive. |
| `ids::{FileId, SectionId, SymbolId}` | internal | `#[doc(hidden)]`. |

## `input` (55 items)

| Item | Class | Recommendation |
| --- | --- | --- |
| `input::source::{InputProvider, MemoryFiles}` *(new, W36)* | stable | Re-export them at the root (lib.rs diff). The trait has one required method, `read`, and one provided method, `contains`. |
| `FileTable`, `InputFile`, `Source`, `MemberEntry` | internal | These are the drivers' data model. `FileTable::for_link` and `FileTable::provider` were added by W36. `#[doc(hidden)]`. |
| `search::{FileSystem, RealFileSystem, SearchContext, LibraryNaming, apply_sysroot}` | unstable | A future post-1.0 extension point (virtual file systems for `-l` and scripts). For now `InputProvider` covers that use case. |
| `archive::*`, `identify::*` (including 15 constants in `identify::{elf_type, coff_machine}`) | unstable | Useful to tools (an `ar` reader, format sniffing), but not reviewed for 1.0. |

## `output` (7 items plus 16 re-exports)

| Item | Class | Recommendation |
| --- | --- | --- |
| `OutputFile`, `OutputOptions`, `Finished`, `BackingPolicy`, `FileMode`, `ReplaceStrategy`, `Backing`, `WritePhase`, `WriteStats`, `ChunkRange`, `LayoutError`, `split_chunks`, `validate_layout`, `write_chunks` | internal | The drivers' writer. `OutputOptions` gained `capture`, `cancel` and `for_link` (W36). `#[doc(hidden)]`. The environment variable `QLD_OUTPUT_BACKING` is read in the library (`BackingPolicy::Auto`); it should move to `main.rs` or become an explicit option, so that library links do not depend on the environment. |
| `build_id::*`, `hash::*` | internal | Used by `tests/output.rs`. `#[doc(hidden)]`. |

## Backends and passes

Everything below is internal. `tests/` uses the modules shown, which is why
they stay `pub` (hidden) rather than `pub(crate)`.

| Module | Items | Used by | Recommendation |
| --- | ---: | --- | --- |
| `elf` (31 submodules) | 1,006 | `tests/{elf_link,script_link,debug,plugin,lto,elf_read}.rs` | `#[doc(hidden)]`. `elf::link` duplicates `qld::link`; the tests call it to skip `--threads` handling. Every submodule except `read` and `link` should become `pub(crate)` in `src/elf/mod.rs` (W8 successors). Only `tests/lto.rs` reaches `elf::inputs` and `elf::lto`. |
| `elf::read` | 674 | `tests/{elf_read,debug,plugin}.rs` | Unstable: an ELF reader is a plausible public API later. Group the 613 constants under `consts` (done) and keep it hidden for 1.0. |
| `coff` (15 submodules) | 442 | `tests/{coff_link,coff_read,args}.rs` | `#[doc(hidden)]`. `coff::link_with(options, &PeOptions, …)` exists because `LinkOptions` did not carry the PE options, but `LinkOptions::pe` now does: fold `link_with` into `link` (W33). |
| `macho` (27 submodules) | 610 | `tests/{macho_link,macho_read}.rs` | `#[doc(hidden)]`. `macho::link_to_bytes` duplicates the new `OutputBuffer`: once `macho::link` stores into the buffer, `link_to_bytes` can become `pub(crate)` (W34). |
| `passes` | 39 | `tests/passes.rs` | `#[doc(hidden)]`. |
| `symbols` | 18 + 22 re-exports | `tests/symbols.rs` | `#[doc(hidden)]` (W28 is redesigning it). |
| `script` | 1 + 60 re-exports | `tests/script.rs` | `#[doc(hidden)]`, except `ScriptError`, which `Error::Script` exposes: re-export it at the root. |
| `debug` | 36 | `tests/debug.rs` | `#[doc(hidden)]`. |
| `arch` | 34 | – | `#[doc(hidden)]`, then `pub(crate)`. |
| `demangle` | 16 | `tests/demangle.rs` | Unstable. A demangler is useful on its own; review it separately after 1.0. |
| `hints` | 12 + 9 re-exports | `tests/hints.rs` | `#[doc(hidden)]`. |
| `plugin` | 7 + 19 re-exports | `tests/{plugin,lto}.rs` | `#[doc(hidden)]`. |

## Behavior the 1.0 API promises but the code does not deliver yet

`docs/library-api.md` lists requirements. These places break them:

| Requirement | Where it breaks | Recommendation (owner) |
| --- | --- | --- |
| No printing | **Done (W41)** for `-M`/`--cref` (`LinkOptions::map_output`) and the `QLD_TIMING` laps of `src/elf/{link,lto}.rs` (`LinkOptions::timing`), both `TextOutput` sinks that are `None` by default. `--print-gc-sections` and `--print-icf-sections` already went to the diagnostic sink. **Remaining:** `src/symbols/resolve.rs:378,405` still reads `QLD_TIMING` and prints its own laps — `resolve_symbols` takes no options, so the flag needs a provided `RoundHook::timing()` (W28's file). `src/plugin/host.rs:1484` prints a plugin message that arrives after the session is gone, when there is no sink to send it to. |
| No `process::exit` | `src/elf/lto.rs:364`, gated by `exit_on_plugin_fatal`, which only the binary sets | Fine as is; keep the flag `#[doc(hidden)]`. |
| No environment dependence | **Done (W41):** the drivers read no variable. `LinkOptions::{env_run_path, env_library_path, zero_ar_date, output_backing, timing}` carry what used to come from `LD_RUN_PATH`, `LD_LIBRARY_PATH`, `ZERO_AR_DATE`, `QLD_OUTPUT_BACKING` and `QLD_TIMING`, and `LinkOptions::use_process_defaults()` is the documented opt-in that fills them. `parse_gnu` and `parse_darwin` call it, because they describe the link the `qld` binary runs; `parse_gnu_with`, `parse_darwin_with` and `LinkOptions::new` leave a link hermetic. (`src/symbols/resolve.rs` still reads `QLD_TIMING`; see the row above.) |
| Caller-controlled threading | **Done (W41):** a pool the caller installed is used as it is, whatever its size, and the driver builds none of its own in it. The driver tells the two cases apart by `LinkOptions::threads`: `Some(n)` means the surrounding pool is the one `qld::link` built for this link, which qld may narrow; `None` inside a pool means the caller's. A caller who wants qld's tuned pools sets `threads` instead. No new option was needed. |
| In-memory inputs and outputs | PE (W33) and Mach-O (W34/W35) now honour `output_buffer`. W41 made `InputKind::Bytes` and `LinkOptions::inputs` work for Mach-O as well. |
| Cancellation | Resolution (`src/elf/lto.rs`, `src/symbols/resolve.rs`) and the relocation scan do not check the token internally | Measured with `examples/cancel.rs` on the `clang` link (180–190 ms): a token cancelled at t = 30 ms made the link return at 110 ms, and one cancelled later made it return 25–55 ms after cancellation, including freeing the link's data. Resolution is the longest unchecked stage. W28 can add a check between resolution rounds. |

## What W36 changed

- `src/args/options.rs` (API only): `OutputBuffer`, `CancelToken`, the
  fields `input_provider`, `output_buffer` and `cancel`,
  `LinkOptions::check_cancelled` and `InputKind::bytes`; `#[non_exhaustive]`
  on 11 enums and 3 flag structs. `src/args/mod.rs` re-exports the two new
  types.
- `src/input/source.rs` (new): `InputProvider`, `MemoryFiles`,
  `impl FileSystem for FileTable` (the provider first, then the disk) and a
  blanket `impl FileSystem for &T`.
- Hooks where inputs are opened: `FileTable::for_link` in
  `src/input/table.rs` (every path load goes through `FileTable::open`,
  which checks the token, asks the provider, then maps the file); three
  one-line changes in `src/elf/inputs.rs` (library search goes through the
  table).
- Hooks where the output is created: `OutputOptions::{capture, cancel,
  for_link}` in `src/output/file.rs` (a captured output is built in memory
  and stored on `finish`; `write_chunks` checks the token per chunk,
  `create` and `finish` check it once); three one-line changes in
  `src/elf/{write,relocatable}.rs`.
- Cancellation checks between stages: eleven `options.check_cancelled()?`
  lines in `src/elf/link.rs`, one after each timing lap except the final
  write.
- `examples/`: `link_argv`, `in_memory`, `custom_sink`, `rayon_pool` and
  `cancel`, plus `examples/support/objects.rs`, which builds x86-64 ELF
  objects and `ar` archives in memory (the tests share it).
- `tests/api.rs`: 13 tests. They cover memory output for executables, `-r`
  and `--oformat binary`; memory output identical to file output; argv plus
  `MemoryFiles` with `-L`/`-l` and an input script; cancellation before and
  during a link; an unused token; a custom sink; and concurrent links in a
  caller's pool. On x86-64 Linux, each linked program is run and its exit
  status checked.

Overhead, measured as interleaved A/B runs of the `qld` binary with no token
set: `clang`, 191.3 ms against 192.3 ms wall and 1,627 ms against 1,578 ms
CPU; `clang-debug` (1.2 GiB output), 976 ms against 912 ms wall and
14,538 ms against 14,470 ms CPU. Both are within noise. `perf` is not
installed, so there are no instruction counts.

## Change list for other owners

| # | Owner | File | Change |
| --- | --- | --- | --- |
| 1 | integrator | `src/lib.rs` | The diff below: hide internal modules, root re-exports, `link_to_memory`, a cancellation check and an `output_buffer` guard in `link`, rustdoc examples. |
| 2 | integrator | `src/error.rs` | The diff below: `Error::Cancelled`. Then apply the `options.rs` follow-up below, so that `CancelToken::error()` returns it. |
| 3 | W38 / W1 | `src/script/mod.rs:21`, `src/output/hash/mod.rs:7` | `[`eval`]` → `[`eval()`]` and `[`xxh64`]` → `[`xxh64()`]`. The links become ambiguous (module or function) once the parent modules are `#[doc(hidden)]`, and `cargo doc -D warnings` fails. Needed together with #1. |
| 4 | W33 | `src/coff/write.rs:211`, `src/coff/link.rs` | Output: build `OutputOptions` from `OutputOptions::for_link(link_options)` (write.rs has only `PeOptions`, so pass the `LinkOptions` or the buffer through `WriteInput`). Inputs: `FileTable::new()` → `FileTable::for_link(options)` at `link.rs:90,118`, and `let fs = RealFileSystem;` → `let fs = table;` in `src/coff/inputs.rs:217`. Cancellation: `options.check_cancelled()?` between stages in `link_with`. Then drop the `output_buffer` guard in `lib.rs` for PE. |
| 5 | W34 / W35 | `src/macho/link.rs:69-84` | In `macho::link`: `if let Some(buffer) = &options.output_buffer { buffer.store(bytes); return Ok(()); }` before `OutputFile::create`, or `..OutputOptions::for_link(options)` there. Inputs: `FileTable::new()` → `FileTable::for_link(options)` at `link.rs:230`. The `std::fs::read` calls at `link.rs:129,150,330`, `inputs.rs:1101`, `layout.rs:324` and `symtab.rs:129` should go through the provider. Merge `DarwinArgs::inputs` into `LinkOptions::inputs` so that `InputKind::Bytes` works. |
| 6 | W28 | `src/elf/lto.rs`, `src/symbols/resolve.rs` | `options.check_cancelled()?` after each resolution round and after the laps in `lto.rs` (`first resolution`, `code generation`, `second resolution`). |
| 7 | W38 | `src/elf/script_layout/load.rs:116,121,227,453`, `src/script/parser.rs:80` (`FsReader`) | Read `-T` scripts and `INCLUDE`d files through `options.input_provider` first (`provider.read(path)` before `std::fs::read`). |
| 8 | any `src/elf` owner | `src/elf/export.rs:248,265,273` | Version scripts, dynamic lists and export lists: the same provider-first read. |
| 9 | ~~W9~~ W41 | `tests/coff_link.rs`, `tests/coff_link_arm64.rs`, `tests/input.rs` | **Done (W41).** |
| 10 | W9 | `tests/passes.rs:19`, `tests/symbols.rs:24` | Import `FileId`/`SectionId`/`SymbolId` from `qld::ids`, so that the root re-exports can go. |
| 11 | ~~W1~~ W41 | `src/args/options.rs` | **Done (W41)** except moving `warnings`/`ignored` into `ParseOutcome`, which waits for the `#[non_exhaustive] ParseOutcome` of row 12. |
| 12 | W1, integrator | `src/args/parse.rs`, `src/main.rs` | `#[non_exhaustive]` on `ParseOutcome`, with a wildcard arm in `main.rs`. |
| 13 | integrator | `src/diag.rs` | `#[non_exhaustive]` plus constructors for `Diagnostic`, `Location` and `SourceLocation`; a default body for `DiagnosticSink::error_count`; no panic in `Collect::take_sorted`. |
| 14 | integrator | `src/target.rs` | `#[non_exhaustive]` on `Target`. |
| 15 | ~~W5 successor~~ W41 | `src/output/file.rs`, `src/elf/{link,lto}.rs`, `src/elf/map.rs` | **Done (W41).** `BackingPolicy::resolve` no longer reads the environment and `OutputOptions::for_link` takes the backing from `LinkOptions::output_backing`. |

## What W41 changed

The five 1.0 blockers, in `src/args/options.rs` unless another file is
named.

1. **`Default` is `new()`.** The derived `Default` left ten options off
   that `new()` turns on (`-z relro`, `--demangle`, `--relax`,
   `-z copyreloc`, `-z combreloc`, `-z extern-protected-data`,
   `-z sectionheader`, `-z gnustack`, `--dependent-libraries`, `--fork`),
   so two constructors described two different links. `Default` now
   delegates to `new()`, which builds on a private `LinkOptions::blank()`
   listing every field. Adding a field is a compile error in `blank()`
   until its default is named there — which is where a field's default
   belongs. A unit test compares `Default::default()` with `new()`.

2. **Six string options became enums**: `IcfMode`, `OrphanHandling`,
   `SortSection`, `DebugCompression`, `Visibility` and `OutputFormat`.
   `--oformat` names a BFD target, so `OutputFormat` has variants for the
   three raw formats and `Bfd(String)` for the rest, with `from_name`,
   `name` and `is_raw`. The parser takes the same spellings and produces
   the same errors; `tests/args.rs` has a test that pins every spelling and
   every rejected value.

3. **One input list for every format.** `DarwinArgs::inputs`,
   `DarwinInput` and `DarwinInputKind` are gone. The ld64 front end fills
   `LinkOptions::inputs`, with `InputKind::Framework { name, suffix }`,
   `InputAttrs::load` (`LoadMode`, re-exported from `args`) and
   `InputAttrs::whole_archive` for `-force_load`. `src/macho/inputs.rs`
   resolves each input to a `Source`, so `InputKind::Bytes` links on
   Mach-O too, and `-arch`/`-platform_version` inference reads in-memory
   inputs. `src/macho/lto/driver.rs`'s `Spec::Gnu`/`Spec::Darwin` index
   pair collapsed to one index. `tests/macho_link.rs` links the same
   program from a file and from bytes and requires the same image.

4. **No printing, no environment reads.** `LinkOptions::map_output` and
   `LinkOptions::timing` are `TextOutput` sinks (`None` by default) for the
   `-M`/`--cref` text and the stage laps; `env_run_path`,
   `env_library_path`, `zero_ar_date` and `output_backing` carry what the
   drivers used to read from the environment.
   `LinkOptions::use_process_defaults()` fills all of them from the
   process, and `parse_gnu`/`parse_darwin` call it, so the CLI is
   unchanged while `LinkOptions::new()` describes a hermetic, silent link.

5. **Nested thread pools.** In a pool the caller installed, the ELF driver
   no longer builds a 16-thread "narrow" pool or an input-sized pool: it
   uses the caller's pool as it is. `--threads` still means the pool
   belongs to the link, so CLI behavior is unchanged.
   `tests/api.rs` pins it with an input provider that records
   `rayon::current_num_threads()`.

6. **`#[non_exhaustive]`** on `LinkOptions`, `InputSpec` and `InputAttrs`,
   with `InputSpec::new`; the struct literals in `tests/coff_link.rs`,
   `tests/coff_link_arm64.rs` and `tests/input.rs` are gone.

New public items in `qld::args`: `TextOutput`, `OutputBacking`, `IcfMode`,
`OrphanHandling`, `SortSection`, `DebugCompression`, `Visibility`,
`OutputFormat`, `LoadMode` (re-exported from `args::darwin`),
`InputSpec::new`, `LinkOptions::use_process_defaults`.

Cost: the `clang` link measured with
`valgrind --tool=callgrind` on `~/.cache/qld-bench/specs/clang/link.json`
(`--no-fork --threads=1`) went from 4,998,368,098 to 4,998,963,517
instructions, +0.012%, with the same output hash (`ffad1ac52df901c0`).

Still open for 1.0, in rough order of value:

- `ParseOutcome` is not `#[non_exhaustive]`, and `warnings`/`ignored` are
  still options rather than parse results (rows 11 and 12).
- `#[non_exhaustive]` and constructors for `Diagnostic`, `Location`,
  `SourceLocation` and `Target` (rows 13 and 14).
- `InputKind::Bytes::name` is a `String` while `Source::Bytes::name` is a
  `PathBuf`.
- `src/symbols/resolve.rs` still reads `QLD_TIMING` on its own.
- `DarwinArgs` and the `args::darwin` enums still need
  `#[non_exhaustive]`.

## Semver policy to adopt at 1.0

- Items documented on docs.rs are covered by semver. `#[doc(hidden)]`
  items, and every module the crate docs call internal, are not.
- Adding a field to `LinkOptions` or a variant to a `#[non_exhaustive]`
  enum is a minor change. Changing a field's type or its default is a major
  change.
- The output of a given link may change in minor releases (layout
  improvements). Determinism across thread counts is guaranteed.

---

## Proposed diff: `src/lib.rs`

Verified by applying it together with the `error.rs` diff and fix #3 on top
of the W36 branch: `cargo clippy --all-targets --all-features -D warnings`,
`cargo doc -D warnings`, `cargo test --doc`, `tests/api.rs` and
`cargo +1.89 check` all pass.

```diff
diff --git a/src/lib.rs b/src/lib.rs
index 1f512f6..f3bb8ab 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -7,6 +7,32 @@
 //! `ROADMAP.md`. Nothing outside this crate's root re-exports should be
 //! considered stable, and the root API will not be stable until 1.0.
 //!
+//! # Library use
+//!
+//! Build [`LinkOptions`] by hand or from a command line ([`parse_gnu`]), and
+//! call [`link`]. Diagnostics go to a [`DiagnosticSink`] of your choice;
+//! nothing is printed and the process never exits. Inputs can be byte
+//! buffers ([`InputKind::bytes`], [`MemoryFiles`]), the output can come back
+//! as bytes ([`link_to_memory`], [`OutputBuffer`]), a link can be cancelled
+//! from another thread ([`CancelToken`]), and it runs in the caller's rayon
+//! pool when called inside [`rayon::ThreadPool::install`].
+//!
+//! ```no_run
+//! use qld::{InputAttrs, InputKind, LinkOptions, OutputKind};
+//! use qld::diag::Collect;
+//!
+//! # let object: Vec<u8> = Vec::new();
+//! let mut options = LinkOptions::new();
+//! options.kind = OutputKind::StaticExecutable;
+//! options.push_input(InputKind::bytes("main.o", object), InputAttrs::default());
+//! let diagnostics = Collect::new();
+//! let image: Vec<u8> = qld::link_to_memory(&options, &diagnostics)?;
+//! # Ok::<(), qld::Error>(())
+//! ```
+//!
+//! The `examples/` directory has complete programs: `link_argv`,
+//! `in_memory`, `custom_sink`, `rayon_pool` and `cancel`.
+//!
 //! # Layout
 //!
 //! Modules follow the link pipeline described in `docs/architecture.md`:
@@ -28,33 +54,57 @@
 //!
 //! Format backends own their own symbol precedence and layout rules. The
 //! shared modules must not depend on a backend.
+//!
+//! Only [`args`], [`diag`], [`error`] and [`target`] are documented; the
+//! other modules are public for qld's own tests and tools, and are not
+//! covered by semantic versioning.
 
 #![deny(unsafe_code)]
 
+#[doc(hidden)]
 pub mod arch;
 pub mod args;
+#[doc(hidden)]
 pub mod coff;
+#[doc(hidden)]
 pub mod debug;
+#[doc(hidden)]
 pub mod demangle;
 pub mod diag;
+#[doc(hidden)]
 pub mod elf;
 pub mod error;
+#[doc(hidden)]
 pub mod hints;
+#[doc(hidden)]
 pub mod ids;
+#[doc(hidden)]
 pub mod input;
+#[doc(hidden)]
 pub mod macho;
+#[doc(hidden)]
 pub mod output;
+#[doc(hidden)]
 pub mod passes;
 #[cfg(feature = "plugin")]
+#[doc(hidden)]
 pub mod plugin;
+#[doc(hidden)]
 pub mod script;
+#[doc(hidden)]
 pub mod symbols;
 pub mod target;
 
-pub use args::{LinkOptions, ParseOutcome, parse_gnu, parse_gnu_with};
+pub use args::{
+    CancelToken, InputAttrs, InputKind, LinkOptions, OutputBuffer, OutputKind, ParseOutcome,
+    parse_gnu, parse_gnu_with,
+};
 pub use diag::{Diagnostic, DiagnosticSink, Severity};
 pub use error::{Error, Result};
+#[doc(hidden)]
 pub use ids::{FileId, SectionId, SymbolId};
+pub use input::source::{InputProvider, MemoryFiles};
+pub use script::ScriptError;
 pub use target::{Architecture, BinaryFormat, Endianness, OperatingSystem, PointerWidth, Target};
 
 /// Program name used to prefix diagnostics.
@@ -86,12 +136,33 @@ pub fn version_line() -> String {
 /// unmapping the inputs, and `link` runs it before returning `Ok` if the
 /// driver did not.
 ///
+/// # Example
+///
+/// ```no_run
+/// use qld::diag::Stderr;
+///
+/// let args = ["ld", "-o", "hello", "crt1.o", "hello.o", "-lc"];
+/// if let qld::ParseOutcome::Link(options) = qld::parse_gnu(&args)? {
+///     qld::link(&options, &Stderr::new(qld::PROGRAM_NAME))?;
+/// }
+/// # Ok::<(), qld::Error>(())
+/// ```
+///
 /// # Errors
 ///
 /// Returns any fatal error from the link, including
-/// [`Error::Unimplemented`] for targets and features not supported yet.
+/// [`Error::Unimplemented`] for targets and features not supported yet,
+/// [`Error::Reported`] when errors were reported to `diagnostics`, and
+/// [`Error::Cancelled`] when [`LinkOptions::cancel`] was cancelled.
 pub fn link(options: &LinkOptions, diagnostics: &dyn DiagnosticSink) -> Result<()> {
-    let run = || match options.target.map(|target| target.format) {
+    options.check_cancelled()?;
+    let format = options.target.map(|target| target.format);
+    if options.output_buffer.is_some() && !matches!(format, None | Some(BinaryFormat::Elf)) {
+        return Err(Error::Unimplemented(format!(
+            "in-memory output for {format:?} links (roadmap M9)"
+        )));
+    }
+    let run = || match format {
         None | Some(BinaryFormat::Elf) => elf::link(options, diagnostics),
         Some(BinaryFormat::Pe) => coff::link(options, diagnostics),
         Some(BinaryFormat::MachO) => macho::link(options, diagnostics),
@@ -109,3 +180,24 @@ pub fn link(options: &LinkOptions, diagnostics: &dyn DiagnosticSink) -> Result<(
     }
     .inspect(|()| options.output_complete())
 }
+
+/// Runs a link described by `options` and returns the output image instead
+/// of writing [`LinkOptions::output`], which then only names the output.
+///
+/// This is [`link`] with a fresh [`OutputBuffer`] in
+/// [`LinkOptions::output_buffer`]. Side outputs (`-Map`,
+/// `--dependency-file`) are still written as files.
+///
+/// # Errors
+///
+/// As [`link`]; [`Error::Unimplemented`] for PE and Mach-O targets, whose
+/// drivers cannot write to memory yet.
+pub fn link_to_memory(options: &LinkOptions, diagnostics: &dyn DiagnosticSink) -> Result<Vec<u8>> {
+    let buffer = OutputBuffer::new();
+    let mut options = options.clone();
+    options.output_buffer = Some(buffer.clone());
+    link(&options, diagnostics)?;
+    buffer
+        .take()
+        .ok_or_else(|| Error::Internal("the link driver wrote no in-memory output".into()))
+}
```

## Proposed diff: `src/error.rs`

```diff
diff --git a/src/error.rs b/src/error.rs
index f4a7f62..437ff21 100644
--- a/src/error.rs
+++ b/src/error.rs
@@ -60,6 +60,9 @@ pub enum Error {
     ///
     /// Every use of this must name the roadmap milestone that will remove it.
     Unimplemented(String),
+    /// The link was cancelled through
+    /// [`LinkOptions::cancel`](crate::args::LinkOptions::cancel).
+    Cancelled,
 }
 
 impl Error {
@@ -110,6 +113,7 @@ impl fmt::Display for Error {
             Self::Reported { errors } if *errors == 1 => write!(f, "1 error"),
             Self::Reported { errors } => write!(f, "{errors} errors"),
             Self::Unimplemented(what) => write!(f, "not implemented yet: {what}"),
+            Self::Cancelled => f.write_str("link cancelled"),
         }
     }
 }
```

**Applied in e72127c** (with the follow-up below): cancellation returns
`Error::Cancelled`. The rest of this section is kept for the record.

Until this lands, cancellation uses an existing variant: `Error::Io` with
`path: None` and an `io::Error` of kind `Interrupted`, whose payload is a
private `Cancelled` type. It displays as `link cancelled`, and
`CancelToken::is_cancellation` recognizes it by that payload type, not by
the message. Once the variant exists, apply this follow-up in
`src/args/options.rs`. Callers that use `is_cancellation` do not change.

```diff
--- a/src/args/options.rs
+++ b/src/args/options.rs
@@ -668,19 +668,6 @@
 
 impl Eq for CancelToken {}
 
-/// The payload of the [`std::io::Error`] a cancelled link returns, so that
-/// [`CancelToken::is_cancellation`] does not depend on the message.
-#[derive(Debug)]
-struct Cancelled;
-
-impl std::fmt::Display for Cancelled {
-    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
-        f.write_str("link cancelled")
-    }
-}
-
-impl std::error::Error for Cancelled {}
-
 impl CancelToken {
     /// Creates a token that is not cancelled.
     #[must_use]
@@ -713,22 +700,17 @@
         }
     }
 
-    /// The error a cancelled link returns: an [`Error::Io`](crate::Error::Io)
-    /// of kind [`std::io::ErrorKind::Interrupted`] that reads
-    /// `link cancelled`.
+    /// The error a cancelled link returns,
+    /// [`Error::Cancelled`](crate::Error::Cancelled).
     #[must_use]
     pub fn error() -> crate::Error {
-        crate::Error::from(std::io::Error::new(
-            std::io::ErrorKind::Interrupted,
-            Cancelled,
-        ))
+        crate::Error::Cancelled
     }
 
     /// Whether `error` is the error of a cancelled link.
     #[must_use]
     pub fn is_cancellation(error: &crate::Error) -> bool {
-        matches!(error, crate::Error::Io { source, .. }
-            if source.get_ref().is_some_and(|inner| inner.is::<Cancelled>()))
+        matches!(error, crate::Error::Cancelled)
     }
 }
```
