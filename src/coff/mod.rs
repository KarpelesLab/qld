//! PE/COFF backend (Windows).
//!
//! Roadmap M7. Scope is in `docs/formats.md`: COFF objects, import
//! libraries (short and long), `.def` files, EXE and DLL output, base
//! relocations, SEH, MinGW auto-import, and resources.
//!
//! | Module | Role |
//! | --- | --- |
//! | [`read`] | Zero-copy readers for objects, import libraries, PE images, `.drectve` and `.def` (workstream W14) |
//! | [`options`] | The PE options [`LinkOptions`](crate::args::LinkOptions) does not carry yet |
//! | [`inputs`] | Search paths, objects and archives, and the resolution file list |
//! | [`object`] | A parsed object: sections, COMDAT groups and a flat symbol list |
//! | [`resolve`] | COFF symbol precedence and COMDAT selection |
//! | [`directives`] | `.drectve` directives the link acts on |
//! | [`edata`] | The export directory |
//! | [`implib`] | `--out-implib` and `--output-def` |
//! | [`imports`] | Short import libraries and directly linked DLLs |
//! | [`layout`] | Output sections, grouped-section ordering, RVAs |
//! | [`defined`] | The symbols MinGW's C runtime expects the linker to define |
//! | [`reloc`] | Symbol addresses, relocation application, base relocations |
//! | [`symtab`] | The image's COFF symbol table |
//! | [`write`](mod@write) | Headers, section contents and the image checksum |
//! | [`link`](mod@link) | The driver |
//!
//! Not implemented yet: `-r`, `--gc-sections`, local symbols in the output
//! symbol table, auto-import of PC-relative references, and architectures
//! other than x86-64.

pub mod defined;
pub mod directives;
pub mod edata;
pub mod implib;
pub mod imports;
pub mod inputs;
pub mod layout;
pub mod link;
pub mod object;
pub mod options;
pub mod read;
pub mod reloc;
pub mod resolve;
pub mod symtab;
pub mod write;

pub use link::{link, link_with};
pub use options::PeOptions;

use crate::target::Architecture;

use read::consts::{
    IMAGE_FILE_MACHINE_AMD64, IMAGE_FILE_MACHINE_ARM64, IMAGE_FILE_MACHINE_ARMNT,
    IMAGE_FILE_MACHINE_I386,
};

/// The COFF `Machine` value for an architecture, if PE has one.
#[must_use]
pub fn machine_for(arch: Architecture) -> Option<u16> {
    Some(match arch {
        Architecture::X86 => IMAGE_FILE_MACHINE_I386,
        Architecture::X86_64 => IMAGE_FILE_MACHINE_AMD64,
        Architecture::Aarch64 => IMAGE_FILE_MACHINE_ARM64,
        Architecture::Arm => IMAGE_FILE_MACHINE_ARMNT,
        _ => return None,
    })
}
