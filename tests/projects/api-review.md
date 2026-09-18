# Library API review for 1.0 (W36, milestone M9)

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
| `LinkOptions` (129 pub fields, 8 methods) | stable | **Add `#[non_exhaustive]`** so that fields can be added in minor releases. Field assignment still works on a non-exhaustive struct; only struct literals and `..Default::default()` stop working outside the crate. **Blocker:** `tests/coff_link.rs` builds `LinkOptions { .. }` literals at lines 406, 950, 958, 965 and 972. They must switch to `LinkOptions::new()` plus assignments before the attribute can land. A full builder is not needed: `new()` plus public fields, `push_input` and a few helpers is simpler and just as future-proof once the struct is non-exhaustive. |
| `LinkOptions::default()` vs `LinkOptions::new()` | accidental | **Footgun:** the derived `Default` turns `demangle`, `relro`, `gnu_stack`, `copy_relocs`, `combine_relocs`, `extern_protected_data`, `section_header`, `relax`, `dependent_libraries` and `fork` off, while `new()` turns them on. Make `Default` return `new()` by writing out `new()`'s field list, or change those fields to `Option<bool>`, with `None` meaning the default. This needs agreement: every workstream that adds a field relies on `..Self::default()` in `new()`, and `tests/coff_link.rs` uses `..LinkOptions::default()`. |
| `LinkOptions` string-typed fields: `icf`, `orphan_handling`, `sort_section`, `compress_debug_sections`, `start_stop_visibility`, `output_format` | stable, needs change | Change them to enums before 1.0 (`IcfMode`, `OrphanHandling`, `SortSection`, `DebugCompression`, `Visibility`, `OutputFormat`). A string cannot be validated when the options are built, and the drivers match on literals. |
| `LinkOptions::{fork, exit_on_plugin_fatal, on_output_complete}` | internal | Only the binary uses these. Make them `#[doc(hidden)]` and keep them out of semver. They could move into a `ProcessOptions` that `main.rs` owns. |
| `LinkOptions::{warnings, ignored}` | stable, should move | These are outputs of parsing, not options. Before 1.0, move them to a `Parsed { options, warnings, ignored }` in `ParseOutcome::Link`. Until then, `examples/link_argv.rs` shows the caller emitting them, as `main.rs` does. |
| `LinkOptions::{input_provider, output_buffer, cancel}` *(new, W36)* | stable | Added. |
| `LinkOptions::darwin.inputs` vs `LinkOptions::inputs` | stable, needs change | Mach-O links read a separate input list (`DarwinArgs::inputs`, `DarwinInputKind`), so `InputKind::Bytes` and `push_input` have no effect on a Mach-O link. Before 1.0, merge them into one list: add `Framework`/`WeakLibrary`/… as `InputKind` variants or as attributes. |
| `LinkOptions::threads` | stable | The doc says `None` means one thread per core; in fact the ELF driver sizes the pool from the input size. Fix the doc (in `options.rs`, W1). |
| `LinkOptions::{output_complete, output_path, is_pic, is_dynamic, push_input, resolve_sysroot, check_cancelled}` | stable | Keep. `output_complete` is internal to the drivers: make it `#[doc(hidden)]`. |
| `InputKind` (non_exhaustive, 6 variants) | stable | Keep. `InputKind::bytes()` added. `Bytes::name` is a `String` while `Source::Bytes::name` is a `PathBuf`: pick one (`PathBuf`, as diagnostics treat it as a path) before 1.0. |
| `InputSpec` | stable | Add `#[non_exhaustive]` once `tests/input.rs:561` and `tests/coff_link.rs:411,511,1059,1219` stop building literals. `position` is assigned by `push_input`, so it should become read-only, through an accessor. |
| `InputAttrs` | stable | `#[non_exhaustive]` once `tests/input.rs:563` stops using a struct literal. `lazy`, `copy_dt_needed` and `format` will keep growing. |
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
| No printing | `src/elf/map.rs:140,159` prints `-M`/`--cref` to stdout; `src/elf/link.rs:90` and `src/elf/lto.rs:823,860` print `QLD_TIMING` laps to stderr; `src/plugin/host.rs:1484` prints plugin messages | Route `-M` through a caller-supplied writer (a `LinkOptions::map_writer`, or a map in the `LinkReport`); send timing to a `LinkReport` or behind an option; send plugin messages to the sink. |
| No `process::exit` | `src/elf/lto.rs:364`, gated by `exit_on_plugin_fatal`, which only the binary sets | Fine as is; keep the flag `#[doc(hidden)]`. |
| No environment dependence | `QLD_TIMING`, `QLD_OUTPUT_BACKING`, `LD_RUN_PATH`/`LD_LIBRARY_PATH` (`src/elf/dso.rs:510-514`), `ZERO_AR_DATE` (`src/macho/inputs.rs:1240`) | GNU ld reads `LD_*` too, so keep that behavior, but have `main.rs` read the environment and pass it in through `LinkOptions` (for example `env_library_path: Vec<PathBuf>`), so that library links are hermetic by default. |
| Caller-controlled threading | Outside any pool, the ELF driver creates its own pools. Inside a caller's pool of more than 16 threads it also creates a 16-thread "narrow" pool (`src/elf/link.rs`, `Narrow`) | Document it in the rustdoc of `link`, or add `LinkOptions::thread_policy = { CallerPool, Own(n) }` so that a caller can forbid extra pools. |
| In-memory inputs and outputs | Only the ELF driver: PE and Mach-O ignore `output_buffer`, `input_provider` and `cancel` | See the change list below; `link` rejects `output_buffer` for them in the lib.rs diff. |
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
| 9 | W9 | `tests/coff_link.rs`, `tests/input.rs` | Replace the `LinkOptions { .. }`, `InputSpec { .. }` and `InputAttrs { .. }` literals with `new()`/`default()` plus assignments, to unblock `#[non_exhaustive]` on those structs. |
| 10 | W9 | `tests/passes.rs:19`, `tests/symbols.rs:24` | Import `FileId`/`SectionId`/`SymbolId` from `qld::ids`, so that the root re-exports can go. |
| 11 | W1 | `src/args/options.rs` | `Default` = `new()`; enums for the six string-typed options; move `warnings`/`ignored` into `ParseOutcome`; document `threads` correctly. |
| 12 | W1, integrator | `src/args/parse.rs`, `src/main.rs` | `#[non_exhaustive]` on `ParseOutcome`, with a wildcard arm in `main.rs`. |
| 13 | integrator | `src/diag.rs` | `#[non_exhaustive]` plus constructors for `Diagnostic`, `Location` and `SourceLocation`; a default body for `DiagnosticSink::error_count`; no panic in `Collect::take_sorted`. |
| 14 | integrator | `src/target.rs` | `#[non_exhaustive]` on `Target`. |
| 15 | W5 successor | `src/output/file.rs`, `src/elf/{link,lto}.rs`, `src/elf/map.rs` | Stop reading `QLD_OUTPUT_BACKING` and `QLD_TIMING` in the library; route `-M` output through a writer. |

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
