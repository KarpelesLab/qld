# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0](https://github.com/KarpelesLab/qld/compare/v0.1.0...v0.2.0) - 2026-09-29

### Other

- record the scratch-directory race in the test harness
- probe the big-endian compiler once
- run the multilib tests; docs for W56
- skip an incompatible plugin-requested library too
- skip an incompatible library and keep searching, as GNU ld does
- alpha, not pre-alpha; the status block matches what qld does now
- AmigaOS Hunk job with the vasm/vlink oracle; docs for W55
- make the Hunk corpus reproducible
- reach the backend from a BinaryFormat::Hunk target too
- m68k ELF output, and an out-of-range PC-relative case
- an AmigaOS Hunk corpus with vlink as the oracle
- m68k and AmigaOS Hunk: link 68000 objects into load files
- a MinGW image's DWARF comes from the runtime, not from -g
- read the long-section-name rule out of the image
- one run per ref, and the cross-architecture jobs on master pushes only
- diagnostics behaviour and W51 follow-ups
- corruption and diagnostics jobs; docs for W51
- --fatal-warnings must fail a link that forks
- PE long section names (W54); corruption tests close the M0 fuzz item
- Merge W54: PE images truncate long section names, as GNU ld does
- lld 22's spare LoongArch TLS GOT entries
- W53 merged; the x86-64 differences settled, exported linker symbols, TLS ABI tags
- Merge W53: linker-defined symbols exported from shared objects, __ehdr_start section-relative, ELFOSABI_GNU, TLS ABI tags and the option gaps
- W48 (PowerPC64 BE) and the four instantiated ELF formats
- W48 merged (PowerPC64 BE, all four ELF formats instantiated)
- Merge W48: PowerPC64 big-endian ELFv1 (static), Elf32Be instantiated, .gnu.attributes fix
- Merge W47: allocator tuning, cheaper relocation decisions, placement index, packed definitions
- resolve a file's relocation targets through one view
- keep a symbol's definition in one cell, not five vectors
- one section ID lookup per relocation target address
- index the placement rules by section name prefix
- read a relocation's target once per decision
- widen glibc's malloc thresholds in the binary
- workstreams W47-W53 (round 6, closing the remaining gaps)
