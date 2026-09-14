# Library API (planned)

The `qld` crate exposes the linker as a library for build tools, compilers,
JIT/AOT pipelines, test harnesses and packagers. The API will be unstable
until 1.0 (M9). This document records the intended shape so that internal
design choices don't close it off.

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

```rust
use qld::{Config, Input, Output, OutputKind, Target};

// Parse a GNU-style command line, exactly as the `qld` binary does.
let config = Config::from_gnu_args(["-o", "hello", "crt1.o", "hello.o", "-lc"])?;

// Or build the configuration programmatically.
let mut config = Config::new(Target::X86_64_LINUX_GNU);
config
    .kind(OutputKind::Pie)
    .output(Output::path("hello"))
    .library_path("/usr/lib64")
    .input(Input::path("crt1.o"))
    .input(Input::bytes("hello.o", object_bytes)) // in-memory input
    .library("c")
    .gc_sections(true)
    .build_id(qld::BuildId::Fast);

let report = qld::link(&config, &mut qld::diag::Collect::default())?;
```

### Main types

| Type | Role |
| --- | --- |
| `Config` | All link options. Plain data; builder methods; `from_gnu_args` / `from_darwin_args` |
| `Target` | Format + architecture + OS/ABI (e.g. `X86_64_LINUX_GNU`, `AARCH64_APPLE_DARWIN`, `X86_64_WINDOWS_GNU`) |
| `Input` | `path`, `bytes` (`Arc<[u8]>`), `library`, `script`, with positional attributes (whole-archive, as-needed, static) |
| `Output` | `path`, or `memory` (returns `Vec<u8>` in the report) |
| `DiagnosticSink` | Trait receiving `Diagnostic { severity, code, message, locations, notes }` |
| `LinkReport` | Output bytes (when in memory), timing per stage, statistics, optional map file data |
| `Error` | Non-exhaustive enum. Fatal errors only; warnings go to the sink |

### Extension points (post-1.0 candidates)

- Custom input providers: a virtual file system for `-l` lookup and scripts
- Symbol resolution hooks for tools that inspect or override resolution
- Access to the final layout (addresses of symbols and sections) without
  writing an output, for tools that only need a map file or for incremental
  build systems
