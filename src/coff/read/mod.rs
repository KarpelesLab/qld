//! Zero-copy PE/COFF reading.
//!
//! **Workstream W14.** The input side of the PE/COFF backend (M7), the
//! counterpart of [`elf::read`](crate::elf::read):
//!
//! - [`CoffObject`]: relocatable objects, regular and `/bigobj`
//!   ([`FileHeader`]): sections ([`SectionHeader`], long names through
//!   `/<n>` and `//<base64>`, alignment, COMDAT flags), symbols
//!   ([`SymbolTable`], [`Symbol`], with [`AuxSectionDefinition`],
//!   [`AuxWeakExternal`], [`AuxFunctionDefinition`] and file name aux
//!   records), relocations ([`Relocations`]), `@feat.00` ([`Feat00`]),
//!   `.llvm_addrsig` ([`AddrsigIter`]) and resource sections.
//! - [`directives`](mod@directives): `.drectve` tokenization with the
//!   Windows quoting rules and parsing into [`Directive`]s.
//! - [`import`](mod@import): short import objects ([`ShortImport`]) and GNU
//!   `dlltool` long import members ([`classify_long_import`],
//!   [`LongImportDlls`]).
//! - [`PeImage`]: DLLs and executables — headers, data directories, RVA
//!   translation and the export directory ([`ExportDirectory`]).
//! - [`def`](mod@def): module-definition files ([`ModuleDefinition`]).
//! - [`consts`]: PE/COFF constants, and machine, storage class and
//!   relocation type names.
//!
//! # Safety and robustness
//!
//! Input is untrusted. Records are decoded with `from_le_bytes` from byte
//! slices; there is no `unsafe`. Every offset and size is bounds-checked
//! with checked arithmetic (this module denies
//! `clippy::arithmetic_side_effects`), and problems are reported as
//! [`Error::Malformed`](crate::Error::Malformed) naming the file and
//! offset.
//!
//! # Performance model
//!
//! Parsing an object reads its fixed headers and checks the table bounds;
//! it allocates nothing. Sections, symbols and relocations are decoded one
//! record at a time from the mapped bytes, and names are returned as
//! `&[u8]` slices into the file. Regular and `/bigobj` objects share one
//! code path with a run-time record size: the difference is two fields, not
//! worth monomorphizing. All types are `Send + Sync`, so files are parsed in
//! parallel.
//!
//! # Limitations
//!
//! - Legacy COFF line numbers are not decoded.
//! - ARM64EC and ARM64X short imports expose the header fields, but not the
//!   extra `__imp_aux_` and mangled thunk symbols they define.
//! - Anonymous objects other than `/bigobj` (MSVC `/GL` LTCG IR) are
//!   rejected.
//! - [`PeImage`] ignores the loader's rounding of `PointerToRawData` to
//!   512 bytes, which only matters for hand-crafted images.

#![deny(clippy::arithmetic_side_effects)]

pub mod addrsig;
pub mod consts;
pub mod def;
pub mod directives;
pub mod export;
pub mod header;
pub mod import;
pub mod object;
pub mod pe;
pub mod reloc;
pub mod section;
pub mod source;
pub mod strtab;
pub mod symbol;

pub use addrsig::{ADDRSIG_SECTION, AddrsigIter};
pub use def::{DefImport, DefSection, ModuleDefinition, ModuleKind, parse_module_definition};
pub use directives::{
    DRECTVE_SECTION, Directive, Directives, Token, Tokens, parse_directives, tokenize,
};
pub use export::ExportSpec;
pub use header::FileHeader;
pub use import::{
    IMP_PREFIX, ImportName, LongImportDlls, LongImportMember, LongImportSymbol, ShortImport,
    classify_long_import,
};
pub use object::{CoffObject, FEAT00_SYMBOL, Feat00, Section, is_resource_section_name};
pub use pe::{DataDirectory, Export, ExportDirectory, ExportTarget, OptionalHeader, PeImage};
pub use reloc::{Relocation, Relocations};
pub use section::{SectionHeader, SectionTable};
pub use source::Source;
pub use strtab::StringTable;
pub use symbol::{
    AuxFunctionDefinition, AuxSectionDefinition, AuxWeakExternal, SectionNumber, Symbol,
    SymbolIter, SymbolTable,
};
