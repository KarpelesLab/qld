# CLAUDE.md

qld is a GNU ld/gold/lld/mold-compatible linker in pure Rust, planned as a
parallel linker that also works as a library crate. It targets ELF, PE/COFF,
Mach-O (including fat binaries) and raw binary output.

## Read first

- `ROADMAP.md`: what is in scope now (milestones M0–M9)
- `docs/architecture.md`: pipeline, crate layout, data model
- `docs/development.md`: MSRV, dependency, `unsafe` and coding rules

## Hard constraints

- **Rust 1.89 MSRV, edition 2024.** Do not use APIs stabilized after 1.89.
  Verify with `cargo +1.89 check --workspace --all-targets` (the `1.89`
  toolchain is installed locally).
- **Pure Rust.** No C build steps or `-sys` crates. The only FFI is
  `dlopen` of LTO plugins, in `qld-plugin` behind the `plugin` feature.
- **Never panic on malformed input.** Use checked offsets and arithmetic, and
  return errors.
- **Deterministic output** regardless of thread count or scheduling.
- **No `object` crate on hot paths.** Parsing is in-house and zero-copy;
  `object`/`gimli` are allowed as test oracles only.
- Keep `docs/` and `ROADMAP.md` in sync with code changes. Intentional
  differences from GNU ld go into `docs/compatibility.md`.
