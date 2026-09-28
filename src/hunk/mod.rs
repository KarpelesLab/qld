//! AmigaOS Hunk backend: Motorola 68000 load files (`--oformat amigahunk`).
//!
//! **Workstream W55.** See `docs/formats.md`.
//!
//! A Hunk load file is what AmigaDOS executes. It has no headers beyond a
//! table of hunk sizes, no segments and no dynamic linking: the loader
//! allocates one block of memory per hunk, wherever it can, copies the
//! contents in and adds each hunk's address to the longwords a relocation
//! block names. Execution starts at offset 0 of the first hunk.
//!
//! # How the link runs
//!
//! Inputs are ELF m68k relocatable objects, which vasm writes with
//! `-Felf` and GNU `as` with `--m68k`. qld links them with the ELF
//! backend — the same resolution, archives, `--gc-sections`, section
//! placement and relocation — and [`write::render`] then cuts the image
//! the ELF writer produced into hunks, exactly as [`crate::elf::rawout`]
//! cuts it into `--oformat binary` pieces. Nothing about the Hunk format
//! reaches the ELF pipeline; the one hook is in
//! [`crate::elf::write::write`].
//!
//! # Scope
//!
//! Executables, with absolute (`R_68K_32`) and PC-relative relocations,
//! `HUNK_CODE`/`HUNK_DATA`/`HUNK_BSS`, `HUNK_RELOC32` (and the short form
//! on request) and `HUNK_SYMBOL`. Not yet: Hunk object output
//! (`HUNK_UNIT`/`HUNK_EXT`), overlays, reading Hunk files as input, and the
//! chip/fast memory attributes, which have no ELF section flag to come
//! from and so are always `MEMF_PUBLIC` ([`format::MemFlags`]).

pub mod format;
pub mod write;

use crate::args::{LinkOptions, OutputKind};
use crate::diag::DiagnosticSink;
use crate::error::{Error, Result};
use crate::target::{Architecture, BinaryFormat, Endianness, PointerWidth};

/// The `--oformat` name of a Hunk load file, as vlink's `-b` flag spells
/// its target.
pub const FORMAT_NAME: &str = "amigahunk";

/// Whether this link writes a Hunk load file: its target says so, or
/// `--oformat amigahunk` does.
#[must_use]
pub fn wanted(options: &LinkOptions) -> bool {
    options
        .target
        .is_some_and(|target| target.format == BinaryFormat::Hunk)
        || options
            .output_format
            .as_ref()
            .is_some_and(|format| format.name() == FORMAT_NAME)
}

/// Links an AmigaOS Hunk output described by `options`.
///
/// Called by [`crate::link`] when the target format is
/// [`BinaryFormat::Hunk`]. The link itself runs in the ELF backend; this
/// only settles the options a load file implies and hands over.
///
/// # Errors
///
/// [`Error::Unimplemented`] for output kinds a load file cannot hold
/// (shared objects, PIEs, Hunk object output), and anything
/// [`crate::elf::link`](fn@crate::elf::link) returns.
pub fn link(options: &LinkOptions, diagnostics: &dyn DiagnosticSink) -> Result<()> {
    let mut options = options.clone();
    match options.kind {
        OutputKind::Executable | OutputKind::StaticExecutable => {
            options.kind = OutputKind::StaticExecutable;
        }
        OutputKind::Relocatable => {
            return Err(Error::Unimplemented(
                "relocatable AmigaOS Hunk output (-r): HUNK_UNIT objects are not written yet"
                    .into(),
            ));
        }
        kind => {
            return Err(Error::Unimplemented(format!(
                "{kind:?} output in the AmigaOS Hunk format, which has no dynamic linking"
            )));
        }
    }
    let target = options.target.get_or_insert(crate::target::Target {
        format: BinaryFormat::Hunk,
        arch: Architecture::M68k,
        endian: Endianness::Big,
        pointer_width: PointerWidth::Bits32,
        os: crate::target::OperatingSystem::None,
    });
    if target.arch != Architecture::M68k {
        return Err(Error::Unimplemented(format!(
            "AmigaOS Hunk output for {:?}: the format is m68k's",
            target.arch
        )));
    }
    // The ELF backend picks its class and byte order from the target, and
    // `Arch::from_target` only knows big-endian m68k.
    target.endian = Endianness::Big;
    target.pointer_width = PointerWidth::Bits32;
    // The image is cut into hunks, not loaded as it is: page alignment
    // between sections would only pad the hunks.
    options.magic = crate::args::options::MagicMode::Nmagic;
    crate::elf::link(&options, diagnostics)
}
