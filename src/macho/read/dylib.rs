//! Dynamic libraries (`MH_DYLIB` and `MH_DYLIB_STUB`).

use super::bytes::Source;
use super::chained::ChainedFixups;
use super::commands::{BuildVersion, DylibCommand, DylibLoadKind, PackedVersion};
use super::consts::{
    LC_DYLD_CHAINED_FIXUPS, LC_DYLD_EXPORTS_TRIE, LC_DYLD_INFO, LC_DYLD_INFO_ONLY, LC_ID_DYLIB,
    LC_RPATH, LC_SUB_CLIENT, LC_SUB_FRAMEWORK, LC_SYMTAB, MH_DYLIB, MH_DYLIB_STUB, MH_EXECUTE,
    MH_NO_REEXPORTED_DYLIBS,
};
use super::file::{MachHeader, MachOFile};
use super::symbol::SymbolTable;
use super::trie::ExportTrieIter;
use crate::error::Result;

/// A dependency of a dylib.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DylibDependency<'a> {
    /// How it is loaded.
    pub kind: DylibLoadKind,
    /// Install name.
    pub name: &'a [u8],
    /// Current version.
    pub current_version: PackedVersion,
    /// Compatibility version.
    pub compatibility_version: PackedVersion,
}

/// A parsed dylib.
#[derive(Clone, Debug)]
pub struct Dylib<'a> {
    file: MachOFile<'a>,
    id: Option<DylibCommand<'a>>,
    dependencies: Vec<DylibDependency<'a>>,
    rpaths: Vec<&'a [u8]>,
    umbrella: Option<&'a [u8]>,
    allowable_clients: Vec<&'a [u8]>,
    build_versions: Vec<BuildVersion<'a>>,
    exports_trie: (&'a [u8], u64),
    chained_fixups: Option<(&'a [u8], u64)>,
    symbols: SymbolTable<'a>,
}

impl<'a> Dylib<'a> {
    /// Parses an `MH_DYLIB` or `MH_DYLIB_STUB` file.
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` if the file is not a dylib, a load command
    /// is malformed, or a table lies outside the file.
    pub fn parse(data: &'a [u8], source: Source<'a>) -> Result<Self> {
        Self::parse_kinds(data, source, &[MH_DYLIB, MH_DYLIB_STUB])
    }

    /// Parses a dylib or an executable (for `-bundle_loader`): an image
    /// whose exports can resolve another image's references.
    ///
    /// # Errors
    ///
    /// As for [`Dylib::parse`], with `MH_EXECUTE` also accepted.
    pub fn parse_image(data: &'a [u8], source: Source<'a>) -> Result<Self> {
        Self::parse_kinds(data, source, &[MH_DYLIB, MH_DYLIB_STUB, MH_EXECUTE])
    }

    fn parse_kinds(data: &'a [u8], source: Source<'a>, kinds: &[u32]) -> Result<Self> {
        let file = MachOFile::parse(data, source)?;
        if !kinds.contains(&file.header().file_type) {
            return Err(source.malformed(12, "Mach-O file type (expected MH_DYLIB)"));
        }
        let mut dylib = Self {
            file,
            id: None,
            dependencies: Vec::new(),
            rpaths: Vec::new(),
            umbrella: None,
            allowable_clients: Vec::new(),
            build_versions: Vec::new(),
            exports_trie: (&[], 0),
            chained_fixups: None,
            symbols: SymbolTable::empty(file.endian(), file.is64(), source),
        };
        let mut trie_from_exports_command = false;
        for command in file.load_commands() {
            let command = command?;
            match command.cmd {
                LC_ID_DYLIB => {
                    if dylib.id.is_none() {
                        dylib.id = Some(command.dylib()?);
                    }
                }
                cmd if DylibLoadKind::from_cmd(cmd).is_some() => {
                    let dependency = command.dylib()?;
                    dylib.dependencies.push(DylibDependency {
                        kind: DylibLoadKind::from_cmd(cmd).unwrap_or(DylibLoadKind::Regular),
                        name: dependency.name,
                        current_version: dependency.current_version,
                        compatibility_version: dependency.compatibility_version,
                    });
                }
                LC_RPATH => dylib.rpaths.push(command.string()?),
                LC_SUB_FRAMEWORK => dylib.umbrella = Some(command.string()?),
                LC_SUB_CLIENT => dylib.allowable_clients.push(command.string()?),
                LC_DYLD_INFO | LC_DYLD_INFO_ONLY => {
                    let info = command.dyld_info()?;
                    if !trie_from_exports_command {
                        dylib.exports_trie = (
                            file.bytes(
                                u64::from(info.export_off),
                                u64::from(info.export_size),
                                "export trie",
                            )?,
                            u64::from(info.export_off),
                        );
                    }
                }
                LC_DYLD_EXPORTS_TRIE => {
                    let info = command.linkedit_data()?;
                    trie_from_exports_command = true;
                    dylib.exports_trie = (
                        file.bytes(
                            u64::from(info.dataoff),
                            u64::from(info.datasize),
                            "export trie",
                        )?,
                        u64::from(info.dataoff),
                    );
                }
                LC_DYLD_CHAINED_FIXUPS => {
                    let info = command.linkedit_data()?;
                    dylib.chained_fixups = Some((
                        file.bytes(
                            u64::from(info.dataoff),
                            u64::from(info.datasize),
                            "chained fixups",
                        )?,
                        u64::from(info.dataoff),
                    ));
                }
                LC_SYMTAB => {
                    let symtab = command.symtab()?;
                    let entry = if file.is64() { 16 } else { 12 };
                    let records = file.bytes(
                        u64::from(symtab.symoff),
                        u64::from(symtab.nsyms).saturating_mul(entry),
                        "symbol table",
                    )?;
                    let strtab = file.bytes(
                        u64::from(symtab.stroff),
                        u64::from(symtab.strsize),
                        "string table",
                    )?;
                    dylib.symbols = SymbolTable::new(
                        records,
                        strtab,
                        u64::from(symtab.symoff),
                        file.endian(),
                        file.is64(),
                        source,
                    );
                }
                _ if command.is_version() => {
                    dylib.build_versions.push(command.build_version()?);
                }
                _ => {}
            }
        }
        if file.header().file_type == MH_DYLIB && dylib.id.is_none() {
            return Err(source.malformed(0, "dylib (no LC_ID_DYLIB)"));
        }
        Ok(dylib)
    }

    /// The underlying file.
    #[must_use]
    pub fn file(&self) -> &MachOFile<'a> {
        &self.file
    }

    /// The header.
    #[must_use]
    pub fn header(&self) -> &MachHeader {
        self.file.header()
    }

    /// `LC_ID_DYLIB`: install name and versions.
    #[must_use]
    pub fn id(&self) -> Option<&DylibCommand<'a>> {
        self.id.as_ref()
    }

    /// The install name (empty without `LC_ID_DYLIB`).
    #[must_use]
    pub fn install_name(&self) -> &'a [u8] {
        self.id.map_or(&[], |id| id.name)
    }

    /// Dependencies, in load command order: ordinal `n` is
    /// `dependencies()[n - 1]`.
    #[must_use]
    pub fn dependencies(&self) -> &[DylibDependency<'a>] {
        &self.dependencies
    }

    /// Re-exported dependencies (`LC_REEXPORT_DYLIB`).
    pub fn reexports(&self) -> impl Iterator<Item = &DylibDependency<'a>> + '_ {
        self.dependencies
            .iter()
            .filter(|d| d.kind == DylibLoadKind::Reexport)
    }

    /// Whether `MH_NO_REEXPORTED_DYLIBS` is set.
    #[must_use]
    pub fn no_reexported_dylibs(&self) -> bool {
        self.header().flags & MH_NO_REEXPORTED_DYLIBS != 0
    }

    /// `LC_RPATH` entries.
    #[must_use]
    pub fn rpaths(&self) -> &[&'a [u8]] {
        &self.rpaths
    }

    /// `LC_SUB_FRAMEWORK`: the umbrella framework this dylib belongs to.
    #[must_use]
    pub fn umbrella(&self) -> Option<&'a [u8]> {
        self.umbrella
    }

    /// `LC_SUB_CLIENT` entries.
    #[must_use]
    pub fn allowable_clients(&self) -> &[&'a [u8]] {
        &self.allowable_clients
    }

    /// `LC_BUILD_VERSION` and `LC_VERSION_MIN_*` commands (a zippered
    /// dylib has two).
    #[must_use]
    pub fn build_versions(&self) -> &[BuildVersion<'a>] {
        &self.build_versions
    }

    /// The raw export trie (from `LC_DYLD_EXPORTS_TRIE`, else from
    /// `LC_DYLD_INFO_ONLY`), empty when there is none.
    #[must_use]
    pub fn exports_trie(&self) -> &'a [u8] {
        self.exports_trie.0
    }

    /// Iterates over the exports in the trie.
    #[must_use]
    pub fn exports(&self) -> ExportTrieIter<'a> {
        ExportTrieIter::new(self.exports_trie.0, self.exports_trie.1, self.file.source())
    }

    /// The symbol table (empty without `LC_SYMTAB`). Old dylibs without an
    /// export trie list their exports here.
    #[must_use]
    pub fn symbols(&self) -> &SymbolTable<'a> {
        &self.symbols
    }

    /// `LC_DYLD_CHAINED_FIXUPS`, decoded.
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` for a malformed header.
    pub fn chained_fixups(&self) -> Result<Option<ChainedFixups<'a>>> {
        self.chained_fixups
            .map(|(data, offset)| {
                ChainedFixups::parse(data, offset, self.file.endian(), self.file.source())
            })
            .transpose()
    }
}
