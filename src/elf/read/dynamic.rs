//! Shared objects (`ET_DYN`): dynamic section, dynamic symbols and symbol
//! versioning.

use super::consts::{
    DT_NEEDED, DT_NULL, DT_SONAME, DT_STRSZ, DT_STRTAB, ET_DYN, PT_DYNAMIC, SHT_DYNAMIC,
    SHT_DYNSYM, SHT_GNU_HASH, SHT_GNU_VERDEF, SHT_GNU_VERNEED, SHT_GNU_VERSYM, SHT_HASH,
    VER_FLG_BASE, VER_NDX_GLOBAL, VERSYM_HIDDEN, VERSYM_VERSION,
};
use super::file::ElfFile;
use super::format::{ElfFormat, Endian, read_u16, read_u32};
use super::section::SectionHeader;
use super::source::{Source, subslice, to_u64};
use super::strtab::StringTable;
use super::symbol::{Symbol, SymbolTable};
use crate::error::Result;
use crate::target::Architecture;

/// A decoded dynamic section entry.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DynEntry {
    /// Tag (`DT_*`).
    pub tag: i64,
    /// Value or address.
    pub value: u64,
}

/// Where a version comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VersionKind<'a> {
    /// Defined by this object (`.gnu.version_d`).
    Defined,
    /// Required from another object (`.gnu.version_r`).
    Needed {
        /// The file name the version is required from.
        file: &'a [u8],
    },
}

/// One entry of the version table, by version index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VersionInfo<'a> {
    /// Version name (for example `GLIBC_2.34`).
    pub name: &'a [u8],
    /// `vd_flags` or `vna_flags` (`VER_FLG_*`).
    pub flags: u16,
    /// Defined here or needed from elsewhere.
    pub kind: VersionKind<'a>,
}

impl VersionInfo<'_> {
    /// Whether this is the base definition, which names the file itself.
    #[must_use]
    pub fn is_base(&self) -> bool {
        self.flags & VER_FLG_BASE != 0
    }
}

/// The version of a dynamic symbol.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SymbolVersion<'a> {
    /// Version index (`versym & 0x7fff`): 0 is local, 1 is global
    /// (unversioned).
    pub index: u16,
    /// Whether the hidden bit is set: the symbol is not the default version
    /// and can only be bound to by explicit version (`foo@V`, not `foo@@V`).
    pub hidden: bool,
    /// The version, for indices 2 and above.
    pub info: Option<VersionInfo<'a>>,
}

impl<'a> SymbolVersion<'a> {
    /// The version name, for versioned symbols.
    #[must_use]
    pub fn name(&self) -> Option<&'a [u8]> {
        self.info.map(|v| v.name)
    }
}

/// A dynamic symbol with its version.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DynamicSymbol<'a> {
    /// The symbol.
    pub symbol: Symbol<'a>,
    /// Its version.
    pub version: SymbolVersion<'a>,
}

/// A parsed shared object.
///
/// Parsing locates `.dynsym`, `.dynstr`, `.dynamic` and the version
/// sections through the section headers (falling back to `PT_DYNAMIC` for
/// the dynamic array), and builds the version table, which is the only
/// allocation.
#[derive(Clone, Debug)]
pub struct SharedObject<'a, F: ElfFormat> {
    elf: ElfFile<'a, F>,
    dynamic: &'a [F::Dyn],
    dynamic_offset: u64,
    dynstr: StringTable<'a>,
    symbols: SymbolTable<'a, F>,
    versym: &'a [[u8; 2]],
    versions: Vec<Option<VersionInfo<'a>>>,
    soname: Option<&'a [u8]>,
    gnu_hash: bool,
    sysv_hash: bool,
}

