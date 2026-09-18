# Library API

The `qld` crate exposes the linker as a library for build tools, compilers,
JIT/AOT pipelines, test harnesses and packagers. The API will be unstable
until 1.0 (M9). This document records the intended shape so that internal
design choices don't close it off.

**Status (W36):**
- **Documented surface:** the crate root, `args`, `diag`, `error` and
  `target`. Every other module is `#[doc(hidden)]`: it is public for qld's own
  tests and tools, and is not covered by semantic versioning.
- **In-memory I/O and cancellation (ELF links only so far):**
  - `InputKind::bytes(name, data)` passes an input as bytes.
  - `MemoryFiles` / `InputProvider` (`LinkOptions::input_provider`) serve
    files by path, ahead of the disk. This covers `-l` search, scripts and
    thin archive members.
  - `link_to_memory` or `OutputBuffer` (`LinkOptions::output_buffer`) return
    the output image.
  - A `CancelToken` (`LinkOptions::cancel`) makes the link return
    `Error::Cancelled`, leaving no output behind.
- **Examples:** `examples/` has `link_argv`, `in_memory`, `custom_sink`,
  `rayon_pool` and `cancel`.
- **Review:** [tests/projects/api-review.md](../tests/projects/api-review.md)
  lists every public item, the 1.0 blockers and the semver policy.

## Requirements

- **No global state.** Several links can run concurrently in one process.
  The only exception is LTO through the GNU plugin API, whose C interface is
  process-global; a second concurrent plugin link fails with an error.
- **No `process::exit`, no printing.** Errors are returned. Diagnostics go
  to a caller-supplied sink.
- **Caller-controlled threading.** Links run in the current rayon pool, so
  callers can wrap a link in their own `ThreadPool::install`.
- **Inputs and outputs can live in memory.** No temporary files are needed.
- **Cancellation.** A cancellation token is checked between pipeline stages
  and inside long parallel loops.
- **Same behavior as the CLI.** The binary is a thin wrapper around this
  crate. There are no CLI-only features.

## Sketch

The types below exist today in `src/args/options.rs` and `src/diag.rs`; the
parts marked *planned* do not.

```rust
use qld::args::{InputAttrs, InputKind, LinkOptions, OutputKind};
use qld::diag::Collect;

// Parse a GNU-style command line, exactly as the `qld` binary does.
let outcome = qld::parse_gnu(&["qld", "-o", "hello", "crt1.o", "hello.o", "-lc"])?;

// Or build the options programmatically.
let mut options = LinkOptions::new();
options.kind = OutputKind::Pie;
options.output = Some("hello".into());
options.search_paths.push("/usr/lib64".into());
options.gc_sections = true;
options.push_input(InputKind::File("crt1.o".into()), InputAttrs::default());
options.push_input(
    InputKind::Bytes { name: "hello.o".into(), data: object_bytes }, // in-memory input
    InputAttrs::default(),
);
options.push_input(InputKind::Library("c".into()), InputAttrs::default());

let diagnostics = Collect::new();
qld::link(&options, &diagnostics)?;
```

### Main types

| Type | Role |
| --- | --- |
| `LinkOptions` | All link options. Plain data, no I/O; built by hand or by an argv front end |
| `Target` | Format + architecture + endianness + pointer width + OS |
| `InputKind` | `File`, `Library`, `LibraryExact`, `Script`, or `Bytes` (`Arc<[u8]>`) for in-memory inputs |
| `InputAttrs` | Positional state per input: whole-archive, as-needed, static-only, in-group |
| `DiagnosticSink` | Trait receiving `Diagnostic { severity, message, locations, notes, order }`; `Collect` and `Stderr` implement it |
| `Error` | Non-exhaustive enum. Fatal errors only; warnings go to the sink |
| `LinkReport` *(planned)* | Output bytes when writing to memory, per-stage timing, statistics, map data |

*Planned:* an in-memory `Output`, a cancellation token, and a builder API that
does not require setting public fields directly.

### Extension points (post-1.0 candidates)

- Custom input providers: a virtual file system for `-l` lookup and scripts
- Symbol resolution hooks for tools that inspect or override resolution
- Access to the final layout (addresses of symbols and sections) without
  writing an output, for tools that only need a map file or for incremental
  build systems