impl<'a, F: ElfFormat> SharedObject<'a, F> {
    /// Parses a shared object.
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` if the file is not a valid `ET_DYN` ELF file
    /// of format `F`, or if a dynamic-linking structure is out of bounds or
    /// malformed.
    pub fn parse(data: &'a [u8], source: Source<'a>) -> Result<Self> {
        let elf = ElfFile::<F>::parse(data, source)?;
        if elf.header().e_type != ET_DYN {
            return Err(source.malformed(16, "ELF file type (expected a shared object)"));
        }

        let mut dynsym = None;
        let mut dynamic_section = None;
        let mut versym_section = None;
        let mut verdef_section = None;
        let mut verneed_section = None;
        let mut gnu_hash = false;
        let mut sysv_hash = false;
        for (index, hdr) in elf.enumerate_sections() {
            let slot = match hdr.sh_type {
                SHT_DYNSYM => &mut dynsym,
                SHT_DYNAMIC => &mut dynamic_section,
                SHT_GNU_VERSYM => &mut versym_section,
                SHT_GNU_VERDEF => &mut verdef_section,
                SHT_GNU_VERNEED => &mut verneed_section,
                SHT_GNU_HASH => {
                    gnu_hash = true;
                    continue;
                }
                SHT_HASH => {
                    sysv_hash = true;
                    continue;
                }
                _ => continue,
            };
            if slot.is_none() {
                *slot = Some((index, hdr));
            }
        }

        // The dynamic array: from the section, or from PT_DYNAMIC.
        let (dynamic, dynamic_offset, dynamic_strtab_link) = match dynamic_section {
            Some((_, hdr)) => (
                elf.section_records::<F::Dyn>(&hdr, "dynamic section")?,
                hdr.sh_offset,
                Some(hdr.sh_link),
            ),
            None => match elf.segments().iter().find(|p| p.p_type == PT_DYNAMIC) {
                Some(phdr) => {
                    let bytes = elf.segment_data(&phdr)?;
                    // A trailing partial entry is ignored, like DT_NULL.
                    let (records, _) = <F::Dyn as super::format::RawRecord>::slice_from(bytes);
                    (records, phdr.p_offset, None)
                }
                None => (&[][..], 0, None),
            },
        };

        let symbols = match dynsym {
            Some((index, hdr)) => elf.symbol_table_with_shndx(index, &hdr, None)?,
            None => SymbolTable::empty(source),
        };

        // The dynamic string table: linked from .dynamic, else from .dynsym,
        // else found through DT_STRTAB.
        let dynstr = match dynamic_strtab_link {
            Some(link) => {
                let hdr = elf.section_header(link).map_err(|_| {
                    source.malformed(dynamic_offset, "dynamic section string table link")
                })?;
                StringTable::new(elf.section_data(&hdr)?, hdr.sh_offset)
            }
            None if dynsym.is_some() => symbols.strtab(),
            None => strtab_from_dynamic::<F>(&elf, dynamic, dynamic_offset)?,
        };

        let mut this = Self {
            elf,
            dynamic,
            dynamic_offset,
            dynstr,
            symbols,
            versym: &[],
            versions: Vec::new(),
            soname: None,
            gnu_hash,
            sysv_hash,
        };

        for entry in this.dynamic_entries() {
            if entry.tag == DT_SONAME {
                this.soname = Some(this.dynamic_string(entry.value)?);
                break;
            }
        }

        if let Some((_, hdr)) = versym_section {
            let versym = this
                .elf
                .section_records::<[u8; 2]>(&hdr, "symbol version table")?;
            if versym.len() != this.symbols.len() {
                return Err(source.malformed(
                    hdr.sh_offset,
                    "symbol version table (entry count differs from .dynsym)",
                ));
            }
            this.versym = versym;
        }
        if let Some((_, hdr)) = verdef_section {
            this.parse_verdef(&hdr)?;
        }
        if let Some((_, hdr)) = verneed_section {
            this.parse_verneed(&hdr)?;
        }
        Ok(this)
    }

    /// The underlying ELF file view.
    #[must_use]
    pub fn elf(&self) -> &ElfFile<'a, F> {
        &self.elf
    }

    /// The file identity used in error messages.
    #[must_use]
    pub fn source(&self) -> Source<'a> {
        self.elf.source()
    }

    /// The target architecture, if qld knows `e_machine`.
    #[must_use]
    pub fn architecture(&self) -> Option<Architecture> {
        self.elf.architecture()
    }

    /// `DT_SONAME`, if present.
    #[must_use]
    pub fn soname(&self) -> Option<&'a [u8]> {
        self.soname
    }

    /// Whether a `.gnu.hash` section is present.
    #[must_use]
    pub fn has_gnu_hash(&self) -> bool {
        self.gnu_hash
    }

    /// Whether a System V `.hash` section is present.
    #[must_use]
    pub fn has_sysv_hash(&self) -> bool {
        self.sysv_hash
    }

    /// The dynamic string table.
    #[must_use]
    pub fn dynstr(&self) -> StringTable<'a> {
        self.dynstr
    }

    /// Iterates over the dynamic entries up to (not including) `DT_NULL`.
    pub fn dynamic_entries(&self) -> impl Iterator<Item = DynEntry> + use<'a, F> {
        self.dynamic
            .iter()
            .map(F::decode_dyn)
            .take_while(|e| e.tag != DT_NULL)
    }

    /// Reads a string from the dynamic string table, for a `DT_NEEDED`-style
    /// value.
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` if the offset is invalid.
    pub fn dynamic_string(&self, offset: u64) -> Result<&'a [u8]> {
        u32::try_from(offset)
            .ok()
            .and_then(|o| self.dynstr.get(o))
            .ok_or_else(|| {
                self.source().malformed(
                    self.dynamic_offset,
                    format!("dynamic string table offset {offset:#x}"),
                )
            })
    }

    /// Iterates over the `DT_NEEDED` library names, in order.
    pub fn needed(&self) -> impl Iterator<Item = Result<&'a [u8]>> + '_ {
        self.dynamic_entries()
            .filter(|e| e.tag == DT_NEEDED)
            .map(|e| self.dynamic_string(e.value))
    }

    /// The dynamic symbol table (empty without `.dynsym`).
    #[must_use]
    pub fn symbols(&self) -> &SymbolTable<'a, F> {
        &self.symbols
    }

    /// Number of dynamic symbols, including the null symbol.
    #[must_use]
    pub fn symbol_count(&self) -> usize {
        self.symbols.len()
    }

    /// The version table, indexed by version index (entries 0 and 1 and any
    /// unused index are `None`).
    #[must_use]
    pub fn versions(&self) -> &[Option<VersionInfo<'a>>] {
        &self.versions
    }

    /// Returns the version of dynamic symbol `index`.
    ///
    /// Without a `.gnu.version` section every symbol is global (index 1).
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` if the version index is not defined by
    /// `.gnu.version_d` or `.gnu.version_r`.
    pub fn symbol_version(&self, index: usize) -> Result<SymbolVersion<'a>> {
        let Some(raw) = self.versym.get(index) else {
            return Ok(SymbolVersion {
                index: VER_NDX_GLOBAL,
                hidden: false,
                info: None,
            });
        };
        let raw = F::Endian::u16(*raw);
        let version = raw & VERSYM_VERSION;
        let info = if version <= VER_NDX_GLOBAL {
            None
        } else {
            let entry = self
                .versions
                .get(usize::from(version))
                .copied()
                .flatten()
                .ok_or_else(|| {
                    self.source().malformed(
                        self.symbols.strtab().file_offset(),
                        format!("symbol version index {version} (symbol {index})"),
                    )
                })?;
            Some(entry)
        };
        Ok(SymbolVersion {
            index: version,
            hidden: raw & VERSYM_HIDDEN != 0,
            info,
        })
    }

    /// Iterates over the dynamic symbols with their versions.
    pub fn dynamic_symbols(&self) -> impl ExactSizeIterator<Item = Result<DynamicSymbol<'a>>> + '_ {
        self.symbols.iter().enumerate().map(|(index, symbol)| {
            Ok(DynamicSymbol {
                symbol: symbol?,
                version: self.symbol_version(index)?,
            })
        })
    }

    fn set_version(&mut self, index: u16, info: VersionInfo<'a>) {
        let index = usize::from(index & VERSYM_VERSION);
        if self.versions.len() <= index {
            self.versions.resize(index.saturating_add(1), None);
        }
        if let Some(slot) = self.versions.get_mut(index) {
            *slot = Some(info);
        }
    }

    fn version_strtab(&self, header: &SectionHeader) -> Result<StringTable<'a>> {
        let hdr = self.elf.section_header(header.sh_link).map_err(|_| {
            self.source()
                .malformed(header.sh_offset, "version section string table link")
        })?;
        Ok(StringTable::new(
            self.elf.section_data(&hdr)?,
            hdr.sh_offset,
        ))
    }

    fn parse_verdef(&mut self, header: &SectionHeader) -> Result<()> {
        type E<F> = <F as ElfFormat>::Endian;
        let data = self.elf.section_data(header)?;
        let strtab = self.version_strtab(header)?;
        let source = self.source();
        let err = |offset: usize, what: &str| {
            source.malformed(
                header.sh_offset.saturating_add(to_u64(offset)),
                what.to_owned(),
            )
        };
        // sh_info is the number of entries; follow vd_next, bounded by the
        // number of 20-byte entries that fit, so a cycle cannot loop forever.
        let limit = data.len() / 20;
        let count = usize::try_from(header.sh_info).unwrap_or(usize::MAX);
        let mut offset = 0usize;
        for _ in 0..count.min(limit) {
            let field = |at: usize| offset.checked_add(at);
            let read16 = |at| field(at).and_then(|p| read_u16::<E<F>>(data, p));
            let read32 = |at| field(at).and_then(|p| read_u32::<E<F>>(data, p));
            let (Some(flags), Some(ndx), Some(cnt), Some(aux), Some(next)) =
                (read16(2), read16(4), read16(6), read32(12), read32(16))
            else {
                return Err(err(offset, "version definition (truncated)"));
            };
            if cnt > 0 {
                let name = usize::try_from(aux)
                    .ok()
                    .and_then(|aux| offset.checked_add(aux))
                    .and_then(|p| read_u32::<E<F>>(data, p))
                    .and_then(|name| strtab.get(name))
                    .ok_or_else(|| err(offset, "version definition name"))?;
                self.set_version(
                    ndx,
                    VersionInfo {
                        name,
                        flags,
                        kind: VersionKind::Defined,
                    },
                );
            }
            if next == 0 {
                break;
            }
            offset = usize::try_from(next)
                .ok()
                .and_then(|n| offset.checked_add(n))
                .ok_or_else(|| err(offset, "version definition link"))?;
        }
        Ok(())
    }

    fn parse_verneed(&mut self, header: &SectionHeader) -> Result<()> {
        type E<F> = <F as ElfFormat>::Endian;
        let data = self.elf.section_data(header)?;
        let strtab = self.version_strtab(header)?;
        let source = self.source();
        let err = |offset: usize, what: &str| {
            source.malformed(
                header.sh_offset.saturating_add(to_u64(offset)),
                what.to_owned(),
            )
        };
        let limit = data.len() / 16;
        let count = usize::try_from(header.sh_info).unwrap_or(usize::MAX);
        let mut aux_budget = limit;
        let mut offset = 0usize;
        for _ in 0..count.min(limit) {
            let at = |o: usize, d: usize| o.checked_add(d);
            let (Some(cnt), Some(file), Some(aux), Some(next)) = (
                at(offset, 2).and_then(|p| read_u16::<E<F>>(data, p)),
                at(offset, 4).and_then(|p| read_u32::<E<F>>(data, p)),
                at(offset, 8).and_then(|p| read_u32::<E<F>>(data, p)),
                at(offset, 12).and_then(|p| read_u32::<E<F>>(data, p)),
            ) else {
                return Err(err(offset, "version requirement (truncated)"));
            };
            let file = strtab
                .get(file)
                .ok_or_else(|| err(offset, "version requirement file name"))?;
            let mut aux_offset = usize::try_from(aux)
                .ok()
                .and_then(|a| offset.checked_add(a))
                .ok_or_else(|| err(offset, "version requirement link"))?;
            for _ in 0..cnt {
                aux_budget = aux_budget
                    .checked_sub(1)
                    .ok_or_else(|| err(aux_offset, "version requirement (too many entries)"))?;
                let (Some(flags), Some(other), Some(name), Some(next_aux)) = (
                    at(aux_offset, 4).and_then(|p| read_u16::<E<F>>(data, p)),
                    at(aux_offset, 6).and_then(|p| read_u16::<E<F>>(data, p)),
                    at(aux_offset, 8).and_then(|p| read_u32::<E<F>>(data, p)),
                    at(aux_offset, 12).and_then(|p| read_u32::<E<F>>(data, p)),
                ) else {
                    return Err(err(aux_offset, "version requirement entry (truncated)"));
                };
                let name = strtab
                    .get(name)
                    .ok_or_else(|| err(aux_offset, "version requirement name"))?;
                self.set_version(
                    other,
                    VersionInfo {
                        name,
                        flags,
                        kind: VersionKind::Needed { file },
                    },
                );
                if next_aux == 0 {
                    break;
                }
                aux_offset = usize::try_from(next_aux)
                    .ok()
                    .and_then(|n| aux_offset.checked_add(n))
                    .ok_or_else(|| err(aux_offset, "version requirement entry link"))?;
            }
            if next == 0 {
                break;
            }
            offset = usize::try_from(next)
                .ok()
                .and_then(|n| offset.checked_add(n))
                .ok_or_else(|| err(offset, "version requirement link"))?;
        }
        Ok(())
    }
}

/// Finds the dynamic string table through `DT_STRTAB`/`DT_STRSZ` and the
/// `PT_LOAD` segments, for files without section headers.
fn strtab_from_dynamic<'a, F: ElfFormat>(
    elf: &ElfFile<'a, F>,
    dynamic: &'a [F::Dyn],
    dynamic_offset: u64,
) -> Result<StringTable<'a>> {
    let mut addr = None;
    let mut size = None;
    for entry in dynamic.iter().map(F::decode_dyn) {
        match entry.tag {
            DT_NULL => break,
            DT_STRTAB => addr = Some(entry.value),
            DT_STRSZ => size = Some(entry.value),
            _ => {}
        }
    }
    let (Some(addr), Some(size)) = (addr, size) else {
        return Ok(StringTable::default());
    };
    let offset = elf
        .segments()
        .vaddr_to_offset(addr)
        .ok_or_else(|| elf.source().malformed(dynamic_offset, "DT_STRTAB address"))?;
    let data = subslice(elf.data(), offset, size).ok_or_else(|| {
        elf.source()
            .malformed(offset, "dynamic string table (out of bounds)")
    })?;
    Ok(StringTable::new(data, offset))
}
